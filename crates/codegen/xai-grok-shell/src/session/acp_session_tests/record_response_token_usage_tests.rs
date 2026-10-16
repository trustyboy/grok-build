use super::support::*;
use super::*;
use xai_grok_sampling_types::{
    BackendToolCallItem, BackendToolKind, ConversationItem, ConversationResponse, TokenUsage, rs,
};

fn response_with_usage(total_tokens: u32) -> ConversationResponse {
    ConversationResponse {
        items: vec![ConversationItem::assistant("ok")],
        stop_reason: None,
        usage: Some(TokenUsage {
            prompt_tokens: total_tokens.saturating_sub(50),
            completion_tokens: 50,
            total_tokens,

            reasoning_tokens: 0,
            cached_prompt_tokens: 0,
            cache_creation_prompt_tokens: 0,
        }),
        cost_usd_ticks: None,
        message_chunks_emitted: 1,
        doom_loop_signals: Vec::new(),
        stop_message: None,
        message_id: None,
        raw_stop_reason: None,
        stop_sequence: None,
    }
}

fn response_without_usage() -> ConversationResponse {
    ConversationResponse {
        items: vec![ConversationItem::assistant("ok")],
        stop_reason: None,
        usage: None,
        cost_usd_ticks: None,
        message_chunks_emitted: 1,
        doom_loop_signals: Vec::new(),
        stop_message: None,
        message_id: None,
        raw_stop_reason: None,
        stop_sequence: None,
    }
}

/// 原生压缩响应的本地元数据；密文大小与有效上下文计数故意不一致。
async fn response_with_native_context(actor: &SessionActor, estimated: bool) -> ConversationResponse {
    let mut response = response_with_usage(240_000);
    let assistant = response.assistant_mut().unwrap();
    assistant.responses_source = Some("trusted-source".into());
    assistant.responses_input_fingerprint = Some(xai_grok_sampling_types::responses_input_fingerprint(
        &xai_grok_sampling_types::ConversationRequest {
            items: actor.chat_state_handle.get_conversation().await,
            ..Default::default()
        },
        "trusted-source",
    ));
    assistant.responses_output = Some(vec![
        serde_json::json!({"type":"compaction", "encrypted_content":"x".repeat(1_200_000),
            "__grok_context_tokens":12_000, "__grok_context_estimated":estimated}),
        serde_json::json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"ok"}]}),
    ]);
    response
}

/// 测试原生策略，不修改真实配置或请求上游。
async fn enable_native_context(actor: &SessionActor) {
    let mut config = actor.chat_state_handle.get_sampling_config().await.unwrap();
    config.api_backend = xai_grok_sampling_types::ApiBackend::Responses;
    config.responses_compact_threshold = std::num::NonZeroU64::new(160_000);
    actor.chat_state_handle.update_sampling_config(config);
}

#[tokio::test(flavor = "current_thread")]
async fn native_context_usage_keeps_billing_and_tool_delta() {
    tokio::task::LocalSet::new().run_until(async {
        let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
        enable_native_context(&actor).await;
        let response = response_with_native_context(&actor, true).await;
        let context = actor.response_native_context(&response).await;
        assert_eq!(context, Some((12_000, true)));
        actor.record_response_token_usage(&response, None).await;
        actor.record_response_items(response.items, true).await;
        actor.commit_response_native_context(context).await;
        assert_eq!(actor.context_tokens_for_capacity().await, 12_000);
        assert!(actor.check_preflight_overflow().await.is_none());
        let usage = actor.chat_state_handle.try_get_session_usage().await.unwrap();
        assert_eq!(usage.totals.input_tokens, 239_950);
        assert_eq!(usage.totals.output_tokens, 50);
        let mut tool_call = ConversationItem::assistant("");
        if let ConversationItem::Assistant(assistant) = &mut tool_call {
            assistant.tool_calls.push(xai_grok_sampling_types::ToolCall {
                id: "call".into(), name: "test_tool".into(), arguments: "{}".into(),
            });
        }
        actor.chat_state_handle.push_assistant_response(tool_call);
        actor.chat_state_handle.push_tool_result(ConversationItem::tool_result("call", "abcd".repeat(100)));
        assert_eq!(actor.context_tokens_for_capacity().await, 12_100);
        actor.record_response_token_usage(&response_without_usage(), None).await;
        assert_eq!(actor.context_tokens_for_capacity().await, 12_100);
    }).await;
}

#[tokio::test(flavor = "current_thread")]
async fn native_context_metadata_requires_source_mode_and_latest_valid_shape() {
    tokio::task::LocalSet::new().run_until(async {
        let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
        let mut response = response_with_native_context(&actor, false).await;
        assert_eq!(actor.response_native_context(&response).await, None);
        enable_native_context(&actor).await;
        assert_eq!(actor.response_native_context(&response).await, Some((12_000, false)));
        response.assistant_mut().unwrap().responses_source = None;
        assert_eq!(actor.response_native_context(&response).await, None);
        actor.record_response_token_usage(&response, None).await;
        assert_eq!(actor.chat_state_handle.get_total_tokens().await, 240_000);
        response.assistant_mut().unwrap().responses_source = Some("trusted-source".into());
        response.assistant_mut().unwrap().responses_output.as_mut().unwrap().push(
            serde_json::json!({"type":"compaction", "encrypted_content":"new"}));
        assert_eq!(actor.response_native_context(&response).await, None);
        response.assistant_mut().unwrap().responses_output = None;
        assert_eq!(actor.response_native_context(&response).await, None);
    }).await;
}

#[tokio::test(flavor = "current_thread")]
async fn native_context_without_usage_does_not_count_ciphertext_twice() {
    tokio::task::LocalSet::new().run_until(async {
        let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
        enable_native_context(&actor).await;
        let mut response = response_with_native_context(&actor, true).await;
        response.usage = None;
        let context = actor.response_native_context(&response).await;
        actor.record_response_token_usage(&response, None).await;
        actor.record_response_items(response.items, context.is_some()).await;
        actor.commit_response_native_context(context).await;
        assert_eq!(actor.chat_state_handle.get_estimated_total_tokens().await, 12_000);
        assert_eq!(actor.chat_state_handle.try_get_session_usage().await.unwrap().totals.model_calls, 0);
    }).await;
}

#[tokio::test(flavor = "current_thread")]
async fn native_context_history_change_and_restore_use_full_history_guard() {
    tokio::task::LocalSet::new().run_until(async {
        let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
        enable_native_context(&actor).await;
        actor.chat_state_handle.push_user_message_and_ack(ConversationItem::user("abcd".repeat(270_000))).await;
        let response = response_with_native_context(&actor, true).await;
        let context = actor.response_native_context(&response).await;
        actor.record_response_token_usage(&response, None).await;
        actor.record_response_items(response.items, true).await;
        actor.commit_response_native_context(context).await;
        assert_eq!(actor.context_tokens_for_capacity().await, 12_000);
        let original = actor.chat_state_handle.get_conversation().await;
        let mut changed = original.clone();
        if let ConversationItem::Assistant(assistant) = changed.last_mut().unwrap() {
            assistant.responses_source = None;
        }
        actor.chat_state_handle.replace_conversation(changed);
        assert!(actor.context_tokens_for_capacity().await >= 270_000);
        assert!(actor.check_preflight_overflow().await.is_some());
        // 恢复旧 raw 不会恢复进程内来源锚点，也不能重新套用旧的低计数。
        actor.chat_state_handle.replace_conversation(original);
        assert!(actor.context_tokens_for_capacity().await >= 270_000);
        assert!(actor.compaction.native_context.borrow().is_none());
    }).await;
}

#[tokio::test(flavor = "current_thread")]
async fn native_context_credentials_change_invalidates_baseline() {
    tokio::task::LocalSet::new().run_until(async {
        let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
        enable_native_context(&actor).await;
        actor.chat_state_handle.push_user_message_and_ack(ConversationItem::user("abcd".repeat(270_000))).await;
        let response = response_with_native_context(&actor, true).await;
        let context = actor.response_native_context(&response).await;
        actor.record_response_token_usage(&response, None).await;
        actor.record_response_items(response.items, true).await;
        actor.commit_response_native_context(context).await;
        let mut credentials = actor.chat_state_handle.get_credentials().await;
        credentials.api_key = Some("rotated".into());
        actor.chat_state_handle.update_credentials(credentials);
        assert!(actor.context_tokens_for_capacity().await >= 270_000);
        assert!(actor.compaction.native_context.borrow().is_none());
    }).await;
}

#[tokio::test(flavor = "current_thread")]
async fn native_context_invalid_projection_and_disabled_mode_do_not_keep_low_count() {
    tokio::task::LocalSet::new().run_until(async {
        let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
        enable_native_context(&actor).await;
        actor.chat_state_handle.push_user_message_and_ack(ConversationItem::user("abcd".repeat(270_000))).await;
        let mut response = response_with_native_context(&actor, true).await;
        response.assistant_mut().unwrap().responses_input_fingerprint = Some("stale".into());
        let context = actor.response_native_context(&response).await;
        actor.record_response_token_usage(&response, None).await;
        actor.record_response_items(response.items, true).await;
        actor.commit_response_native_context(context).await;
        assert!(actor.context_tokens_for_capacity().await >= 270_000);
        assert!(actor.compaction.native_context.borrow().is_none());

        let response = response_with_native_context(&actor, false).await;
        let context = actor.response_native_context(&response).await;
        actor.record_response_token_usage(&response, None).await;
        actor.record_response_items(response.items, true).await;
        actor.commit_response_native_context(context).await;
        assert_eq!(actor.context_tokens_for_capacity().await, 12_000);
        let mut config = actor.chat_state_handle.get_sampling_config().await.unwrap();
        config.responses_compact_threshold = None;
        actor.chat_state_handle.update_sampling_config(config);
        assert!(actor.context_tokens_for_capacity().await >= 270_000);
        assert!(actor.check_preflight_overflow().await.is_some());
    }).await;
}

/// A reasoning-capable Responses turn reports the live context after its output has already been included.
/// Persisting that output must not add it again to the context sent to the pager or the pre-sampling compaction gate.
#[tokio::test(flavor = "current_thread")]
async fn response_reasoning_does_not_inflate_model_reported_context() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let (actor, mut event_rx) =
                create_test_actor_ex(0, 500_000, 95, gateway_tx, persistence_tx).await;
            let response = ConversationResponse {
                items: vec![
                    ConversationItem::Reasoning(rs::ReasoningItem {
                        id: "reasoning-1".to_string(),
                        summary: vec![],
                        content: None,
                        encrypted_content: Some("r".repeat(984_000)),
                        status: None,
                    }),
                    ConversationItem::BackendToolCall(BackendToolCallItem {
                        kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
                            id: "search-1".to_string(),
                            status: rs::WebSearchToolCallStatus::Completed,
                            action: rs::WebSearchToolCallAction::Search(
                                rs::WebSearchActionSearch {
                                    query: "context accounting".to_string(),
                                    sources: Some(vec![]),
                                },
                            ),
                        }),
                    }),
                    ConversationItem::assistant("ok"),
                ],
                usage: Some(TokenUsage {
                    prompt_tokens: 400_000,
                    completion_tokens: 40_000,
                    total_tokens: 440_000,
                    reasoning_tokens: 39_999,
                    cached_prompt_tokens: 0,
                    cache_creation_prompt_tokens: 0,
                }),
                stop_reason: None,
                cost_usd_ticks: None,
                message_chunks_emitted: 1,
                doom_loop_signals: Vec::new(),
                stop_message: None,
                message_id: None,
                raw_stop_reason: None,
                stop_sequence: None,
            };

            actor.record_response_token_usage(&response, None).await;
            let usage_reported = response.usage.is_some();
            actor
                .record_response_items(response.items, usage_reported)
                .await;

            assert_eq!(actor.chat_state_handle.get_total_tokens().await, 440_000);
            assert_eq!(
                actor.chat_state_handle.get_estimated_total_tokens().await,
                440_000,
                "pager metadata must not become 686K by adding 246K of already-counted reasoning",
            );

            actor
                .send_update(
                    acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(
                        vec![],
                    )),
                    None,
                )
                .await;
            let notification = event_rx.recv().await.expect("notification queued");
            let SessionEvent::Notification(SessionNotification::Acp(notification)) = notification
            else {
                panic!("expected ACP notification");
            };
            assert_eq!(
                notification
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get("totalTokens"))
                    .and_then(serde_json::Value::as_u64),
                Some(440_000),
                "the pager reads this metadata as its displayed context",
            );

            assert!(
                actor.check_auto_compact_needed().await.is_none(),
                "440K is below a 95% threshold for a 500K context window",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn response_without_usage_keeps_model_output_as_estimated_growth() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(100_000, 500_000, 95, gateway_tx, persistence_tx).await;
            let response = ConversationResponse {
                items: vec![
                    ConversationItem::Reasoning(rs::ReasoningItem {
                        id: "reasoning-1".to_string(),
                        summary: vec![],
                        content: None,
                        encrypted_content: Some("r".repeat(4_000)),
                        status: None,
                    }),
                    ConversationItem::assistant("a".repeat(4_000)),
                ],
                usage: None,
                stop_reason: None,
                cost_usd_ticks: None,
                message_chunks_emitted: 1,
                doom_loop_signals: Vec::new(),
                stop_message: None,
                message_id: None,
                raw_stop_reason: None,
                stop_sequence: None,
            };

            actor.record_response_token_usage(&response, None).await;
            let usage_reported = response.usage.is_some();
            actor
                .record_response_items(response.items, usage_reported)
                .await;

            assert_eq!(actor.chat_state_handle.get_total_tokens().await, 100_000);
            assert!(
                actor
                    .chat_state_handle
                    .get_conversation()
                    .await
                    .iter()
                    .any(|item| matches!(item, ConversationItem::Reasoning(_))),
                "reasoning must still be persisted",
            );
            // The encrypted-only reasoning estimates at 4000*3/4/4 = 750 after the base64 correction
            // The assistant text estimates at 4000/4 = 1000
            assert_eq!(
                actor.chat_state_handle.get_estimated_total_tokens().await,
                101_750,
                "without provider usage, reasoning and assistant text remain estimated growth",
            );
        })
        .await;
}

/// `record_response_token_usage` must update `chat_state.total_tokens` to the model-reported value.
/// Without this call, `total_tokens` stays frozen at the seed from `ChatState::new`.
/// That freezes `/context` and corrupts resume restore (the original bug).
#[tokio::test(flavor = "current_thread")]
async fn updates_chat_state_total_tokens_from_response_usage() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let _sync = actor.chat_state_handle.get_total_tokens().await;
            assert_eq!(actor.chat_state_handle.get_total_tokens().await, 0);

            actor.record_response_token_usage(&response_with_usage(150_000), None).await;

            assert_eq!(actor.chat_state_handle.get_total_tokens().await, 150_000);
            let prompt = actor
                .chat_state_handle
                .try_get_prompt_usage()
                .await
                .expect("chat-state alive")
                .expect("prompt ledger opened");
            assert_eq!(prompt.totals.model_calls, 1);
            assert_eq!(prompt.totals.input_tokens, 149_950);
            assert!(prompt.totals.cost_usd_ticks.is_none());
            assert_eq!(
                actor
                    .chat_state_handle
                    .try_get_session_usage()
                    .await
                    .expect("chat-state actor alive")
                    .totals
                    .model_calls,
                1
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn preserves_total_tokens_when_response_has_no_usage() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(99_999, 256_000, 85, gateway_tx, persistence_tx).await;
            let _sync = actor.chat_state_handle.get_total_tokens().await;

            actor.record_response_token_usage(&response_without_usage(), None).await;

            assert_eq!(actor.chat_state_handle.get_total_tokens().await, 99_999);
        })
        .await;
}

/// The wire contract the pager and TUI renderers depend on.
/// `build_session_info().context.used` must reflect the model-reported `total_tokens` after a turn.
/// `usage_pct`, `free_tokens`, and `message_tokens` must be server-computed, not derived by the renderer via subtraction.
#[tokio::test(flavor = "current_thread")]
async fn build_session_info_used_reflects_recorded_response() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // Push a small non-system fixture (user, assistant, and tool result).
            // Without non-system items `message_tokens` would be 0 and the regression guard below would pass vacuously.
            // Bytes/4 of these strings is small but above zero.
            actor
                .chat_state_handle
                .push_assistant_response(ConversationItem::assistant("hi there hi there hi there"));
            actor
                .chat_state_handle
                .push_tool_result(ConversationItem::tool_result(
                    "call-1",
                    "tool result body tool result body",
                ));
            actor
                .chat_state_handle
                .push_user_message_and_ack(ConversationItem::user("hello hello hello hello"))
                .await;

            actor.record_response_token_usage(&response_with_usage(120_000), None).await;

            let info = actor.build_session_info().await;
            assert_eq!(info.context.used, 120_000);
            assert_eq!(info.context.total, 256_000);
            // Server-computed: the renderer no longer derives these
            assert_eq!(info.context.free_tokens, 256_000 - 120_000);
            // 120_000 / 256_000 = 0.46875, which rounds to 47
            assert_eq!(info.context.usage_pct, 47);
            // Regression guard: bytes/4 of the non-system items must be above zero
            // Subtraction-based formulas saturated this to zero; the direct sum returns the real estimate
            assert!(
                info.context.message_tokens > 0,
                "message_tokens should reflect non-system items, got {}",
                info.context.message_tokens,
            );
            assert!(
                info.context.usage_pct > 0,
                "usage_pct should be non-zero when used > 0",
            );
        })
        .await;
}

/// `build_session_info` must populate `SessionInfoData.show_model_fingerprint` from the catalog entry for the session's current model.
/// `build_session_info` reads the slug from the sampling config, so a direct `.get(slug)` would miss the entry and wrongly yield false.
/// The flag is the sole control (the client keeps no built-in per-slug default), so this is the only thing that can turn checkpoint identity on.
#[tokio::test(flavor = "current_thread")]
async fn build_session_info_sources_show_model_fingerprint_from_catalog() {
    use crate::agent::config::{ModelEntry, ModelInfo};

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // The catalog KEY ("custom-catalog-id") differs from the session's routing SLUG ("test", the harness sampling model)
            // The flag starts off, so the lookup must yield false
            let mut entry = ModelEntry {
                info: ModelInfo::fallback("test"),
                mtls_cert_dir: None,
                api_key: None,
                env_key: None,
                auth_provider: None,
                api_base_url: None,
            };
            entry.info.show_model_fingerprint = false;
            actor
                .models_manager
                .insert_test_entry("custom-catalog-id", entry.clone());
            assert!(
                !actor.build_session_info().await.show_model_fingerprint,
                "non-coding slug without the catalog flag must yield false",
            );

            // The same entry with the flag ON must yield true, exercising slug-to-catalog-key resolution end-to-end
            // A direct slug `.get("test")` would miss the entry keyed "custom-catalog-id" and regress to false
            entry.info.show_model_fingerprint = true;
            actor
                .models_manager
                .insert_test_entry("custom-catalog-id", entry);
            assert!(
                actor.build_session_info().await.show_model_fingerprint,
                "catalog show_model_fingerprint=true must flow to SessionInfoData via slug→key resolution",
            );
        })
        .await;
}

/// `record_response_token_usage` must also stash the per-turn `TokenUsage` in chat state.
/// The next `PromptResponse._meta` carries the input/output token counts to the bot's telemetry.
#[tokio::test(flavor = "current_thread")]
async fn stashes_per_turn_usage_in_chat_state() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // Baseline: no stashed usage.
            assert!(
                actor
                    .chat_state_handle
                    .get_last_turn_usage()
                    .await
                    .is_none()
            );

            // The fixture splits total 200_000 into prompt 199_950 and completion 50
            actor.record_response_token_usage(&response_with_usage(200_000), None).await;

            let stashed = actor
                .chat_state_handle
                .get_last_turn_usage()
                .await
                .expect("usage stashed by record_response_token_usage");
            assert_eq!(stashed.prompt_tokens, 199_950);
            assert_eq!(stashed.completion_tokens, 50);
            assert_eq!(stashed.total_tokens, 200_000);
        })
        .await;
}
