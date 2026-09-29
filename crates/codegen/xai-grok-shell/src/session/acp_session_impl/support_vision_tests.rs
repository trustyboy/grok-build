use super::*;
use crate::agent::config::{ModelEntry, ModelInfo};

/// 创建独立会话，避免测试读取真实配置。
async fn actor() -> SessionActor {
    let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
    super::super::support::create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await
}

/// 注册目录 key 与路由 slug 不同的模型。
fn insert_model(actor: &SessionActor, key: &str, slug: &str, supports_vision: bool) {
    let mut info = ModelInfo::fallback(slug);
    info.supports_vision = supports_vision;
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

/// 模拟模型切换写入的会话采样状态。
async fn select_model(actor: &SessionActor, model: &str) {
    let mut config = actor.chat_state_handle.get_sampling_config().await.unwrap();
    config.model = model.to_owned();
    actor.chat_state_handle.update_sampling_config(config);
    assert_eq!(actor.current_model_id().await, model);
}

#[tokio::test(flavor = "current_thread")]
async fn support_vision_query_uses_session_model_and_resolves_slug() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let actor = actor().await;
            insert_model(&actor, "text-key", "text-slug", false);
            select_model(&actor, "text-slug").await;
            let current = actor.models_manager.current_model_id();
            assert_ne!(current.0.as_ref(), "text-slug");
            let state = actor
                .handle_support_vision(None, Some("text-key"))
                .await
                .unwrap();
            assert_eq!(state.model_id, "text-slug");
            assert!(!state.enabled);
            assert!(actor.vision_overrides.borrow().is_empty());
            let config = actor.reconstruct_full_config().await;
            assert_eq!(config.model, "text-slug");
            assert!(!config.supports_vision);
            assert_eq!(actor.models_manager.current_model_id(), current);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn support_vision_isolated_and_retained_after_model_switch() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let first = actor().await;
            let mut second = actor().await;
            second.models_manager = first.models_manager.clone();
            insert_model(&first, "text-key", "text-slug", false);
            insert_model(&first, "vision-key", "vision-slug", true);
            select_model(&first, "text-slug").await;
            select_model(&second, "text-slug").await;
            let config_before = first.chat_state_handle.get_sampling_config().await.unwrap();
            let enabled = first
                .handle_support_vision(Some(true), Some("text-key"))
                .await
                .unwrap();
            assert!(enabled.enabled);
            assert!(
                !second
                    .handle_support_vision(None, None)
                    .await
                    .unwrap()
                    .enabled
            );
            assert_eq!(
                serde_json::to_value(config_before).unwrap(),
                serde_json::to_value(first.chat_state_handle.get_sampling_config().await.unwrap())
                    .unwrap()
            );
            assert!(
                !first.models_manager.models()["text-key"]
                    .info
                    .supports_vision
            );
            select_model(&first, "vision-slug").await;
            first
                .handle_support_vision(Some(false), None)
                .await
                .unwrap();
            select_model(&first, "text-key").await;
            assert!(
                first
                    .handle_support_vision(None, None)
                    .await
                    .unwrap()
                    .enabled
            );
            select_model(&first, "vision-key").await;
            assert!(
                !first
                    .handle_support_vision(None, None)
                    .await
                    .unwrap()
                    .enabled
            );
            let mut resumed = actor().await;
            resumed.models_manager = first.models_manager.clone();
            select_model(&resumed, "vision-slug").await;
            assert!(
                resumed
                    .handle_support_vision(None, None)
                    .await
                    .unwrap()
                    .enabled
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn support_vision_stale_model_and_missing_config_do_not_mutate() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut actor = actor().await;
            insert_model(&actor, "text-key", "text-slug", false);
            select_model(&actor, "text-slug").await;
            assert!(
                actor
                    .handle_support_vision(Some(true), Some("other"))
                    .await
                    .is_err()
            );
            assert!(actor.vision_overrides.borrow().is_empty());
            assert!(
                !actor
                    .handle_support_vision(None, None)
                    .await
                    .unwrap()
                    .enabled
            );
            actor.chat_state_handle = xai_chat_state::ChatStateHandle::noop();
            assert!(actor.handle_support_vision(Some(true), None).await.is_err());
            assert!(actor.vision_overrides.borrow().is_empty());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn support_vision_hook_matches_wire_images_and_keeps_attachments() {
    tokio::task::LocalSet::new().run_until(async {
        let (gateway_tx, mut gateway_rx) = tokio::sync::mpsc::unbounded_channel();
        let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let actor = super::super::support::create_test_actor(
            0, 256_000, 85, gateway_tx, persistence_tx,
        ).await;
        insert_model(&actor, "text-key", "test", false);
        actor.client_hooks.borrow_mut().insert(
            xai_grok_hooks::event::HookEventName::UserPromptTransform,
            vec![crate::extensions::hooks::ClientHookGroup {
                matcher: None, callback_ids: vec!["vision".into()], timeout: None,
            }],
        );
        let (wire_tx, mut wire_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new().route("/chat/completions", axum::routing::post(
            move |axum::Json(body): axum::Json<serde_json::Value>| {
                let wire_tx = wire_tx.clone();
                async move {
                    wire_tx.send(body).unwrap();
                    ([("content-type", "text/event-stream")],
                        "data: {\"id\":\"test\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let mut config = actor.chat_state_handle.get_sampling_config().await.unwrap();
        config.base_url = format!("http://{address}");
        actor.chat_state_handle.update_sampling_config(config);
        let directory = tempfile::tempdir().unwrap();
        let images = [acp::ImageContent::new("aW1hZ2U=", "image/png")];
        let (text, paths) = crate::session::image_describe::persist_and_prepend_image_files(
            directory.path(), &images, "检查图片",
        ).unwrap();
        for enabled in [false, true] {
            actor.handle_support_vision(Some(enabled), None).await.unwrap();
            let snapshot = actor.capture_turn_sampling_state().await;
            super::super::sampler_turn::TURN_SAMPLING_STATE.scope(snapshot, async {
                let hook_vision = super::super::sampler_turn::TURN_SAMPLING_STATE
                    .with(|state| state.supports_vision);
                let prompt = actor.dispatch_user_prompt_transform(
                    text.clone(), Some("vision-test"), &images, &paths, hook_vision,
                ).await;
                assert!(prompt.contains("<image_files>"));
                assert!(std::path::Path::new(&paths[0]).is_file());
                let xai_acp_lib::AcpClientMessage::ExtNotification(notification) =
                    gateway_rx.try_recv().expect("hook notification") else {
                        panic!("expected hook notification");
                    };
                let hook: serde_json::Value = serde_json::from_str(notification.request.params.get()).unwrap();
                assert_eq!(hook["supports_vision"], enabled);
                assert_eq!(hook["images"][0]["path"], paths[0]);
                let mut user = ConversationItem::user(prompt);
                user.add_image("data:image/png;base64,aW1hZ2U=".to_owned());
                let mut config = actor.reconstruct_full_config().await;
                config.max_retries = Some(0);
                let sampler = xai_grok_sampler::SamplingClient::new(config).unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(10),
                    sampler.conversation_collect(ConversationRequest {
                        items: vec![user], ..Default::default()
                    }),
                ).await.expect("mock response timeout").unwrap();
                let wire = wire_rx.try_recv().unwrap();
                assert_eq!(wire["model"], "test");
                let serialized = wire.to_string();
                assert_eq!(serialized.contains("\"image_url\""), enabled);
                assert!(serialized.contains("<image_files>"));
            }).await;
        }
        drop(server);
    }).await;
}

#[tokio::test(flavor = "current_thread")]
async fn support_vision_inflight_snapshot_and_next_turn_agree() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let actor = actor().await;
            insert_model(&actor, "text-key", "text-slug", false);
            insert_model(&actor, "vision-key", "vision-slug", true);
            select_model(&actor, "text-slug").await;
            actor
                .chat_state_handle
                .update_credentials(xai_chat_state::Credentials {
                    api_key: Some("old-model-key".to_owned()),
                    ..Default::default()
                });
            let snapshot = actor.capture_turn_sampling_state().await;
            super::super::sampler_turn::TURN_SAMPLING_STATE
                .scope(snapshot, async {
                    let reply = actor
                        .handle_support_vision(Some(true), Some("text-key"))
                        .await
                        .unwrap();
                    assert!(reply.enabled);
                    assert!(
                        !super::super::sampler_turn::TURN_SAMPLING_STATE
                            .with(|state| state.supports_vision)
                    );
                    let sampler = actor.reconstruct_full_config().await;
                    assert_eq!(sampler.model, "text-slug");
                    assert!(!sampler.supports_vision);
                    // 上游修正窗口或压缩更新同模型配置时，本轮快照不能恢复旧值。
                    let mut adjusted = actor.chat_state_handle.get_sampling_config().await.unwrap();
                    adjusted.context_window = std::num::NonZeroU64::new(128_000).unwrap();
                    actor.chat_state_handle.update_sampling_config(adjusted);
                    assert_eq!(actor.reconstruct_full_config().await.context_window, 128_000);
                    actor
                        .chat_state_handle
                        .update_credentials(xai_chat_state::Credentials {
                            api_key: Some("refreshed-key".to_owned()),
                            ..Default::default()
                        });
                    assert_eq!(
                        actor.reconstruct_full_config().await.api_key.as_deref(),
                        Some("refreshed-key")
                    );
                    select_model(&actor, "vision-slug").await;
                    actor
                        .chat_state_handle
                        .update_credentials(xai_chat_state::Credentials {
                            api_key: Some("other-model-key".to_owned()),
                            ..Default::default()
                        });
                    let sampler = actor.reconstruct_full_config().await;
                    assert_eq!(sampler.model, "text-slug");
                    assert!(!sampler.supports_vision);
                    assert_ne!(sampler.api_key.as_deref(), Some("other-model-key"));
                })
                .await;
            let sampler = actor.reconstruct_full_config().await;
            assert_eq!(sampler.model, "vision-slug");
            assert!(sampler.supports_vision);
            select_model(&actor, "text-slug").await;
            assert!(actor.reconstruct_full_config().await.supports_vision);
        })
        .await;
}
