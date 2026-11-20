//! 会话级无限重试开关：上游持续 503/429 时按 1 秒间隔重试，长任务不因重试预算中断。

use super::*;
use crate::session::{UnlimitedRetryAction, UnlimitedRetryState};

/// 无限重试的固定间隔（秒）。
pub(super) const UNLIMITED_RETRY_INTERVAL_SECS: u64 = 1;

impl SessionActor {
    /// 当前会话的无限重试状态；只读取会话覆盖，不查看模型配置。
    pub(super) fn session_unlimited_retry_state(&self) -> UnlimitedRetryState {
        UnlimitedRetryState {
            enabled: self.unlimited_retry.get(),
            interval_secs: UNLIMITED_RETRY_INTERVAL_SECS,
        }
    }

    /// 原子地翻转或设置开关；不写配置、不持久化，从下一轮请求开始生效。
    pub(super) async fn handle_unlimited_retry(
        &self,
        action: UnlimitedRetryAction,
    ) -> Result<UnlimitedRetryState, acp::Error> {
        let state = self.session_unlimited_retry_state();
        let enabled = match action {
            UnlimitedRetryAction::Status => return Ok(state),
            UnlimitedRetryAction::Toggle => !state.enabled,
            UnlimitedRetryAction::On => true,
            UnlimitedRetryAction::Off => false,
        };
        self.unlimited_retry.set(enabled);
        Ok(self.session_unlimited_retry_state())
    }
}
