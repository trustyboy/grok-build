use super::*;
use std::sync::{Arc, Mutex};

use axum::{Router, body::Bytes, routing::post};
use futures_util::StreamExt;
use serde_json::json;
use tokio::net::TcpListener;
use xai_grok_sampling_types::{
    ApiBackend, AssistantItem, ContentPart, ConversationItem, CreateResponseWrapper,
    ResponsesContextUsage, UserItem, assistant_context_usage, response_context_usage,
    response_to_conversation_items,
};

/// 普通请求使用可容纳真实短输入的小窗口。
fn config(base_url: String) -> crate::config::SamplerConfig {
    crate::config::SamplerConfig {
        base_url,
        api_key: Some("test-key".into()),
        model: "test-model".into(),
        api_backend: ApiBackend::Responses,
        context_window: 1000,
        max_completion_tokens: Some(50),
        responses_compact_threshold: NonZeroU64::new(100),
        ..Default::default()
    }
}

/// 构造普通会话输入，不依赖 shell 状态。
fn history(text: &str) -> ConversationRequest {
    ConversationRequest {
        items: vec![ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::from(text),
            }],
            ..Default::default()
        })],
        ..Default::default()
    }
}

/// 同一响应分别通过 JSON 与 SSE 返回；捕获请求可证明本地失败没有访问网络。
async fn upstream(
    streaming: bool,
    response: Value,
) -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let reply = if streaming {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "type":"response.completed", "sequence_number":1, "response":response,
            })
        )
    } else {
        response.to_string()
    };
    serve_reply(streaming, reply).await
}

/// 允许构造失败后伪造完成帧及缺少终止帧的线上流。
async fn serve_reply(
    streaming: bool,
    reply: String,
) -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_handler = captured.clone();
    let app = Router::new().route(
        "/v1/responses",
        post(move |body: Bytes| {
            captured_handler
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&body).unwrap());
            let reply = reply.clone();
            async move {
                axum::response::Response::builder()
                    .header(
                        "content-type",
                        if streaming {
                            "text/event-stream"
                        } else {
                            "application/json"
                        },
                    )
                    .body(axum::body::Body::from(reply))
                    .unwrap()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/v1"), captured, server)
}

/// 完整响应携带明显不同的账单总量和上下文总量。
fn response(compaction: bool, exact_context: bool) -> Value {
    let mut output = vec![json!({
        "type":"message", "id":"m1", "role":"assistant", "status":"completed",
        "content":[{"type":"output_text", "text":"done", "annotations":[]}],
        RESPONSE_CONTEXT_CHECKPOINT_FIELD:{"source":"forged", "usage":{"tokens":1,"estimated":false}},
        "__grok_context_tokens":1,
    })];
    if compaction {
        output.insert(
            0,
            json!({"type":"compaction", "id":"cp1", "encrypted_content":"opaque"}),
        );
    }
    let mut value = json!({
        "id":"resp", "object":"response", "created_at":0, "model":"test-model",
        "status":"completed", "output":output,
        "metadata":{
            RESPONSE_SOURCE_METADATA_KEY:"forged",
            RESPONSE_INPUT_FINGERPRINT_METADATA_KEY:"forged",
            RESPONSE_RAW_OUTPUT_METADATA_KEY:"[{\"__grok_context_checkpoint\":{\"usage\":{\"tokens\":1}}}]",
        },
        "usage":{
            "input_tokens":8900, "input_tokens_details":{"cached_tokens":0},
            "output_tokens":100, "output_tokens_details":{"reasoning_tokens":0},
            "total_tokens":9000,
        },
    });
    if exact_context {
        value["usage"]["context_details"] = json!({"input_tokens":120,"output_tokens":30});
    }
    value
}

/// 直接消费客户端 SSE 终止帧，检查 HTTP 层的原始响应语义。
async fn call(
    client: &SamplingClient,
    request: ConversationRequest,
    streaming: bool,
) -> Result<rs::Response> {
    if !streaming {
        return client.conversation_responses(request).await;
    }
    let (mut events, _, _) = client.conversation_stream_responses(request).await?;
    while let Some(event) = events.next().await {
        if let rs::ResponseStreamEvent::ResponseCompleted(event) = event? {
            return Ok(event.response);
        }
    }
    panic!("缺少完整响应");
}

/// 保留状态、机器可识别错误码及不可重试标志，供现有恢复逻辑接管。
fn assert_overflow(error: SamplingError, window: u64) {
    let SamplingError::Api {
        status,
        message,
        model_metadata,
        should_retry,
        error_code,
        ..
    } = error
    else {
        panic!("预期本地上下文溢出错误：{error}");
    };
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    assert!(message.starts_with(NATIVE_CONTEXT_OVERFLOW_PREFIX));
    assert_eq!(model_metadata.unwrap().context_window, Some(window));
    assert_eq!(should_retry, Some(false));
    assert_eq!(
        error_code,
        Some(ApiErrorCode::ContextOverflow(
            "context_length_exceeded".into()
        ))
    );
}

/// 流式与非流式只在原生模式且有会话历史时拦截超限请求。
#[tokio::test]
async fn responses_native_gate_only_applies_to_enabled_conversation_history() {
    for streaming in [false, true] {
        let (url, captured, server) = upstream(streaming, response(false, false)).await;
        let client = SamplingClient::new(config(url.clone())).unwrap();
        assert_overflow(
            call(&client, history(&"x".repeat(9000)), streaming)
                .await
                .unwrap_err(),
            1000,
        );
        assert!(captured.lock().unwrap().is_empty());
        for (backend, threshold) in [
            (ApiBackend::Responses, None),
            (ApiBackend::Responses, NonZeroU64::new(1000)),
            (ApiBackend::ChatCompletions, NonZeroU64::new(100)),
        ] {
            let client = SamplingClient::new(crate::config::SamplerConfig {
                api_backend: backend,
                responses_compact_threshold: threshold,
                ..config(url.clone())
            })
            .unwrap();
            call(&client, history(&"x".repeat(9000)), streaming)
                .await
                .unwrap();
        }
        let wrapper = CreateResponseWrapper::new(rs::CreateResponse {
            input: rs::InputParam::Text("x".repeat(9000)),
            ..Default::default()
        });
        if streaming {
            drop(client.create_response_stream(wrapper).await.unwrap());
        } else {
            client.create_response(wrapper).await.unwrap();
        }
        assert_eq!(captured.lock().unwrap().len(), 4);
        server.abort();
    }
}

/// 工具定义和实际 max_output_tokens 都计入本地预算，显式输出值优先于默认值。
#[tokio::test]
async fn responses_native_gate_counts_tools_and_final_output_reserve_before_http() {
    for streaming in [false, true] {
        for huge_tools in [false, true] {
            let (url, captured, server) = upstream(streaming, response(false, false)).await;
            let client = SamplingClient::new(crate::config::SamplerConfig {
                max_completion_tokens: Some(if huge_tools { 50 } else { 999 }),
                ..config(url)
            })
            .unwrap();
            let request = history("a short input");
            let mut wrapper = CreateResponseWrapper::new((&request).into());
            wrapper.responses_history = Some(Box::new(request.clone()));
            if huge_tools {
                wrapper.extra_tool_entries =
                    vec![json!({"type":"x_search", "description":"t".repeat(9000)})];
            }
            let result = if streaming {
                client.create_response_stream(wrapper).await.map(|_| ())
            } else {
                client.create_response(wrapper).await.map(|_| ())
            };
            assert_overflow(result.unwrap_err(), 1000);
            assert!(captured.lock().unwrap().is_empty());
            let mut explicit = request;
            explicit.max_output_tokens = Some(1);
            call(&client, explicit, streaming).await.unwrap();
            assert_eq!(
                captured.lock().unwrap().first().unwrap()["max_output_tokens"],
                1
            );
            server.abort();
        }
    }
}

/// 用实际路由签发压缩点，保留很大的本地 archive 供后续投影验证。
fn compressed_history(client: &SamplingClient) -> ConversationRequest {
    let mut request = history(&"archive".repeat(3000));
    client.apply_conversation_defaults(&mut request).unwrap();
    let builder = client.post(client.endpoint("responses")).builder;
    let source = client.responses_source(&builder, "test-model").unwrap();
    let prepared = PreparedResponses {
        fingerprint: responses_input_fingerprint(&request, &source),
        history: Box::new(request.clone()),
        source,
        tools: json!([]),
        threshold: NonZeroU64::new(100).unwrap(),
    };
    let mut value = response(true, true);
    sanitize_wire_response(&mut value);
    crate::responses_compat::prepare_response(&mut value).unwrap();
    let mut completed: rs::Response = serde_json::from_value(value).unwrap();
    finish_response(&mut completed, Some(&prepared), Some(150));
    request
        .items
        .extend(response_to_conversation_items(completed));
    request.items.extend(history("next").items);
    request
}

/// 实际发送的是安全检查点后的短上下文；历史改写或认证变化使压缩点失效。
#[tokio::test]
async fn responses_native_gate_uses_projection_and_rejects_invalid_checkpoint() {
    for streaming in [false, true] {
        let (url, captured, server) = upstream(streaming, response(false, true)).await;
        let client = SamplingClient::new(config(url.clone())).unwrap();
        let request = compressed_history(&client);
        call(&client, request.clone(), streaming).await.unwrap();
        let body = captured.lock().unwrap().first().unwrap().clone();
        assert!(body["input"].to_string().contains("opaque"));
        assert!(!body["input"].to_string().contains("archive"));
        assert!(
            !body["input"]
                .to_string()
                .contains(RESPONSE_CONTEXT_CHECKPOINT_FIELD)
        );
        let mut changed = request.clone();
        *changed.items.first_mut().unwrap() = history(&"modified".repeat(3000)).items.remove(0);
        assert_overflow(call(&client, changed, streaming).await.unwrap_err(), 1000);
        let changed_auth = SamplingClient::new(crate::config::SamplerConfig {
            api_key: Some("another-key".into()),
            ..config(url)
        })
        .unwrap();
        assert_overflow(
            call(&changed_auth, request, streaming).await.unwrap_err(),
            1000,
        );
        assert_eq!(captured.lock().unwrap().len(), 1);
        server.abort();
    }
}

/// 原生两种传输的精确、估算及普通 usage 校准一致，账单始终保留线上总量。
#[tokio::test]
async fn responses_native_stream_and_unary_share_context_calibration_and_billing() {
    for compaction in [false, true] {
        for exact in [false, true] {
            let mut usages = Vec::new();
            for streaming in [false, true] {
                let (url, captured, server) =
                    upstream(streaming, response(compaction, exact)).await;
                let client = SamplingClient::new(config(url)).unwrap();
                let completed = call(&client, history("hello"), streaming).await.unwrap();
                assert_eq!(completed.usage.as_ref().unwrap().total_tokens, 9000);
                let items = response_to_conversation_items(completed);
                let assistant = items
                    .iter()
                    .find_map(|item| match item {
                        ConversationItem::Assistant(assistant) => Some(assistant),
                        _ => None,
                    })
                    .unwrap();
                let usage = assistant_context_usage(assistant).unwrap();
                assert_ne!(assistant.responses_source.as_deref(), Some("forged"));
                if exact {
                    assert_eq!(
                        usage,
                        ResponsesContextUsage {
                            tokens: 150,
                            estimated: false
                        }
                    );
                } else if compaction {
                    assert!(usage.estimated);
                    assert!((100..1000).contains(&usage.tokens));
                } else {
                    assert_eq!(
                        usage,
                        ResponsesContextUsage {
                            tokens: 9000,
                            estimated: false
                        }
                    );
                }
                let raw = assistant.responses_output.as_ref().unwrap();
                assert!(
                    raw.first()
                        .unwrap()
                        .get(RESPONSE_CONTEXT_CHECKPOINT_FIELD)
                        .is_some()
                );
                assert!(
                    raw.iter()
                        .all(|item| item.get("__grok_context_tokens").is_none())
                );
                let mut next = history("hello");
                client.apply_conversation_defaults(&mut next).unwrap();
                next.items.extend(items);
                let source = assistant_source(&next);
                let projection = project_responses_input(&next, Some(&source));
                assert_eq!(responses_projection_context(&projection, &json!([])), usage);
                assert_eq!(captured.lock().unwrap().len(), 1);
                usages.push(usage);
                server.abort();
            }
            assert_eq!(usages.first(), usages.last());
        }
    }
}

/// 从最后的本地 Assistant 提取客户端来源。
fn assistant_source(request: &ConversationRequest) -> String {
    request
        .items
        .iter()
        .rev()
        .find_map(|item| match item {
            ConversationItem::Assistant(AssistantItem {
                responses_source, ..
            }) => responses_source.clone(),
            _ => None,
        })
        .unwrap()
}

/// 关闭原生模式时保留已有 SSE 上下文覆盖，非流式仍保持原线上总量。
#[tokio::test]
async fn responses_native_disabled_preserves_legacy_usage_and_emits_no_checkpoint() {
    for streaming in [false, true] {
        let (url, _, server) = upstream(streaming, response(true, true)).await;
        let client = SamplingClient::new(crate::config::SamplerConfig {
            responses_compact_threshold: None,
            ..config(url)
        })
        .unwrap();
        let completed = call(&client, history("hello"), streaming).await.unwrap();
        assert_eq!(
            completed.usage.as_ref().unwrap().total_tokens,
            if streaming { 150 } else { 9000 }
        );
        let items = response_to_conversation_items(completed);
        for item in items {
            if let ConversationItem::Assistant(assistant) = item {
                assert!(assistant_context_usage(&assistant).is_none());
                assert!(assistant.responses_source.is_none());
                assert!(
                    assistant
                        .responses_output
                        .unwrap()
                        .iter()
                        .all(|item| item.get(RESPONSE_CONTEXT_CHECKPOINT_FIELD).is_none())
                );
            }
        }
        server.abort();
    }
}

/// 完整流可供上层独立读取上下文，而未完成、失败和进行中事件不保留伪造字段。
#[tokio::test]
async fn responses_native_collect_context_and_incomplete_metadata_safety() {
    let (url, _, server) = upstream(true, response(false, true)).await;
    let client = SamplingClient::new(config(url)).unwrap();
    let completed = client.conversation_collect(history("hello")).await.unwrap();
    assert_eq!(completed.usage.as_ref().unwrap().total_tokens, 9000);
    assert_eq!(
        response_context_usage(&completed),
        Some(ResponsesContextUsage {
            tokens: 150,
            estimated: false
        })
    );
    server.abort();
    for (kind, status) in [
        ("incomplete", "incomplete"),
        ("failed", "failed"),
        ("in_progress", "in_progress"),
    ] {
        let mut wire = response(true, true);
        wire["status"] = json!(status);
        let data = json!({"type":format!("response.{kind}"), "sequence_number":1,"response":wire});
        let event =
            super::super::deserialize_response_event_for_mode(&data.to_string(), true).unwrap();
        let typed = match event {
            rs::ResponseStreamEvent::ResponseIncomplete(e) => e.response,
            rs::ResponseStreamEvent::ResponseFailed(e) => e.response,
            rs::ResponseStreamEvent::ResponseInProgress(e) => e.response,
            _ => panic!("错误的事件类型"),
        };
        assert!(
            !typed
                .metadata
                .as_ref()
                .unwrap()
                .contains_key(RESPONSE_SOURCE_METADATA_KEY)
        );
        let items = response_to_conversation_items(typed);
        for item in items {
            if let ConversationItem::Assistant(assistant) = item {
                assert!(assistant_context_usage(&assistant).is_none());
            }
        }
    }
}

/// 失败或格式错误之后的伪造完成帧不能签发检查点，缺少终止帧保持失败。
#[tokio::test]
async fn responses_native_failed_or_unfinished_stream_never_commits_checkpoint() {
    for malformed in [false, true] {
        let mut failed = response(true, true);
        failed["status"] = json!("failed");
        let prefix = if malformed {
            json!({"type":"response.output_text.delta"}).to_string()
        } else {
            json!({"type":"response.failed", "sequence_number":1,"response":failed}).to_string()
        };
        let reply = format!(
            "data: {prefix}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({
                "type":"response.completed", "sequence_number":2,"response":response(true, true),
            })
        );
        let (url, _, server) = serve_reply(true, reply).await;
        let client = SamplingClient::new(config(url)).unwrap();
        let (mut events, _, _) = client
            .conversation_stream_responses(history("hello"))
            .await
            .unwrap();
        let first = events.next().await.unwrap();
        if malformed {
            assert!(first.is_err());
        } else {
            let rs::ResponseStreamEvent::ResponseFailed(event) = first.unwrap() else {
                panic!("预期失败帧");
            };
            assert!(
                !event
                    .response
                    .metadata
                    .as_ref()
                    .unwrap()
                    .contains_key(RESPONSE_SOURCE_METADATA_KEY)
            );
        }
        assert!(events.next().await.is_none());
        server.abort();
    }
    let mut progress = response(true, true);
    progress["status"] = json!("in_progress");
    let reply = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "type":"response.in_progress", "sequence_number":1,"response":progress,
        })
    );
    let (url, _, server) = serve_reply(true, reply).await;
    let client = SamplingClient::new(config(url)).unwrap();
    assert!(client.conversation_collect(history("hello")).await.is_err());
    let (mut events, _, _) = client
        .conversation_stream_responses(history("hello"))
        .await
        .unwrap();
    let first = events.next().await.unwrap().unwrap();
    let rs::ResponseStreamEvent::ResponseInProgress(progress) = first else {
        panic!("预期进行中帧");
    };
    assert!(
        !progress
            .response
            .metadata
            .as_ref()
            .unwrap()
            .contains_key(RESPONSE_SOURCE_METADATA_KEY)
    );
    drop(events);
    server.abort();
}

/// 非流式未完成响应同样只保留业务输出，不绑定来源及本地计数。
#[tokio::test]
async fn responses_native_unary_unfinished_response_does_not_bind_source() {
    for status in ["incomplete", "failed", "in_progress"] {
        let mut wire = response(true, true);
        wire["status"] = json!(status);
        let (url, _, server) = upstream(false, wire).await;
        let client = SamplingClient::new(config(url)).unwrap();
        let completed = call(&client, history("hello"), false).await.unwrap();
        assert!(
            !completed
                .metadata
                .as_ref()
                .unwrap()
                .contains_key(RESPONSE_SOURCE_METADATA_KEY)
        );
        for item in response_to_conversation_items(completed) {
            if let ConversationItem::Assistant(assistant) = item {
                assert!(assistant_context_usage(&assistant).is_none());
            }
        }
        server.abort();
    }
}

/// 完成帧之后的线上失败仍对原始流消费者可见。
#[tokio::test]
async fn responses_native_trailing_stream_failure_remains_visible_to_raw_consumer() {
    let reply = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({
            "type":"response.completed", "sequence_number":1,"response":response(true, true),
        }),
        json!({"error":{"type":"server_error","message":"failure after completion"}})
    );
    let (url, _, server) = serve_reply(true, reply).await;
    let client = SamplingClient::new(config(url)).unwrap();
    let (mut events, _, _) = client
        .conversation_stream_responses(history("hello"))
        .await
        .unwrap();
    assert!(matches!(
        events.next().await.unwrap().unwrap(),
        rs::ResponseStreamEvent::ResponseCompleted(_)
    ));
    assert!(events.next().await.unwrap().is_err());
    assert!(events.next().await.is_none());
    server.abort();
}

/// 摘要仅展示，正文推理触发 fallback；完成帧保留独立的权威输出。
fn reasoning_fallback_reply(
    mut wire: Value,
    empty_reasoning: bool,
    missing_message: bool,
) -> String {
    let output = wire["output"].as_array_mut().unwrap();
    if missing_message {
        output.retain(|item| item.get("type").and_then(Value::as_str) != Some("message"));
    } else {
        for item in output.iter_mut() {
            if item.get("type").and_then(Value::as_str) == Some("message") {
                item["content"] =
                    json!([{"type":"output_text","text":"streamed message done","annotations":[]}]);
            }
        }
    }
    if empty_reasoning {
        output.insert(0, json!({"type":"reasoning","id":"r1","summary":[],"encrypted_content":"reasoning-cipher"}));
    }
    [
        json!({"type":"response.reasoning_summary_text.delta","sequence_number":1,
            "item_id":"r1","output_index":0,"summary_index":0,"delta":"streamed summary; "}),
        json!({"type":"response.reasoning_text.delta","sequence_number":2,
            "item_id":"r1","output_index":0,"content_index":0,"delta":"streamed reasoning"}),
        json!({"type":"response.output_text.delta","sequence_number":3,
            "item_id":"m1","output_index":2,"content_index":0,"delta":"streamed message"}),
        json!({"type":"response.completed","sequence_number":4,"response":wire}),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect::<String>()
        + "data: [DONE]\n\n"
}

/// 消费真实 HTTP 客户端及 L2，保留显示通道以核对推理未因历史保护而丢失。
async fn collect_with_display(
    client: &SamplingClient,
    request: ConversationRequest,
) -> (
    xai_grok_sampling_types::ConversationResponse,
    String,
    String,
) {
    use crate::events::{SamplingChannel, SamplingEvent};
    let (raw, metadata, doom_loop) = client.conversation_stream_responses(request).await.unwrap();
    let mut events = crate::stream::stream_responses(
        raw,
        metadata,
        crate::types::RequestId::random(),
        std::time::Duration::from_secs(30),
        doom_loop,
    )
    .boxed();
    let mut reasoning = String::new();
    let mut text = String::new();
    while let Some(event) = events.next().await {
        match event {
            SamplingEvent::ChannelToken {
                channel: SamplingChannel::Reasoning,
                text: delta,
                ..
            } => reasoning.push_str(&delta),
            SamplingEvent::ChannelToken {
                channel: SamplingChannel::Text,
                text: delta,
                ..
            } => text.push_str(&delta),
            SamplingEvent::Completed { response, .. } => return (*response, reasoning, text),
            SamplingEvent::Failed { error, .. } => panic!("流式响应失败：{error:?}"),
            _ => {}
        }
    }
    panic!("缺少完成事件");
}

/// 原生签发后不补写推理 sibling 或正文，显示仍保留，下一轮继续采用新压缩点。
#[tokio::test]
async fn responses_native_reasoning_delta_preserves_authoritative_checkpoint_on_next_turn() {
    for empty_reasoning in [false, true] {
        for missing_message in [false, true] {
            let reply =
                reasoning_fallback_reply(response(true, true), empty_reasoning, missing_message);
            let (url, captured, server) = serve_reply(true, reply).await;
            let client = SamplingClient::new(config(url)).unwrap();
            let original = compressed_history(&client);
            let (completed, shown_reasoning, shown_text) =
                collect_with_display(&client, original.clone()).await;
            assert_eq!(shown_reasoning, "streamed summary; streamed reasoning");
            assert!(shown_text.contains("streamed message"));
            assert!(response_context_usage(&completed).is_some());
            let reasoning: Vec<_> = completed
                .items
                .iter()
                .filter_map(|item| match item {
                    ConversationItem::Reasoning(reasoning) => Some(reasoning),
                    _ => None,
                })
                .collect();
            assert_eq!(reasoning.len(), usize::from(empty_reasoning));
            assert!(reasoning.iter().all(|item| item.summary.is_empty()));
            let assistant = completed.assistant().unwrap();
            assert_eq!(
                assistant.content.as_ref(),
                if missing_message {
                    ""
                } else {
                    "streamed message done"
                }
            );
            let expected_checkpoint = assistant
                .responses_output
                .as_ref()
                .unwrap()
                .first()
                .unwrap()
                .get(RESPONSE_CONTEXT_CHECKPOINT_FIELD)
                .unwrap()
                .get("checkpoint_key")
                .and_then(Value::as_str)
                .unwrap()
                .to_owned();
            let mut next = original;
            next.items.extend(completed.items);
            next.items.extend(history("next turn").items);
            let source = assistant_source(&next);
            let projection = project_responses_input(&next, Some(&source));
            assert_eq!(
                projection.checkpoint_key.as_deref(),
                Some(expected_checkpoint.as_str())
            );
            assert!(
                !Value::Array(projection.input)
                    .to_string()
                    .contains("archive")
            );
            client.conversation_collect(next).await.unwrap();
            let requests = captured.lock().unwrap();
            assert_eq!(requests.len(), 2);
            let body = requests.last().unwrap();
            assert!(!body["input"].to_string().contains("archive"));
            assert!(body["input"].to_string().contains("opaque"));
            server.abort();
        }
    }
}

/// 未签发原生记录时保留旧推理补写，缺失项与空 summary 均可恢复流式推理。
#[tokio::test]
async fn responses_legacy_reasoning_delta_still_populates_missing_summary() {
    for empty_reasoning in [false, true] {
        for missing_message in [false, true] {
            let reply =
                reasoning_fallback_reply(response(false, true), empty_reasoning, missing_message);
            let (url, _, server) = serve_reply(true, reply).await;
            let client = SamplingClient::new(crate::config::SamplerConfig {
                responses_compact_threshold: None,
                ..config(url)
            })
            .unwrap();
            let (completed, shown_reasoning, shown_text) =
                collect_with_display(&client, history("hello")).await;
            assert_eq!(shown_reasoning, "streamed summary; streamed reasoning");
            assert!(shown_text.contains("streamed message"));
            assert!(response_context_usage(&completed).is_none());
            let reasoning: Vec<_> = completed
                .items
                .iter()
                .filter_map(|item| match item {
                    ConversationItem::Reasoning(reasoning) => Some(reasoning),
                    _ => None,
                })
                .collect();
            assert_eq!(reasoning.len(), 1);
            assert_eq!(reasoning.first().unwrap().summary.len(), 1);
            assert!(
                reasoning
                    .first()
                    .unwrap()
                    .summary
                    .iter()
                    .any(|part| match part {
                        rs::SummaryPart::SummaryText(summary) =>
                            summary.text == "streamed reasoning",
                    })
            );
            assert_eq!(
                completed.assistant().unwrap().content.as_ref(),
                if missing_message {
                    ""
                } else {
                    "streamed message done"
                }
            );
            server.abort();
        }
    }
}
