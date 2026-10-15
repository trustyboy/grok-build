//! 当前会话、当前模型的 Fast 查询与覆盖。

use agent_client_protocol as acp;
use serde::Deserialize;

use crate::agent::mvp_agent::MvpAgent;
use crate::session::{ExtMethodResult, FastModeAction, SessionCommand};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FastModeRequest {
    /// 目标会话。
    session_id: String,
    /// 省略时只查询，修改必须显式声明操作。
    #[serde(default)]
    action: FastModeAction,
    /// 防止界面旧状态修改了另一个模型。
    expected_model_id: Option<String>,
}

/// 等待 actor 确认后返回下一轮 Fast 状态。
pub(crate) async fn handle(
    agent: &MvpAgent,
    args: &acp::ExtRequest,
) -> Result<acp::ExtResponse, acp::Error> {
    let request: FastModeRequest = serde_json::from_str(args.params.get())
        .map_err(|e| acp::Error::invalid_params().data(format!("Fast 参数无效：{e}")))?;
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
        .send(SessionCommand::FastMode {
            action: request.action,
            expected_model_id: request.expected_model_id,
            responds_to: tx,
        })
        .map_err(|_| acp::Error::internal_error().data("session actor unavailable"))?;
    let mut state = rx.await.map_err(|_| {
        acp::Error::internal_error().data("session actor closed before acknowledgement")
    })??;
    // 客户端以目录键识别模型，actor 内部可能使用路由 slug。
    state.model_id = session.model_id.0.to_string();
    ExtMethodResult::success(state)
        .to_ext_response()
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_mode_request_validates_actions() {
        for action in ["toggle", "on", "off", "status"] {
            let request: FastModeRequest = serde_json::from_value(serde_json::json!({
                "sessionId": "s", "action": action, "expectedModelId": "model"
            }))
            .unwrap();
            assert_eq!(request.expected_model_id.as_deref(), Some("model"));
        }
        let query: FastModeRequest =
            serde_json::from_value(serde_json::json!({"sessionId": "s"})).unwrap();
        assert!(matches!(query.action, FastModeAction::Status));
        for value in [
            serde_json::json!({"action": "on"}),
            serde_json::json!({"sessionId": "s", "action": "invalid"}),
            serde_json::json!({"sessionId": "s", "action": true}),
            serde_json::json!({"sessionId": "s", "enabled": true}),
        ] {
            assert!(serde_json::from_value::<FastModeRequest>(value).is_err());
        }
    }
}
