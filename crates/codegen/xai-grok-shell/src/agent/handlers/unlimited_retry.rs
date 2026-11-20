//! 当前会话的无限重试查询与覆盖。

use agent_client_protocol as acp;
use serde::Deserialize;

use crate::agent::mvp_agent::MvpAgent;
use crate::session::{ExtMethodResult, SessionCommand, UnlimitedRetryAction, UnlimitedRetryState};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UnlimitedRetryRequest {
    /// 目标会话。
    session_id: String,
    /// 省略时只查询，修改必须显式声明操作。
    #[serde(default)]
    action: UnlimitedRetryAction,
}

/// 等待 actor 确认后返回下一轮生效的无限重试状态。
pub(crate) async fn handle(
    agent: &MvpAgent,
    args: &acp::ExtRequest,
) -> Result<acp::ExtResponse, acp::Error> {
    let request: UnlimitedRetryRequest = serde_json::from_str(args.params.get())
        .map_err(|e| acp::Error::invalid_params().data(format!("无限重试参数无效：{e}")))?;
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
        .send(SessionCommand::UnlimitedRetry {
            action: request.action,
            responds_to: tx,
        })
        .map_err(|_| acp::Error::internal_error().data("session actor unavailable"))?;
    let state: UnlimitedRetryState = rx.await.map_err(|_| {
        acp::Error::internal_error().data("session actor closed before acknowledgement")
    })??;
    ExtMethodResult::success(state)
        .to_ext_response()
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_retry_request_validates_actions() {
        for action in ["toggle", "on", "off", "status"] {
            let request: UnlimitedRetryRequest =
                serde_json::from_value(serde_json::json!({
                    "sessionId": "s", "action": action
                }))
                .unwrap();
            assert!(matches!(
                request.action,
                UnlimitedRetryAction::Toggle
                    | UnlimitedRetryAction::On
                    | UnlimitedRetryAction::Off
                    | UnlimitedRetryAction::Status
            ));
        }
        let query: UnlimitedRetryRequest =
            serde_json::from_value(serde_json::json!({"sessionId": "s"})).unwrap();
        assert!(matches!(query.action, UnlimitedRetryAction::Status));
        for value in [
            serde_json::json!({"action": "on"}),
            serde_json::json!({"sessionId": "s", "action": "invalid"}),
            serde_json::json!({"sessionId": "s", "action": true}),
            serde_json::json!({"sessionId": "s", "enabled": true}),
        ] {
            assert!(serde_json::from_value::<UnlimitedRetryRequest>(value).is_err());
        }
    }
}
