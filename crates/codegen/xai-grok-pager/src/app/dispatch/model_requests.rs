//! 模型设置请求的会话绑定、并发保护及确认后提交。

use super::{maybe_drain_queue_and_note_peek, refresh_open_settings_modals};
use crate::app::actions::{
    Effect, FastModeAction, FastModeState, ModelRequestContext, SupportVisionState,
};
use crate::app::app_view::{ActiveView, AppView};
use crate::scrollback::block::RenderBlock;
use agent_client_protocol as acp;
use std::sync::Arc;
use xai_grok_shell::session::subagents_model::{SubagentsModelAction, SubagentsModelState};

/// 复用模型切换屏障，阻止并发修改及提示词提前发送。
fn begin_request(app: &mut AppView) -> Option<ModelRequestContext> {
    let ActiveView::Agent(agent_id) = app.active_view else {
        app.show_toast("请先进入会话，再操作当前模型。");
        return None;
    };
    let agent = app.agents.get_mut(&agent_id)?;
    if agent.session.model_switch_pending {
        agent.show_toast("模型设置请求正在处理中，请完成后重试。");
        return None;
    }
    let Some(session_id) = agent.session.session_id.clone() else {
        agent.show_toast("请先进入已就绪的会话，再操作当前模型。");
        return None;
    };
    let Some(expected_model_id) = agent.session.models.current.clone() else {
        agent.show_toast("当前模型尚未就绪，请稍后重试。");
        return None;
    };
    let token = Arc::new(());
    agent.pending_model_request = Some(token.clone());
    agent.session.model_switch_pending = true;
    Some(ModelRequestContext {
        agent_id,
        session_id,
        expected_model_id,
        token,
    })
}

/// 发现请求只接受一个无空白的精确 ID。
pub(super) fn discover_and_switch(app: &mut AppView, model_id: acp::ModelId) -> Vec<Effect> {
    if model_id.0.is_empty() || model_id.0.chars().any(char::is_whitespace) {
        app.show_toast("模型发现需要单个无空白的精确模型 ID。");
        return vec![];
    }
    begin_request(app)
        .map(|request| vec![Effect::DiscoverAndSwitchModel { request, model_id }])
        .unwrap_or_default()
}

/// 查询和修改均等待服务端返回有效状态。
pub(super) fn support_vision(app: &mut AppView, enabled: Option<bool>) -> Vec<Effect> {
    begin_request(app)
        .map(|request| vec![Effect::SupportVision { request, enabled }])
        .unwrap_or_default()
}

/// 所有 Fast 操作复用模型设置屏障，不在前端推测协议支持。
pub(super) fn fast_mode(app: &mut AppView, action: FastModeAction) -> Vec<Effect> {
    begin_request(app)
        .map(|request| vec![Effect::FastMode { request, action }])
        .unwrap_or_default()
}

/// 子代理操作复用请求屏障，不缓存服务端选择。
pub(super) fn subagents_model(app: &mut AppView, action: SubagentsModelAction) -> Vec<Effect> {
    begin_request(app)
        .map(|request| vec![Effect::SubagentsModel { request, action }])
        .unwrap_or_default()
}

/// 仅持有同一请求标识的会话才能解除屏障。
fn finish_request(app: &mut AppView, request: &ModelRequestContext) -> bool {
    let Some(agent) = app.agents.get_mut(&request.agent_id) else {
        return false;
    };
    if agent.session.session_id.as_ref() != Some(&request.session_id)
        || !agent
            .pending_model_request
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, &request.token))
    {
        return false;
    }
    agent.pending_model_request = None;
    agent.session.model_switch_pending = false;
    agent.session.models.model_changed_during_switch = false;
    true
}

/// 成功且响应属于当前请求时才替换目录和当前模型。
pub(super) fn discover_complete(
    app: &mut AppView,
    request: ModelRequestContext,
    model_id: acp::ModelId,
    result: Result<acp::SessionModelState, String>,
) -> Vec<Effect> {
    if !finish_request(app, &request) {
        return vec![];
    }
    let agent = app
        .agents
        .get_mut(&request.agent_id)
        .expect("request agent");
    let result = result.and_then(|models| {
        if agent.session.models.current.as_ref() != Some(&request.expected_model_id) {
            return Err("当前模型已变化，已忽略过期的发现响应。".into());
        }
        if models.current_model_id != model_id
            || !models
                .available_models
                .iter()
                .any(|info| info.model_id == model_id)
        {
            return Err("服务端返回的模型目录与发现目标不一致。".into());
        }
        Ok(models)
    });
    match result {
        Ok(models) => {
            agent.session.models = Some(models).into();
            agent.refresh_context_total();
            agent.session.user_model_preference = Some(model_id.clone());
            app.models
                .update_catalog(agent.session.models.available.clone());
            agent.scrollback.push_block(RenderBlock::system(format!(
                "已切换到 {}（仅当前会话，不保存默认模型）",
                agent.session.models.display_name_for(&model_id)
            )));
        }
        Err(error) => {
            agent
                .scrollback
                .push_block(RenderBlock::system(format!("模型发现或切换失败：{error}")));
        }
    }
    refresh_open_settings_modals(app);
    maybe_drain_queue_and_note_peek(app, request.agent_id)
}

/// 目录可广播，确认文本只写入发起界面；主模型及会话默认值保持不变。
pub(super) fn subagents_model_complete(
    app: &mut AppView,
    request: ModelRequestContext,
    result: Result<SubagentsModelState, String>,
) -> Vec<Effect> {
    if !finish_request(app, &request) {
        return vec![];
    }
    let result = result.and_then(|state| {
        if app.agents[&request.agent_id]
            .session
            .models
            .current
            .as_ref()
            != Some(&request.expected_model_id)
        {
            return Err("当前模型已变化，已忽略过期的子代理模型响应。".into());
        }
        if state.model_id.is_empty() || state.context_window == 0 {
            return Err("服务端返回了无效的子代理模型状态。".into());
        }
        Ok(state)
    });
    let message = match result {
        Ok(state) => {
            let message = subagents_model_message(&state);
            let catalog: indexmap::IndexMap<_, _> = state
                .available_models
                .into_iter()
                .map(|info| (info.model_id.clone(), info))
                .collect();
            app.models.update_catalog(catalog.clone());
            for agent in app.agents.values_mut() {
                agent.session.models.update_catalog(catalog.clone());
                agent.refresh_context_total();
            }
            refresh_open_settings_modals(app);
            message
        }
        Err(error) => format!("子代理模型查询或设置失败：{error}"),
    };
    app.agents
        .get_mut(&request.agent_id)
        .expect("request agent")
        .scrollback
        .push_block(RenderBlock::system(message));
    maybe_drain_queue_and_note_peek(app, request.agent_id)
}

/// 协议来源标识转换为用户可读说明。
fn subagents_source_label(source: &str) -> &str {
    match source {
        "explicit" => "单次指定",
        "session" => "当前会话设置",
        "role" => "角色配置",
        "persona" => "persona 配置",
        "type_config" => "按类型配置",
        "parent_model_config" => "主模型专属配置",
        "config" => "子代理全局配置",
        "definition" => "代理定义",
        "parent" => "主会话当前值",
        "model_default" => "所选模型默认值",
        other => other,
    }
}

/// 展示服务端解析后的有效值和来源，不推测继承策略。
fn subagents_model_message(state: &SubagentsModelState) -> String {
    let effort = state
        .reasoning_effort
        .map(|effort| effort.to_string())
        .unwrap_or_else(|| "未指定".into());
    let mut message = format!(
        "默认子代理模型：{}（来源：{}）\n推理强度（effort）：{}（来源：{}）\n上下文窗口（window）：{} tokens\n以上为普通 general-purpose 子代理的默认选择，仅对新建子代理生效。单次显式指定与恢复子代理沿用各自规则。",
        state.model_id,
        subagents_source_label(&state.model_source),
        effort,
        subagents_source_label(&state.effort_source),
        state.context_window,
    );
    if let Some(model) = &state.parent_model_id {
        message.push_str(&format!("\n当前主模型：{model}"));
    }
    if let Some(model) = &state.parent_configured_model {
        message.push_str(&format!("\n主模型专属子代理配置：{model}"));
    }
    if let Some(model) = &state.configured_model {
        message.push_str(&format!("\n配置模型：{model}"));
    }
    if let Some(effort) = &state.configured_effort {
        message.push_str(&format!("\n配置推理强度：{effort}"));
    }
    if let Some(warning) = &state.warning {
        message.push_str(&format!("\n提示：{warning}"));
    }
    message
}

/// 只展示原会话和模型的确认状态，不缓存或持久化开关。
pub(super) fn fast_mode_complete(
    app: &mut AppView,
    request: ModelRequestContext,
    result: Result<FastModeState, String>,
) -> Vec<Effect> {
    if !finish_request(app, &request) {
        return vec![];
    }
    let agent = app
        .agents
        .get_mut(&request.agent_id)
        .expect("request agent");
    let result = result.and_then(|state| {
        if agent.session.models.current.as_ref() != Some(&request.expected_model_id)
            || state.model_id != request.expected_model_id.0.as_ref()
        {
            return Err("当前模型已变化，已忽略过期的快速模式响应。".into());
        }
        Ok(state)
    });
    let message = match result {
        Ok(state) if !state.supported => {
            format!("{} 不支持快速模式；仅 Responses API 支持。", state.model_id)
        }
        Ok(state) => format!(
            "{} 快速模式：{}（仅当前会话、当前模型；下一轮生效；不写入配置）{}",
            state.model_id,
            if state.enabled {
                "已开启"
            } else {
                "已关闭"
            },
            if state.enabled {
                "；费用更高，实际加速取决于上游服务支持。"
            } else {
                "；省略 service_tier，沿用上游默认。"
            }
        ),
        Err(error) => format!("快速模式查询或修改失败：{error}"),
    };
    agent.scrollback.push_block(RenderBlock::system(message));
    maybe_drain_queue_and_note_peek(app, request.agent_id)
}

/// 只展示服务端确认值，不修改共享模型能力或附件行为。
pub(super) fn support_vision_complete(
    app: &mut AppView,
    request: ModelRequestContext,
    result: Result<SupportVisionState, String>,
) -> Vec<Effect> {
    if !finish_request(app, &request) {
        return vec![];
    }
    let agent = app
        .agents
        .get_mut(&request.agent_id)
        .expect("request agent");
    let result = result.and_then(|state| {
        if agent.session.models.current.as_ref() != Some(&request.expected_model_id)
            || state.model_id != request.expected_model_id.0.as_ref()
        {
            return Err("当前模型已变化，已忽略过期的视觉状态响应。".into());
        }
        Ok(state)
    });
    let message = match result {
        Ok(state) => format!(
            "{} 原生视觉：{}（仅当前会话；不影响附件添加）",
            state.model_id,
            if state.enabled { "on" } else { "off" }
        ),
        Err(error) => format!("视觉状态查询或修改失败：{error}"),
    };
    agent.scrollback.push_block(RenderBlock::system(message));
    maybe_drain_queue_and_note_peek(app, request.agent_id)
}
