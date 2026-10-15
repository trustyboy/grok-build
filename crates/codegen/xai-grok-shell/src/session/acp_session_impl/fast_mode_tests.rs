use super::*;
use crate::agent::config::{ModelEntry, ModelInfo};
use xai_grok_sampling_types::ApiBackend;

/// 创建不读取真实用户配置的独立会话。
async fn actor() -> SessionActor {
    let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
    super::super::support::create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await
}

/// 注册配置默认值与目录别名，用于校验路由 slug 的归属。
fn insert_model(actor: &SessionActor, key: &str, slug: &str, backend: ApiBackend, fast_mode: bool) {
    let mut info = ModelInfo::fallback(slug);
    info.api_backend = backend;
    info.fast_mode = fast_mode;
    actor.models_manager.insert_test_entry(
        key,
        ModelEntry {
            info,
            mtls_cert_dir: None,
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
        },
    );
}

/// 模拟模型切换对采样配置的更新。
async fn select_model(actor: &SessionActor, key: &str) {
    let models = actor.models_manager.models();
    let info = &models[key].info;
    let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
    cfg.model = info.model.clone();
    cfg.api_backend = info.api_backend.clone();
    cfg.fast_mode = info.fast_mode;
    actor.chat_state_handle.update_sampling_config(cfg);
    assert_eq!(actor.current_model_id().await, info.model);
}

#[tokio::test(flavor = "current_thread")]
async fn fast_mode_defaults_toggle_isolation_and_model_switch() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let first = actor().await;
            let mut second = actor().await;
            second.models_manager = first.models_manager.clone();
            insert_model(&first, "off-key", "off-slug", ApiBackend::Responses, false);
            insert_model(&first, "on-key", "on-slug", ApiBackend::Responses, true);
            select_model(&first, "off-key").await;
            select_model(&second, "off-key").await;
            assert!(
                !first
                    .handle_fast_mode(FastModeAction::Status, Some("off-key"))
                    .await
                    .unwrap()
                    .enabled
            );
            assert!(first.fast_mode_overrides.borrow().is_empty());
            assert!(
                first
                    .handle_fast_mode(FastModeAction::Toggle, Some("off-slug"))
                    .await
                    .unwrap()
                    .enabled
            );
            assert!(first.reconstruct_full_config().await.fast_mode);
            assert!(!second.reconstruct_full_config().await.fast_mode);
            assert!(!first.models_manager.models()["off-key"].info.fast_mode);

            select_model(&first, "on-key").await;
            assert!(
                first
                    .handle_fast_mode(FastModeAction::Status, None)
                    .await
                    .unwrap()
                    .enabled
            );
            assert!(
                !first
                    .handle_fast_mode(FastModeAction::Off, None)
                    .await
                    .unwrap()
                    .enabled
            );
            assert!(!first.reconstruct_full_config().await.fast_mode);
            select_model(&first, "off-key").await;
            assert!(first.reconstruct_full_config().await.fast_mode);
            assert!(
                !first
                    .handle_fast_mode(FastModeAction::Toggle, None)
                    .await
                    .unwrap()
                    .enabled
            );
            select_model(&first, "on-key").await;
            assert!(!first.reconstruct_full_config().await.fast_mode);
            select_model(&second, "on-key").await;
            assert!(second.reconstruct_full_config().await.fast_mode);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn fast_mode_rejects_unsupported_stale_and_missing_config() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut actor = actor().await;
            for (key, backend) in [
                ("chat", ApiBackend::ChatCompletions),
                ("messages", ApiBackend::Messages),
            ] {
                insert_model(&actor, key, key, backend, true);
                select_model(&actor, key).await;
                let state = actor
                    .handle_fast_mode(FastModeAction::Status, None)
                    .await
                    .unwrap();
                assert!(!state.enabled && !state.supported);
                assert!(!actor.reconstruct_full_config().await.fast_mode);
                for action in [
                    FastModeAction::Toggle,
                    FastModeAction::On,
                    FastModeAction::Off,
                ] {
                    assert!(actor.handle_fast_mode(action, None).await.is_err());
                }
            }
            insert_model(
                &actor,
                "responses",
                "responses",
                ApiBackend::Responses,
                false,
            );
            select_model(&actor, "responses").await;
            assert!(
                actor
                    .handle_fast_mode(FastModeAction::On, Some("stale"))
                    .await
                    .is_err()
            );
            assert!(actor.fast_mode_overrides.borrow().is_empty());
            actor.chat_state_handle = xai_chat_state::ChatStateHandle::noop();
            assert!(
                actor
                    .handle_fast_mode(FastModeAction::On, None)
                    .await
                    .is_err()
            );
            assert!(actor.fast_mode_overrides.borrow().is_empty());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn fast_mode_inflight_snapshot_and_next_turn() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let actor = actor().await;
            insert_model(&actor, "old", "old", ApiBackend::Responses, false);
            insert_model(&actor, "new", "new", ApiBackend::Responses, true);
            select_model(&actor, "old").await;
            let snapshot = actor.capture_turn_sampling_state().await;
            super::super::sampler_turn::TURN_SAMPLING_STATE
                .scope(snapshot, async {
                    assert!(
                        actor
                            .handle_fast_mode(FastModeAction::On, None)
                            .await
                            .unwrap()
                            .enabled
                    );
                    assert!(!actor.reconstruct_full_config().await.fast_mode);
                    select_model(&actor, "new").await;
                    let sampler = actor.reconstruct_full_config().await;
                    assert_eq!(sampler.model, "old");
                    assert!(!sampler.fast_mode);
                })
                .await;
            assert!(actor.reconstruct_full_config().await.fast_mode);
            select_model(&actor, "old").await;
            assert!(actor.reconstruct_full_config().await.fast_mode);
            let snapshot = actor.capture_turn_sampling_state().await;
            super::super::sampler_turn::TURN_SAMPLING_STATE
                .scope(snapshot, async {
                    actor
                        .handle_fast_mode(FastModeAction::Off, None)
                        .await
                        .unwrap();
                    assert!(actor.reconstruct_full_config().await.fast_mode);
                })
                .await;
            assert!(!actor.reconstruct_full_config().await.fast_mode);
        })
        .await;
}
