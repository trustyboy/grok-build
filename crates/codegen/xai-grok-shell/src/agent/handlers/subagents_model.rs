//! 当前父会话的子代理模型选择，复用目录发现并保持主模型选择。

use agent_client_protocol as acp;
use serde::Deserialize;

use crate::agent::mvp_agent::MvpAgent;
use crate::agent::remote_config::resolve_catalog_key;
use crate::extensions::ExtResult;
use crate::session::ExtMethodResult;
use crate::session::subagents_model::{SubagentsModelAction, SubagentsModelSelection};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubagentsModelRequest {
    /// 操作所属的父会话。
    session_id: String,
    /// 发现期间校验主会话是否切换了模型。
    expected_model_id: Option<String>,
    /// 平铺操作参数，与终端命令使用同一协议。
    #[serde(flatten)]
    action: SubagentsModelAction,
}

/// 校验参数后一次提交会话选择，失败保留先前值。
pub(crate) async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let request: SubagentsModelRequest = serde_json::from_str(args.params.get())
        .map_err(|e| acp::Error::invalid_params().data(format!("子代理模型参数无效：{e}")))?;
    let sid = acp::SessionId::new(request.session_id);
    agent
        .session_handle_waiting_for_load(&sid)
        .await
        .ok_or_else(|| acp::Error::invalid_params().data("当前会话不存在"))?;
    // 网络发现也占用此会话的配置锁，防止较早的请求覆盖后来的选择。
    let _guard = agent.config_mutation_lock(&sid).lock_owned().await;
    let handle = agent
        .resident_handle(&sid)
        .ok_or_else(|| acp::Error::invalid_params().data("当前会话已关闭"))?;
    if request
        .expected_model_id
        .as_deref()
        .is_some_and(|id| id != handle.model_id.0.as_ref())
    {
        return Err(acp::Error::invalid_params().data("当前模型已改变，请重新执行命令"));
    }
    let mut discovered = None;
    let selection = match request.action {
        SubagentsModelAction::Status => None,
        SubagentsModelAction::Inherit => Some(SubagentsModelSelection::Inherit),
        SubagentsModelAction::Reset => Some(SubagentsModelSelection::Default),
        SubagentsModelAction::Set {
            model_id,
            reasoning_effort,
            context_window,
        } => {
            let catalog = agent.models_manager.models();
            let target = acp::ModelId::new(model_id.clone());
            let key = if let Some(key) = resolve_catalog_key(&catalog, &target) {
                key
            } else {
                // 未发现模型的能力未知，先发现裸 ID 后再设置窗口和推理参数。
                if reasoning_effort.is_some() || context_window.is_some() {
                    return Err(acp::Error::invalid_params()
                        .data("请先使用完整模型 ID 发现模型，再设置其支持的窗口和推理强度"));
                }
                let found = super::model_discovery::discover_model(
                    agent,
                    &sid,
                    &model_id,
                    handle.model_id.0.as_ref(),
                )
                .await?;
                let key = found.model_id.clone();
                discovered = Some(found);
                key
            };
            if !agent.models_manager.available().contains_key(&key) {
                return Err(acp::Error::invalid_params().data("目标模型不可选，请检查模型过滤规则"));
            }
            // 重新解析临时模型来源，使用模型本身的认证及能力。
            agent.resolve_model_id(&key)?;
            if let Some(effort) = reasoning_effort {
                if !agent
                    .models_manager
                    .model_supports_reasoning_effort_value(key.0.as_ref(), effort)
                {
                    return Err(acp::Error::invalid_params()
                        .data(format!("模型 {} 不支持推理强度 {effort}", key.0)));
                }
            }
            if let Some(window) = context_window {
                if !agent
                    .models_manager
                    .model_supports_context_window(key.0.as_ref(), window)
                {
                    return Err(acp::Error::invalid_params()
                        .data(format!("模型 {} 不支持上下文窗口 {window}", key.0)));
                }
            }
            Some(SubagentsModelSelection::Model {
                model_id: key.0.to_string(),
                reasoning_effort,
                context_window,
            })
        }
    };
    if let Some(found) = &discovered {
        found.validate(agent)?;
    }
    let current = agent
        .resident_handle(&sid)
        .ok_or_else(|| acp::Error::invalid_params().data("查询期间会话已关闭"))?;
    if !std::sync::Arc::ptr_eq(&handle.subagents_model, &current.subagents_model)
        || current.model_id != handle.model_id
    {
        return Err(acp::Error::invalid_params().data("查询期间会话或模型已改变，请重试"));
    }
    let mut ctx = agent
        .try_build_subagent_spawn_context(sid.0.as_ref())
        .ok_or_else(|| acp::Error::invalid_params().data("当前会话不可用"))?;
    if let Some(selection) = selection {
        *current.subagents_model.lock() = selection.clone();
        ctx.subagents_model = selection;
    }
    let mut state = crate::agent::subagent::subagents_model_state(&ctx).await;
    state.available_models = agent.models_manager.available().into_values().collect();
    if discovered.is_some() {
        // 其他界面只接收目录更新，主模型仍由各自会话持有。
        if let Ok(params) = serde_json::value::to_raw_value(&agent.model_state(None)) {
            agent
                .gateway
                .forward_fire_and_forget(acp::ExtNotification::new(
                    "x.ai/models/update",
                    params.into(),
                ));
        }
    }
    ExtMethodResult::success(state)
        .to_ext_response()
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 协议仅接受明确操作，额外参数不得变成隐式修改。
    #[test]
    fn subagents_model_request_contract() {
        for action in ["status", "inherit", "reset"] {
            let request: SubagentsModelRequest = serde_json::from_value(serde_json::json!({
                "sessionId":"s", "action":action,
            }))
            .unwrap();
            assert_eq!(request.session_id, "s");
        }
        let request: SubagentsModelRequest = serde_json::from_value(serde_json::json!({
            "sessionId":"s", "expectedModelId":"parent", "action":"set", "modelId":"child",
            "reasoningEffort":"high", "contextWindow":256000,
        }))
        .unwrap();
        assert!(matches!(
            request.action,
            SubagentsModelAction::Set {
                reasoning_effort: Some(_),
                context_window: Some(_),
                ..
            }
        ));
        for value in [
            serde_json::json!({"action":"status"}),
            serde_json::json!({"sessionId":"s", "action":"set"}),
            serde_json::json!({"sessionId":"s", "action":"invalid"}),
            serde_json::json!({"sessionId":"s", "action":"reset", "modelId":"c"}),
            serde_json::json!({"sessionId":"s", "action":"inherit", "contextWindow":256000}),
            serde_json::json!({"sessionId":"s", "action":"status", "unexpected":true}),
            serde_json::json!({"sessionId":"s", "action":"set", "modelId":"c", "contextWindow":0}),
            serde_json::json!({"sessionId":"s", "action":"set", "modelId":"c", "reasoningEffort":"invalid"}),
        ] {
            assert!(serde_json::from_value::<SubagentsModelRequest>(value).is_err());
        }
    }
}
