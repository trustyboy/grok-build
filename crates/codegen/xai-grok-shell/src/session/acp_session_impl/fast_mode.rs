//! Fast 覆盖按会话和目录模型隔离，仅影响后续轮次。

use super::*;
use crate::session::{FastModeAction, FastModeState};

#[cfg(test)]
#[path = "fast_mode_tests.rs"]
mod tests;

impl SessionActor {
    /// 从会话配置解析接口和默认值，目录键与路由 slug 共用覆盖。
    pub(super) fn session_fast_mode_state(
        &self,
        cfg: &xai_grok_sampling_types::SamplingConfig,
    ) -> FastModeState {
        let models = self.models_manager.models();
        let key = crate::agent::remote_config::resolve_catalog_key(
            &models,
            &acp::ModelId::new(cfg.model.clone()),
        );
        let key = key
            .as_ref()
            .map_or(cfg.model.as_str(), |key| key.0.as_ref());
        let supported = cfg.api_backend == xai_grok_sampling_types::ApiBackend::Responses;
        let enabled = supported
            && self
                .fast_mode_overrides
                .borrow()
                .get(key)
                .copied()
                .unwrap_or(cfg.fast_mode);
        FastModeState {
            model_id: cfg.model.clone(),
            enabled,
            supported,
        }
    }

    /// 在同一 actor 命令内校验模型并切换，查询不会创建覆盖。
    pub(super) async fn handle_fast_mode(
        &self,
        action: FastModeAction,
        expected_model_id: Option<&str>,
    ) -> Result<FastModeState, acp::Error> {
        let cfg = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .ok_or_else(|| acp::Error::internal_error().data("会话缺少采样配置"))?;
        let models = self.models_manager.models();
        let current = acp::ModelId::new(cfg.model.clone());
        let key =
            crate::agent::remote_config::resolve_catalog_key(&models, &current).unwrap_or(current);
        if let Some(expected) = expected_model_id {
            let expected = acp::ModelId::new(expected);
            let expected_key = crate::agent::remote_config::resolve_catalog_key(&models, &expected)
                .unwrap_or(expected);
            if expected_key != key {
                return Err(
                    acp::Error::invalid_params().data("当前模型已变化，请重新查询 Fast 状态")
                );
            }
        }
        let state = self.session_fast_mode_state(&cfg);
        let enabled = match action {
            FastModeAction::Status => return Ok(state),
            FastModeAction::Toggle => !state.enabled,
            FastModeAction::On => true,
            FastModeAction::Off => false,
        };
        if !state.supported {
            return Err(acp::Error::invalid_params().data("Fast 仅支持 Responses 接口"));
        }
        self.fast_mode_overrides
            .borrow_mut()
            .insert(key.0.to_string(), enabled);
        Ok(FastModeState { enabled, ..state })
    }
}
