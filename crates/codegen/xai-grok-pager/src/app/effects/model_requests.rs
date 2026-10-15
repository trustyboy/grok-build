//! 模型扩展请求的协议编码与严格响应校验。

use super::helpers::{SessionRpcError, format_acp_error};
use super::{acp_send_bounded, sanitize_user_error};
use crate::app::actions::{FastModeAction, FastModeState, ModelRequestContext, SupportVisionState};
use agent_client_protocol as acp;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use xai_acp_lib::AcpAgentTx;
use xai_grok_shell::session::ExtMethodResult;
use xai_grok_shell::session::subagents_model::{SubagentsModelAction, SubagentsModelState};

/// 发现并切换接口的成功载荷。
#[derive(Serialize, Deserialize)]
struct DiscoveredModels {
    /// 服务端确认的模型目录和当前选择。
    models: acp::SessionModelState,
}

/// 构造会话绑定请求；查询视觉状态时不发送 enabled。
fn request_params(request: &ModelRequestContext, enabled: Option<bool>) -> serde_json::Value {
    let mut params = serde_json::json!({
        "sessionId": request.session_id,
        "expectedModelId": request.expected_model_id,
    });
    if let Some(enabled) = enabled {
        params["enabled"] = enabled.into();
    }
    params
}

/// 错误信封不能被当作部分成功处理。
fn parse_result<T: Serialize + DeserializeOwned>(body: &str) -> Result<T, String> {
    let envelope: ExtMethodResult<T> =
        serde_json::from_str(body).map_err(|_| "服务端返回了无效的模型设置响应。".to_string())?;
    if let Some(error) = envelope.error {
        let message = error
            .as_str()
            .or_else(|| error.get("message").and_then(serde_json::Value::as_str))
            .map(str::to_owned)
            .unwrap_or_else(|| error.to_string());
        return Err(sanitize_user_error(&message));
    }
    envelope
        .result
        .ok_or_else(|| "服务端响应缺少 result。".into())
}

/// 发送扩展请求并保留服务端错误原因。
async fn send<T: Serialize + DeserializeOwned>(
    method: &'static str,
    params: serde_json::Value,
    tx: &AcpAgentTx,
) -> Result<T, String> {
    let req = acp::ExtRequest::new(
        method,
        serde_json::value::to_raw_value(&params)
            .expect("model request params")
            .into(),
    );
    let response = acp_send_bounded(req, tx, method).await.map_err(|error| {
        let message = match error {
            SessionRpcError::Rpc(error) => format_acp_error(&error, false),
            timeout @ SessionRpcError::TimedOut { .. } => timeout.to_string(),
        };
        sanitize_user_error(&message)
    })?;
    parse_result(response.0.get())
}

/// 发现失败不回退到普通模型切换。
pub(super) async fn discover_and_switch(
    request: &ModelRequestContext,
    model_id: &acp::ModelId,
    tx: &AcpAgentTx,
) -> Result<acp::SessionModelState, String> {
    let mut params = request_params(request, None);
    params["modelId"] = serde_json::to_value(model_id).expect("model id");
    let result: DiscoveredModels = send("x.ai/models/discover_and_switch", params, tx).await?;
    Ok(result.models)
}

/// 无 enabled 时为只读查询。
pub(super) async fn support_vision(
    request: &ModelRequestContext,
    enabled: Option<bool>,
    tx: &AcpAgentTx,
) -> Result<SupportVisionState, String> {
    send(
        "x.ai/session/support_vision",
        request_params(request, enabled),
        tx,
    )
    .await
}

/// 翻转由服务端处理；查询也绑定发起时的会话和模型。
pub(super) async fn fast_mode(
    request: &ModelRequestContext,
    action: FastModeAction,
    tx: &AcpAgentTx,
) -> Result<FastModeState, String> {
    let mut params = request_params(request, None);
    params["action"] = serde_json::to_value(action).expect("fast mode action");
    send("x.ai/session/fast_mode", params, tx).await
}

/// 共享操作类型展平到请求；未知 ID 也只走此接口。
pub(super) async fn subagents_model(
    request: &ModelRequestContext,
    action: SubagentsModelAction,
    tx: &AcpAgentTx,
) -> Result<SubagentsModelState, String> {
    let mut params = serde_json::to_value(action).expect("subagents model action");
    params["sessionId"] = serde_json::to_value(&request.session_id).expect("session id");
    params["expectedModelId"] = serde_json::to_value(&request.expected_model_id).expect("model id");
    send("x.ai/session/subagent_model", params, tx).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 查询省略 enabled，修改保留 false，模型与会话使用 camelCase。
    #[test]
    fn model_request_wire_params() {
        let request = ModelRequestContext {
            agent_id: crate::app::agent::AgentId(0),
            session_id: acp::SessionId::new("s"),
            expected_model_id: acp::ModelId::new("m"),
            token: std::sync::Arc::new(()),
        };
        assert_eq!(
            request_params(&request, None),
            serde_json::json!({
                "sessionId": "s", "expectedModelId": "m"
            })
        );
        assert_eq!(request_params(&request, Some(false))["enabled"], false);
        assert_eq!(request_params(&request, Some(true))["enabled"], true);
    }

    /// 模拟 ACP 通道，验证接口名、精确 ID、查询省略值及错误不透传。
    #[tokio::test]
    async fn model_request_mock_wire_roundtrip() {
        use std::sync::Arc;
        use xai_acp_lib::AcpAgentMessage;
        let request = ModelRequestContext {
            agent_id: crate::app::agent::AgentId(0),
            session_id: acp::SessionId::new("s"),
            expected_model_id: acp::ModelId::new("m"),
            token: Arc::new(()),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            for enabled in [None, Some(false), Some(true)] {
                let Some(AcpAgentMessage::ExtMethod(args)) = rx.recv().await else {
                    panic!("expected ext request")
                };
                assert_eq!(args.request.method.as_ref(), "x.ai/session/support_vision");
                let params: serde_json::Value =
                    serde_json::from_str(args.request.params.get()).unwrap();
                assert_eq!(params["sessionId"], "s");
                assert_eq!(params["expectedModelId"], "m");
                assert_eq!(
                    params.get("enabled").and_then(serde_json::Value::as_bool),
                    enabled
                );
                let response = serde_json::json!({"result": {"modelId": "m", "enabled": enabled.unwrap_or(true)}});
                args.response_tx
                    .send(Ok(acp::ExtResponse::new(Arc::from(
                        serde_json::value::to_raw_value(&response).unwrap(),
                    ))))
                    .unwrap();
            }
            let Some(AcpAgentMessage::ExtMethod(args)) = rx.recv().await else {
                panic!("expected discovery")
            };
            assert_eq!(
                args.request.method.as_ref(),
                "x.ai/models/discover_and_switch"
            );
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(args.request.params.get()).unwrap(),
                serde_json::json!({"sessionId":"s", "expectedModelId":"m", "modelId":"Vendor/Exact-ID"})
            );
            let response = serde_json::json!({"result": null, "error": "not found"});
            args.response_tx
                .send(Ok(acp::ExtResponse::new(Arc::from(
                    serde_json::value::to_raw_value(&response).unwrap(),
                ))))
                .unwrap();
            assert!(
                rx.recv().await.is_none(),
                "failure must not send a fallback request"
            );
        });
        for enabled in [None, Some(false), Some(true)] {
            assert_eq!(
                support_vision(&request, enabled, &tx)
                    .await
                    .unwrap()
                    .enabled,
                enabled.unwrap_or(true)
            );
        }
        assert_eq!(
            discover_and_switch(&request, &acp::ModelId::new("Vendor/Exact-ID"), &tx)
                .await
                .unwrap_err(),
            "not found"
        );
        drop(tx);
        server.await.unwrap();
    }

    /// 模拟 ACP，覆盖全部操作、非 Responses 查询和后端拒绝修改。
    #[tokio::test]
    async fn fast_mode_mock_wire_roundtrip() {
        use std::sync::Arc;
        use xai_acp_lib::AcpAgentMessage;
        let request = ModelRequestContext {
            agent_id: crate::app::agent::AgentId(0),
            session_id: acp::SessionId::new("s"),
            expected_model_id: acp::ModelId::new("m"),
            token: Arc::new(()),
        };
        let cases = [
            (FastModeAction::Toggle, "toggle", true, None),
            (FastModeAction::On, "on", true, None),
            (FastModeAction::Off, "off", true, None),
            (FastModeAction::Status, "status", true, None),
            (FastModeAction::Status, "status", false, None),
            (
                FastModeAction::Toggle,
                "toggle",
                false,
                Some("仅 Responses 支持"),
            ),
            (FastModeAction::On, "on", false, Some("仅 Responses 支持")),
            (FastModeAction::Off, "off", false, Some("仅 Responses 支持")),
        ];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            for (action, wire, supported, error) in cases {
                let Some(AcpAgentMessage::ExtMethod(args)) = rx.recv().await else {
                    panic!("expected fast mode request")
                };
                assert_eq!(args.request.method.as_ref(), "x.ai/session/fast_mode");
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(args.request.params.get()).unwrap(),
                    serde_json::json!({"sessionId": "s", "expectedModelId": "m", "action": wire})
                );
                let response = if let Some(error) = error {
                    serde_json::json!({"error": {"message": error}})
                } else {
                    serde_json::json!({"result": {
                        "modelId": "m", "enabled": supported && action != FastModeAction::Off,
                        "supported": supported
                    }})
                };
                args.response_tx
                    .send(Ok(acp::ExtResponse::new(Arc::from(
                        serde_json::value::to_raw_value(&response).unwrap(),
                    ))))
                    .unwrap();
            }
            assert!(rx.recv().await.is_none(), "must not send fallback requests");
        });
        for (action, _, supported, error) in cases {
            let result = fast_mode(&request, action, &tx).await;
            if let Some(error) = error {
                assert_eq!(result.unwrap_err(), error);
            } else {
                let state = result.unwrap();
                assert_eq!(state.model_id, "m");
                assert_eq!(state.supported, supported);
                assert_eq!(state.enabled, supported && action != FastModeAction::Off);
            }
        }
        drop(tx);
        server.await.unwrap();
    }

    /// Set 和控制操作共用接口，精确 ID、可选参数及来源状态无损传递。
    #[tokio::test]
    async fn subagents_model_mock_wire_roundtrip() {
        use std::num::NonZeroU64;
        use std::sync::Arc;
        use xai_acp_lib::AcpAgentMessage;
        use xai_grok_shell::sampling::types::ReasoningEffort;
        let request = ModelRequestContext {
            agent_id: crate::app::agent::AgentId(0),
            session_id: acp::SessionId::new("s"),
            expected_model_id: acp::ModelId::new("main"),
            token: Arc::new(()),
        };
        let cases = [
            SubagentsModelAction::Status,
            SubagentsModelAction::Inherit,
            SubagentsModelAction::Reset,
            SubagentsModelAction::Set {
                model_id: "known".into(),
                reasoning_effort: None,
                context_window: None,
            },
            SubagentsModelAction::Set {
                model_id: "known".into(),
                reasoning_effort: Some(ReasoningEffort::High),
                context_window: NonZeroU64::new(256000),
            },
            SubagentsModelAction::Set {
                model_id: "Vendor/Exact-ID".into(),
                reasoning_effort: None,
                context_window: None,
            },
        ];
        let server_cases = cases.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            for (index, action) in server_cases.iter().enumerate() {
                let Some(AcpAgentMessage::ExtMethod(args)) = rx.recv().await else {
                    panic!("expected subagents model request");
                };
                assert_eq!(args.request.method.as_ref(), "x.ai/session/subagent_model");
                let mut params: serde_json::Value =
                    serde_json::from_str(args.request.params.get()).unwrap();
                let object = params.as_object_mut().unwrap();
                assert_eq!(object.remove("sessionId").unwrap(), "s");
                assert_eq!(object.remove("expectedModelId").unwrap(), "main");
                assert!(params["action"].is_string());
                assert_eq!(
                    serde_json::from_value::<SubagentsModelAction>(params.clone()).unwrap(),
                    *action
                );
                if index == 4 {
                    assert_eq!(params["modelId"], "known");
                    assert_eq!(params["reasoningEffort"], "high");
                    assert_eq!(params["contextWindow"], 256000);
                }
                if index == 5 {
                    assert_eq!(params["modelId"], "Vendor/Exact-ID");
                    assert!(params["reasoningEffort"].is_null());
                    assert!(params["contextWindow"].is_null());
                }
                let response = if index == 5 {
                    serde_json::json!({"error":{"message":"not found"}})
                } else {
                    serde_json::json!({"result": {
                        "modelId":"child", "modelSource":"session", "reasoningEffort":"high",
                        "effortSource":"model_default", "contextWindow":256000,
                        "configuredModel":"configured", "configuredEffort":"low", "warning":"提示",
                        "availableModels":[{"modelId":"child", "name":"Child"}]
                    }})
                };
                args.response_tx
                    .send(Ok(acp::ExtResponse::new(Arc::from(
                        serde_json::value::to_raw_value(&response).unwrap(),
                    ))))
                    .unwrap();
            }
            assert!(
                rx.recv().await.is_none(),
                "must not switch main model or fall back"
            );
        });
        for (index, action) in cases.into_iter().enumerate() {
            let result = subagents_model(&request, action, &tx).await;
            if index == 5 {
                assert_eq!(result.unwrap_err(), "not found");
            } else {
                let state = result.unwrap();
                assert_eq!(state.model_id, "child");
                assert_eq!(state.model_source, "session");
                assert_eq!(state.reasoning_effort, Some(ReasoningEffort::High));
                assert_eq!(state.effort_source, "model_default");
                assert_eq!(state.context_window, 256000);
                assert_eq!(state.configured_model.as_deref(), Some("configured"));
                assert_eq!(state.configured_effort.as_deref(), Some("low"));
                assert_eq!(state.warning.as_deref(), Some("提示"));
                assert_eq!(state.available_models.len(), 1);
            }
        }
        drop(tx);
        server.await.unwrap();
    }

    /// 缺少有效状态或包含错误的信封不能提交。
    #[test]
    fn subagents_model_rejects_invalid_envelopes() {
        for body in [
            "{}",
            "not json",
            r#"{"result":{}}"#,
            r#"{"error":{"message":"denied"}}"#,
            r#"{"result":{"modelId":"child","modelSource":"session","effortSource":"default","contextWindow":256000,"availableModels":[]},"error":"denied"}"#,
        ] {
            assert!(parse_result::<SubagentsModelState>(body).is_err(), "{body}");
        }
    }

    /// Fast 状态必须包含两个布尔值；错误信封不能伪装成功。
    #[test]
    fn fast_mode_envelopes() {
        let state: FastModeState =
            parse_result(r#"{"result":{"modelId":"m","enabled":false,"supported":false}}"#)
                .unwrap();
        assert!(!state.enabled);
        assert!(!state.supported);
        for body in [
            r#"{"modelId":"m","enabled":true,"supported":true}"#,
            r#"{"result":{"enabled":true,"supported":true}}"#,
            r#"{"result":{"modelId":"m","supported":true}}"#,
            r#"{"result":{"modelId":"m","enabled":true}}"#,
            r#"{"result":{"modelId":"m","enabled":"true","supported":true}}"#,
            r#"{"result":{"modelId":"m","enabled":true,"supported":null}}"#,
            r#"{"result":{"modelId":"m","enabled":true,"supported":true},"error":"denied"}"#,
        ] {
            assert!(parse_result::<FastModeState>(body).is_err(), "{body}");
        }
    }

    /// 成功载荷必须位于 result，缺字段或错误信封均失败。
    #[test]
    fn model_request_envelopes() {
        let state: SupportVisionState =
            parse_result(r#"{"result":{"modelId":"m","enabled":false}}"#).unwrap();
        assert_eq!(state.model_id, "m");
        assert!(!state.enabled);
        for body in [
            r#"{"modelId":"m","enabled":true}"#,
            "{}",
            "not json",
            r#"{"result":{"modelId":"m"}}"#,
            r#"{"result":{"modelId":"m","enabled":true},"error":"denied"}"#,
        ] {
            assert!(parse_result::<SupportVisionState>(body).is_err());
        }
        assert_eq!(
            parse_result::<SupportVisionState>(
                r#"{"error":{"code":"denied","message":"blocked"}}"#
            )
            .unwrap_err(),
            "blocked"
        );
        assert_eq!(
            parse_result::<SupportVisionState>(r#"{"error":"not found"}"#).unwrap_err(),
            "not found"
        );
        let models: DiscoveredModels = parse_result(r#"{"result":{"models":{"currentModelId":"m","availableModels":[{"modelId":"m","name":"m","_meta":{"runtimeDiscovered":true}}]}}}"#).unwrap();
        assert_eq!(models.models.current_model_id.0.as_ref(), "m");
        assert!(parse_result::<DiscoveredModels>(r#"{"result":{}}"#).is_err());
    }
}
