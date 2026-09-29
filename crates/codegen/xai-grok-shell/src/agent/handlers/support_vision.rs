//! 当前会话、当前模型的视觉查询与覆盖，不写入配置。

use agent_client_protocol as acp;
use serde::Deserialize;

use crate::agent::mvp_agent::MvpAgent;
use crate::session::{ExtMethodResult, SessionCommand};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SupportVisionRequest {
    /// 目标会话。
    session_id: String,
    /// 省略时只查询。
    enabled: Option<bool>,
    /// 防止界面旧状态修改了另一个模型。
    expected_model_id: Option<String>,
}

/// 等待 actor 确认后返回下一轮视觉状态。
pub(crate) async fn handle(
    agent: &MvpAgent,
    args: &acp::ExtRequest,
) -> Result<acp::ExtResponse, acp::Error> {
    let request: SupportVisionRequest = serde_json::from_str(args.params.get())
        .map_err(|e| acp::Error::invalid_params().data(format!("invalid params: {e}")))?;
    let session_id = acp::SessionId::new(request.session_id);
    let session = agent
        .session_handle_waiting_for_load(&session_id)
        .await
        .ok_or_else(|| acp::Error::invalid_params().data("unknown session id"))?;
    let _guard = agent.config_mutation_lock(&session_id).lock_owned().await;
    let session = agent.resident_handle(&session_id).unwrap_or(session);
    let (tx, rx) = tokio::sync::oneshot::channel();
    session
        .cmd_tx
        .send(SessionCommand::SupportVision {
            enabled: request.enabled,
            expected_model_id: request.expected_model_id,
            responds_to: tx,
        })
        .map_err(|_| acp::Error::internal_error().data("session actor unavailable"))?;
    let mut state = rx.await.map_err(|_| {
        acp::Error::internal_error().data("session actor closed before acknowledgement")
    })??;
    // 客户端以目录键识别模型，actor 内部的 wire slug 可能与它不同。
    state.model_id = session.model_id.0.to_string();
    ExtMethodResult::success(state)
        .to_ext_response()
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SupportVisionState;

    #[test]
    fn support_vision_request_and_response_contract() {
        let query: SupportVisionRequest =
            serde_json::from_value(serde_json::json!({"sessionId": "s"})).unwrap();
        assert!(query.enabled.is_none());
        assert!(query.expected_model_id.is_none());
        for enabled in [true, false] {
            let request: SupportVisionRequest = serde_json::from_value(serde_json::json!({
                "sessionId": "s", "enabled": enabled, "expectedModelId": "model"
            }))
            .unwrap();
            assert_eq!(request.enabled, Some(enabled));
            assert_eq!(request.expected_model_id.as_deref(), Some("model"));
            let response = serde_json::to_value(ExtMethodResult::success(SupportVisionState {
                model_id: "model".to_owned(),
                enabled,
            }))
            .unwrap();
            assert_eq!(
                response,
                serde_json::json!({
                    "result": {"modelId": "model", "enabled": enabled}
                })
            );
        }
    }

    #[test]
    fn support_vision_rejects_invalid_parameters() {
        for value in [
            serde_json::json!({"enabled": true}),
            serde_json::json!({"sessionId": "s", "enabled": "on"}),
            serde_json::json!({"sessionId": "s", "enabled": 1}),
            serde_json::json!({"sessionId": "s", "expectedModelId": false}),
        ] {
            assert!(serde_json::from_value::<SupportVisionRequest>(value).is_err());
        }
    }
}
