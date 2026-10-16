//! Layer-2 stream transform for the OpenAI Responses API.
//!
//! Consumes a raw `rs::ResponseStreamEvent` stream and produces [`SamplingEvent`]s.
//! Pure: no I/O, no shell coupling.

use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::{BoxStream, Stream};

use xai_grok_sampling_types::{
    ConversationItem, ConversationResponse, ResponseModelMetadata, SamplingError, StopReason,
    TokenUsage, messages as messages_types, rs,
};

use crate::doom_loop_recovery::FailedResponseCapture;
use crate::events::{SamplingChannel, SamplingErrorInfo, SamplingEvent};
use crate::metrics::InferenceLatencyStats;
use crate::types::RequestId;

/// Wire values of `incomplete_details.reason` on an `Incomplete` response.
/// The xAI server emits the three `max_*` values; `content_filter` is OpenAI vocabulary, kept for spec compatibility.
const INCOMPLETE_REASON_CONTENT_FILTER: &str = "content_filter";
const INCOMPLETE_REASON_MAX_OUTPUT_TOKENS: &str = "max_output_tokens";
/// The model's context window was exhausted mid-generation (xAI extension).
const INCOMPLETE_REASON_MAX_PROMPT_TOKENS: &str = "max_prompt_tokens";
/// A server-side time limit cut generation short (xAI extension).
const INCOMPLETE_REASON_MAX_TIME_LIMIT: &str = "max_time_limit";

/// Returns whether a Responses API event reflects real model progress rather than a liveness-only heartbeat or status transition.
pub(crate) fn responses_event_has_meaningful_content(event: &rs::ResponseStreamEvent) -> bool {
    use rs::ResponseStreamEvent;

    match event {
        ResponseStreamEvent::ResponseCreated(_)
        | ResponseStreamEvent::ResponseInProgress(_)
        | ResponseStreamEvent::ResponseQueued(_) => false,
        ResponseStreamEvent::ResponseOutputTextDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseOutputTextDone(event) => !event.text.is_empty(),
        ResponseStreamEvent::ResponseRefusalDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseRefusalDone(event) => !event.refusal.is_empty(),
        ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseFunctionCallArgumentsDone(event) => {
            !event.arguments.is_empty() || event.name.as_ref().is_some_and(|name| !name.is_empty())
        }
        ResponseStreamEvent::ResponseReasoningSummaryTextDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseReasoningSummaryTextDone(event) => !event.text.is_empty(),
        ResponseStreamEvent::ResponseReasoningTextDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseReasoningTextDone(event) => !event.text.is_empty(),
        ResponseStreamEvent::ResponseMCPCallArgumentsDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseMCPCallArgumentsDone(event) => !event.arguments.is_empty(),
        ResponseStreamEvent::ResponseCodeInterpreterCallCodeDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseCodeInterpreterCallCodeDone(event) => !event.code.is_empty(),
        ResponseStreamEvent::ResponseCustomToolCallInputDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseCustomToolCallInputDone(event) => !event.input.is_empty(),
        ResponseStreamEvent::ResponseFailed(event) => {
            !event.response.output.is_empty()
                || event
                    .response
                    .usage
                    .as_ref()
                    .is_some_and(|usage| usage.output_tokens > 0)
        }
        ResponseStreamEvent::ResponseCompleted(_)
        | ResponseStreamEvent::ResponseIncomplete(_)
        | ResponseStreamEvent::ResponseOutputItemAdded(_)
        | ResponseStreamEvent::ResponseOutputItemDone(_)
        | ResponseStreamEvent::ResponseContentPartAdded(_)
        | ResponseStreamEvent::ResponseContentPartDone(_)
        | ResponseStreamEvent::ResponseFileSearchCallInProgress(_)
        | ResponseStreamEvent::ResponseFileSearchCallSearching(_)
        | ResponseStreamEvent::ResponseFileSearchCallCompleted(_)
        | ResponseStreamEvent::ResponseWebSearchCallInProgress(_)
        | ResponseStreamEvent::ResponseWebSearchCallSearching(_)
        | ResponseStreamEvent::ResponseWebSearchCallCompleted(_)
        | ResponseStreamEvent::ResponseReasoningSummaryPartAdded(_)
        | ResponseStreamEvent::ResponseReasoningSummaryPartDone(_)
        | ResponseStreamEvent::ResponseImageGenerationCallCompleted(_)
        | ResponseStreamEvent::ResponseImageGenerationCallGenerating(_)
        | ResponseStreamEvent::ResponseImageGenerationCallInProgress(_)
        | ResponseStreamEvent::ResponseImageGenerationCallPartialImage(_)
        | ResponseStreamEvent::ResponseMCPCallCompleted(_)
        | ResponseStreamEvent::ResponseMCPCallFailed(_)
        | ResponseStreamEvent::ResponseMCPCallInProgress(_)
        | ResponseStreamEvent::ResponseMCPListToolsCompleted(_)
        | ResponseStreamEvent::ResponseMCPListToolsFailed(_)
        | ResponseStreamEvent::ResponseMCPListToolsInProgress(_)
        | ResponseStreamEvent::ResponseCodeInterpreterCallInProgress(_)
        | ResponseStreamEvent::ResponseCodeInterpreterCallInterpreting(_)
        | ResponseStreamEvent::ResponseCodeInterpreterCallCompleted(_)
        | ResponseStreamEvent::ResponseOutputTextAnnotationAdded(_)
        | ResponseStreamEvent::ResponseError(_) => true,
    }
}

pub(crate) fn responses_event_may_have_output(event: &rs::ResponseStreamEvent) -> bool {
    !matches!(event, rs::ResponseStreamEvent::ResponseError(_))
        && responses_event_has_meaningful_content(event)
}

/// Copy everything the Doom-loop capture needs out of a frame.
/// Any frame that names tool activity or compaction state vetoes the replay, since reasoning must never be retried without the item it is bound to.
fn observe_for_recovery(capture: &FailedResponseCapture, event: &rs::ResponseStreamEvent) {
    use rs::ResponseStreamEvent as Event;
    if !capture.is_armed() {
        return;
    }
    match event {
        Event::ResponseOutputTextDelta(text) => capture.record_output_delta(
            text.output_index,
            text.content_index,
            text.item_id.clone(),
            &text.delta,
        ),
        Event::ResponseOutputTextDone(text) => capture.record_output_done(
            text.output_index,
            text.content_index,
            text.item_id.clone(),
            text.text.clone(),
        ),
        Event::ResponseReasoningTextDelta(reasoning) => capture.record_reasoning_delta(
            reasoning.output_index,
            reasoning.content_index,
            reasoning.item_id.clone(),
            &reasoning.delta,
        ),
        Event::ResponseReasoningTextDone(reasoning) => capture.record_reasoning_done(
            reasoning.output_index,
            reasoning.content_index,
            reasoning.item_id.clone(),
            reasoning.text.clone(),
        ),
        Event::ResponseReasoningSummaryTextDelta(summary) => capture
            .record_reasoning_summary_delta(
                summary.output_index,
                summary.summary_index,
                summary.item_id.clone(),
                &summary.delta,
            ),
        Event::ResponseReasoningSummaryTextDone(summary) => capture.record_reasoning_summary_done(
            summary.output_index,
            summary.summary_index,
            summary.item_id.clone(),
            summary.text.clone(),
        ),
        Event::ResponseOutputItemAdded(added) => capture.record_item_start(&added.item),
        Event::ResponseOutputItemDone(done) => {
            capture.record_output_item(done.output_index, &done.item);
        }
        Event::ResponseCompleted(completed) => {
            capture.record_terminal_output(&completed.response.output);
        }
        Event::ResponseIncomplete(incomplete) => {
            capture.record_terminal_output(&incomplete.response.output);
        }
        // Frames that only name in-flight tool work
        // The item they belong to may never complete on this attempt, so the frame itself is the notice that a call was in flight
        Event::ResponseFunctionCallArgumentsDelta(_)
        | Event::ResponseFunctionCallArgumentsDone(_)
        | Event::ResponseCustomToolCallInputDelta(_)
        | Event::ResponseCustomToolCallInputDone(_)
        | Event::ResponseCodeInterpreterCallCodeDelta(_)
        | Event::ResponseCodeInterpreterCallCodeDone(_)
        | Event::ResponseCodeInterpreterCallInProgress(_)
        | Event::ResponseCodeInterpreterCallInterpreting(_)
        | Event::ResponseCodeInterpreterCallCompleted(_)
        | Event::ResponseFileSearchCallInProgress(_)
        | Event::ResponseFileSearchCallSearching(_)
        | Event::ResponseFileSearchCallCompleted(_)
        | Event::ResponseWebSearchCallInProgress(_)
        | Event::ResponseWebSearchCallSearching(_)
        | Event::ResponseWebSearchCallCompleted(_)
        | Event::ResponseImageGenerationCallInProgress(_)
        | Event::ResponseImageGenerationCallGenerating(_)
        | Event::ResponseImageGenerationCallCompleted(_)
        | Event::ResponseMCPCallInProgress(_)
        | Event::ResponseMCPCallCompleted(_)
        | Event::ResponseMCPCallFailed(_)
        | Event::ResponseMCPCallArgumentsDelta(_)
        | Event::ResponseMCPCallArgumentsDone(_) => capture.record_unreplayable(),
        _ => {}
    }
}

/// 只追加完整快照尚未显示的后缀；冲突不能用追加修正。
fn reconcile_snapshot(current: &mut String, snapshot: &str) -> String {
    if let Some(suffix) = snapshot.strip_prefix(current.as_str()) {
        let suffix = suffix.to_owned();
        *current = snapshot.to_owned();
        suffix
    } else {
        tracing::warn!("Responses 完整内容与已显示片段不一致，保留最终响应作为历史");
        String::new()
    }
}

/// 每个输出项独立保存正文和参数，done 快照不重复发送。
#[derive(Default)]
struct ContentState {
    text: BTreeMap<(u32, u32), String>,
    item_indices: BTreeMap<String, u32>,
    tools: BTreeMap<u32, ToolState>,
    next_tool_index: u32,
}

#[derive(Default)]
struct ToolState {
    index: Option<u32>,
    id: String,
    name: String,
    arguments: String,
    emitted_arguments: String,
}

impl ContentState {
    /// 无 added 时先缓存参数，获得调用身份后再发送。
    fn tool_update(
        &mut self,
        output_index: u32,
        identity: Option<(&str, &str)>,
        arguments: Option<(&str, bool)>,
        request_id: &RequestId,
    ) -> Vec<SamplingEvent> {
        let tool = self.tools.entry(output_index).or_default();
        let mut events = Vec::new();
        if let Some((id, name)) = identity {
            if !id.is_empty() && tool.id.is_empty() {
                tool.id = id.to_owned();
            }
            if !name.is_empty() && tool.name.is_empty() {
                tool.name = name.to_owned();
            }
        }
        if let Some((arguments, complete)) = arguments {
            if complete {
                tool.arguments = arguments.to_owned();
            } else {
                tool.arguments.push_str(arguments);
            }
        }
        if tool.index.is_none() && !tool.id.is_empty() && !tool.name.is_empty() {
            let index = self.next_tool_index;
            self.next_tool_index += 1;
            tool.index = Some(index);
            events.push(SamplingEvent::ToolCallDelta {
                request_id: request_id.clone(),
                tool_index: index,
                id: Some(tool.id.clone()),
                name: Some(tool.name.clone()),
                arguments_delta: None,
            });
        }
        if let Some(index) = tool.index {
            let suffix = reconcile_snapshot(&mut tool.emitted_arguments, &tool.arguments);
            if !suffix.is_empty() {
                events.push(SamplingEvent::ToolCallDelta {
                    request_id: request_id.clone(),
                    tool_index: index,
                    id: None,
                    name: None,
                    arguments_delta: Some(suffix),
                });
            }
        }
        events
    }

    /// 消费消息及函数的完整快照，不重复创建客户端工具调用。
    fn item_snapshot(
        &mut self,
        output_index: u32,
        item: &rs::OutputItem,
        request_id: &RequestId,
        text: &mut Vec<String>,
    ) -> Vec<SamplingEvent> {
        let id = match item {
            rs::OutputItem::Message(message) => Some(message.id.as_str()),
            rs::OutputItem::FunctionCall(call) => {
                if let Some(index) = call
                    .id
                    .as_ref()
                    .and_then(|id| self.item_indices.get(id))
                    .copied()
                {
                    self.item_indices
                        .entry(call.call_id.clone())
                        .or_insert(index);
                }
                Some(call.call_id.as_str())
            }
            _ => None,
        };
        let output_index = id
            .map(|id| {
                *self
                    .item_indices
                    .entry(id.to_owned())
                    .or_insert(output_index)
            })
            .unwrap_or(output_index);
        match item {
            rs::OutputItem::Message(message) => {
                for (index, part) in message.content.iter().enumerate() {
                    let snapshot = match part {
                        rs::OutputMessageContent::OutputText(value) => &value.text,
                        rs::OutputMessageContent::Refusal(value) => &value.refusal,
                    };
                    text.push(reconcile_snapshot(
                        self.text.entry((output_index, index as u32)).or_default(),
                        snapshot,
                    ));
                }
                Vec::new()
            }
            rs::OutputItem::FunctionCall(call) => self.tool_update(
                output_index,
                Some((&call.call_id, &call.name)),
                Some((&call.arguments, true)),
                request_id,
            ),
            _ => Vec::new(),
        }
    }
}

/// 仅已有 X 搜索子工具属于服务端托管调用。
fn is_x_search(name: &str) -> bool {
    matches!(
        name,
        "x_search" | "x_keyword_search" | "x_semantic_search" | "x_user_search" | "x_thread_fetch"
    )
}

/// 服务端结果只显示固定摘要，不转发可能包含二进制的原始载荷。
fn hosted_result_summary(item: &rs::OutputItem) -> Option<(&str, &'static str, &'static str)> {
    match item {
        rs::OutputItem::FileSearchCall(call) => Some((
            &call.id,
            "file_search",
            "服务端文件搜索已返回；详细结果保留在响应历史中。",
        )),
        rs::OutputItem::McpCall(call) => Some((
            &call.id,
            "mcp_call",
            "服务端 MCP 调用已返回；详细结果保留在响应历史中。",
        )),
        rs::OutputItem::ImageGenerationCall(call) => Some((
            &call.id,
            "image_generation",
            "服务端图像生成已返回；当前客户端不展示图像载荷。",
        )),
        _ => None,
    }
}

/// 仅识别未展示的内容类型，不把提示写入原始响应。
fn unsupported_output_notice(response: &rs::Response) -> Option<&'static str> {
    let raw = response
        .metadata
        .as_ref()?
        .get(xai_grok_sampling_types::RESPONSE_RAW_OUTPUT_METADATA_KEY)?;
    let output: Vec<serde_json::Value> = serde_json::from_str(raw).ok()?;
    let mut audio = false;
    let mut unknown = false;
    for item in &output {
        let kind = item
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if kind == "message" {
            for part in item
                .get("content")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
            {
                match part
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                {
                    "output_text" | "refusal" => {}
                    "audio" | "output_audio" => audio = true,
                    _ => unknown = true,
                }
            }
        } else if matches!(kind, "audio" | "output_audio") {
            audio = true;
        } else if !matches!(
            kind,
            "reasoning"
                | "compaction"
                | "function_call"
                | "web_search_call"
                | "custom_tool_call"
                | "code_interpreter_call"
                | "file_search_call"
                | "mcp_call"
                | "image_generation_call"
        ) {
            unknown = true;
        }
    }
    if audio {
        Some("[客户端能力提示] 上游返回了音频内容，当前客户端暂不支持播放或展示；原始响应已保留。")
    } else if unknown {
        Some("[客户端能力提示] 上游返回了当前客户端不支持展示的内容类型；原始响应已保留。")
    } else {
        None
    }
}

/// Transform a raw Responses API event stream into a stream of [`SamplingEvent`]s.
/// `None` (check disabled) leaves the response untouched.
pub fn stream_responses<'a>(
    raw_stream: BoxStream<'a, Result<rs::ResponseStreamEvent, SamplingError>>,
    model_metadata: Option<ResponseModelMetadata>,
    request_id: RequestId,
    idle_timeout: Duration,
    doom_loop: Option<crate::doom_loop::DoomLoopSignalCollector>,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    stream_responses_tracked(
        raw_stream,
        model_metadata,
        request_id,
        idle_timeout,
        doom_loop,
        Arc::new(AtomicBool::new(false)),
        FailedResponseCapture::default(),
    )
}

pub(crate) fn stream_responses_tracked<'a>(
    raw_stream: BoxStream<'a, Result<rs::ResponseStreamEvent, SamplingError>>,
    model_metadata: Option<ResponseModelMetadata>,
    request_id: RequestId,
    idle_timeout: Duration,
    doom_loop: Option<crate::doom_loop::DoomLoopSignalCollector>,
    output_observed: Arc<AtomicBool>,
    failed_response: FailedResponseCapture,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    async_stream::stream! {
        use rs::{ResponseStreamEvent, Status};

        let decode_region = crate::span_timing::Region::from_span(tracing::info_span!(
            "sampling.stream_decode",
            ttft_ms = tracing::field::Empty,
            ttlb_ms = tracing::field::Empty,
            output_tokens = tracing::field::Empty,
            chunk_count = tracing::field::Empty,
        ));
        let stream_start = Instant::now();
        let mut chunk_timestamps: Vec<Instant> = Vec::new();

        yield SamplingEvent::StreamStarted {
            request_id: request_id.clone(),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        if let Some(metadata) = model_metadata {
            yield SamplingEvent::ModelMetadata {
                request_id: request_id.clone(),
                metadata,
            };
        }

        let mut final_response: Option<rs::Response> = None;
        let mut chunk_index: u64 = 0;
        let mut message_chunk_count: u64 = 0;
        let mut first_token_emitted = false;
        let mut reasoning_acc = String::new();
        let mut last_content_chunk_at = Instant::now();

        let mut content_state = ContentState::default();
        let mut x_search_items = std::collections::BTreeSet::new();
        let mut x_search_started = std::collections::BTreeSet::new();
        let mut hosted_results_shown = std::collections::BTreeSet::new();

        let mut stream = raw_stream;
        loop {
            let event_result = match tokio::time::timeout(idle_timeout, stream.next()).await {
                Ok(Some(event_result)) => event_result,
                Ok(None) => break,
                Err(_elapsed) => {
                    let err = SamplingError::IdleTimeout {
                        elapsed_secs: idle_timeout.as_secs(),
                    };
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }
            };

            let event = match event_result {
                Ok(event) => event,
                Err(err) => {
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }
            };

            if responses_event_may_have_output(&event) {
                output_observed.store(true, Ordering::Relaxed);
            }

            // A confident midstream signal aborts the attempt immediately.
            // Terminal frames are processed so their complete response items remain available to the retry loop
            // `drive_l2` rejects the completed response before it can be accepted
            let is_terminal_response = matches!(
                &event,
                ResponseStreamEvent::ResponseCompleted(_)
                    | ResponseStreamEvent::ResponseIncomplete(_)
            );
            // Observation happens before the abort gate so the aborting frame lands in the capture like any other
            // The attempt is discarded either way, so nothing here reaches downstream consumers
            observe_for_recovery(&failed_response, &event);

            if !is_terminal_response
                && let Some(triggers) = doom_loop.as_ref().and_then(|c| c.abort_triggers())
            {
                let all_triggers = doom_loop
                    .as_ref()
                    .map(|collector| {
                        collector
                            .take()
                            .into_iter()
                            .map(|signal| signal.raw)
                            .collect()
                    })
                    .unwrap_or_default();
                yield SamplingEvent::DoomLoopSignals {
                    request_id: request_id.clone(),
                    triggers: all_triggers,
                };
                let err = SamplingError::DoomLoopDetected {
                    triggers,
                    aborted_at_chunk: Some(chunk_index),
                };
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }

            let event_has_content = responses_event_has_meaningful_content(&event);

            // Track whether ResponseIncomplete should break the loop after the content-aware idle check below
            let mut should_break = false;

            let hosted_items: &[rs::OutputItem] = match &event {
                ResponseStreamEvent::ResponseOutputItemDone(ev) => std::slice::from_ref(&ev.item),
                ResponseStreamEvent::ResponseCompleted(ev) => &ev.response.output,
                ResponseStreamEvent::ResponseIncomplete(ev) => &ev.response.output,
                _ => &[],
            };
            for item in hosted_items {
                if let Some((id, name, summary)) = hosted_result_summary(item)
                    && hosted_results_shown.insert((name.to_owned(), id.to_owned()))
                {
                    yield SamplingEvent::BackendToolCallStarted {
                        request_id: request_id.clone(), call_id: id.to_owned(), name: name.to_owned(),
                    };
                    yield SamplingEvent::BackendToolCallCompleted {
                        request_id: request_id.clone(), call_id: id.to_owned(), name: name.to_owned(),
                        result: Some(serde_json::Value::String(summary.to_owned())),
                    };
                }
            }

            let mut text_updates = Vec::new();
            let tool_updates = match &event {
                ResponseStreamEvent::ResponseOutputTextDelta(ev) => {
                    content_state.item_indices.entry(ev.item_id.clone()).or_insert(ev.output_index);
                    content_state.text.entry((ev.output_index, ev.content_index)).or_default().push_str(&ev.delta);
                    text_updates.push(ev.delta.clone());
                    Vec::new()
                }
                ResponseStreamEvent::ResponseRefusalDelta(ev) => {
                    content_state.item_indices.entry(ev.item_id.clone()).or_insert(ev.output_index);
                    content_state.text.entry((ev.output_index, ev.content_index)).or_default().push_str(&ev.delta);
                    text_updates.push(ev.delta.clone());
                    Vec::new()
                }
                ResponseStreamEvent::ResponseOutputTextDone(ev) => {
                    content_state.item_indices.entry(ev.item_id.clone()).or_insert(ev.output_index);
                    text_updates.push(reconcile_snapshot(content_state.text.entry((ev.output_index, ev.content_index)).or_default(), &ev.text));
                    Vec::new()
                }
                ResponseStreamEvent::ResponseRefusalDone(ev) => {
                    content_state.item_indices.entry(ev.item_id.clone()).or_insert(ev.output_index);
                    text_updates.push(reconcile_snapshot(content_state.text.entry((ev.output_index, ev.content_index)).or_default(), &ev.refusal));
                    Vec::new()
                }
                ResponseStreamEvent::ResponseOutputItemAdded(ev) => {
                    if let rs::OutputItem::CustomToolCall(call) = &ev.item
                        && is_x_search(&call.name)
                    {
                        x_search_items.insert(call.id.clone());
                    }
                    if let rs::OutputItem::FunctionCall(call) = &ev.item {
                        content_state.item_indices.entry(call.call_id.clone()).or_insert(ev.output_index);
                        content_state.tool_update(ev.output_index, Some((&call.call_id, &call.name)),
                            (!call.arguments.is_empty()).then_some((call.arguments.as_str(), true)), &request_id)
                    } else {
                        content_state.item_snapshot(ev.output_index, &ev.item, &request_id, &mut text_updates)
                    }
                }
                ResponseStreamEvent::ResponseOutputItemDone(ev) => {
                    content_state.item_snapshot(ev.output_index, &ev.item, &request_id, &mut text_updates)
                }
                ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(ev) => {
                    content_state.item_indices.entry(ev.item_id.clone()).or_insert(ev.output_index);
                    content_state.tool_update(ev.output_index, None, Some((&ev.delta, false)), &request_id)
                }
                ResponseStreamEvent::ResponseFunctionCallArgumentsDone(ev) => {
                    content_state.item_indices.entry(ev.item_id.clone()).or_insert(ev.output_index);
                    content_state.tool_update(ev.output_index, ev.name.as_deref().map(|name| ("", name)), Some((&ev.arguments, true)), &request_id)
                }
                ResponseStreamEvent::ResponseCompleted(ev) => {
                    let mut updates = Vec::new();
                    for (index, item) in ev.response.output.iter().enumerate() {
                        updates.extend(content_state.item_snapshot(index as u32, item, &request_id, &mut text_updates));
                    }
                    updates
                }
                ResponseStreamEvent::ResponseIncomplete(ev) => {
                    let mut updates = Vec::new();
                    for (index, item) in ev.response.output.iter().enumerate() {
                        updates.extend(content_state.item_snapshot(index as u32, item, &request_id, &mut text_updates));
                    }
                    updates
                }
                _ => Vec::new(),
            };
            for text in text_updates.into_iter().filter(|text| !text.is_empty()) {
                if !first_token_emitted {
                    first_token_emitted = true;
                    yield SamplingEvent::FirstToken { request_id: request_id.clone() };
                }
                chunk_timestamps.push(Instant::now());
                chunk_index += 1;
                message_chunk_count += 1;
                yield SamplingEvent::ChannelToken {
                    request_id: request_id.clone(), channel: SamplingChannel::Text, text, chunk_index,
                };
            }
            for update in tool_updates {
                yield update;
            }

            match event {
                ResponseStreamEvent::ResponseReasoningSummaryTextDelta(summary_event) => {
                    let delta = summary_event.delta;
                    if !delta.is_empty() {
                        if !first_token_emitted {
                            first_token_emitted = true;
                            yield SamplingEvent::FirstToken {
                                request_id: request_id.clone(),
                            };
                        }
                        chunk_index += 1;
                        yield SamplingEvent::ChannelToken {
                            request_id: request_id.clone(),
                            channel: SamplingChannel::Reasoning,
                            text: delta,
                            chunk_index,
                        };
                    }
                }

                ResponseStreamEvent::ResponseReasoningTextDelta(reasoning_event) => {
                    let delta = reasoning_event.delta;
                    if !delta.is_empty() {
                        if !first_token_emitted {
                            first_token_emitted = true;
                            yield SamplingEvent::FirstToken {
                                request_id: request_id.clone(),
                            };
                        }
                        chunk_index += 1;
                        reasoning_acc.push_str(&delta);
                        yield SamplingEvent::ChannelToken {
                            request_id: request_id.clone(),
                            channel: SamplingChannel::Reasoning,
                            text: delta,
                            chunk_index,
                        };
                    }
                }

                ResponseStreamEvent::ResponseCompleted(completed_event) => {
                    final_response = Some(completed_event.response);
                    should_break = true;
                }

                ResponseStreamEvent::ResponseIncomplete(incomplete_event) => {
                    final_response = Some(incomplete_event.response);
                    should_break = true;
                }

                ResponseStreamEvent::ResponseFailed(failed_event) => {
                    let response = failed_event.response;
                    let error_message = response
                        .error
                        .as_ref()
                        .map(|e| format!("{}: {}", e.code, e.message))
                        .unwrap_or_else(|| "Response failed with unknown error".to_string());
                    let err = SamplingError::Api {
                        status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                        message: error_message,
                        model_metadata: None,
                        retry_after_secs: None,
                        should_retry: None,
                        error_code: response
                            .error
                            .as_ref()
                            .map(|e| xai_grok_sampling_types::ApiErrorCode::parse(&e.code)),
                    };
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }

                ResponseStreamEvent::ResponseError(error_event) => {
                    let error_message = format!(
                        "{}: {}",
                        error_event.code.as_deref().unwrap_or("error"),
                        error_event.message
                    );
                    let err = SamplingError::Api {
                        status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                        message: error_message,
                        model_metadata: None,
                        retry_after_secs: None,
                        should_retry: None,
                        // The wire code, absent when the event carried none.
                        error_code: error_event
                            .code
                            .as_deref()
                            .map(xai_grok_sampling_types::ApiErrorCode::parse),
                    };
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }

                // ── Backend-hosted tool lifecycle events ────────────
                // These tools are executed server-side by the agentic sampler
                // We emit progress events so the shell/pager can show status to the user

                // Web search
                ResponseStreamEvent::ResponseWebSearchCallInProgress(ev) => {
                    yield SamplingEvent::BackendToolCallStarted {
                        request_id: request_id.clone(),
                        call_id: ev.item_id.clone(),
                        name: "web_search".to_string(),
                    };
                }
                // Completed/Searching carry no data; the real payload arrives via ResponseOutputItemDone(WebSearchCall) below
                ResponseStreamEvent::ResponseWebSearchCallCompleted(_)
                | ResponseStreamEvent::ResponseWebSearchCallSearching(_) => {}

                // Code interpreter runs server-side, like web/x search
                // The shell renders that as a client `tool_use` and `user` `tool_result` split grok has no HostedTool::CodeInterpreter, so these events never arrive under the current hosted-tool set
                // The started event fires on InProgress; the full payload (code and outputs) rides ResponseOutputItemDone(CodeInterpreterCall) below
                ResponseStreamEvent::ResponseCodeInterpreterCallInProgress(ev) => {
                    yield SamplingEvent::BackendToolCallStarted {
                        request_id: request_id.clone(),
                        call_id: ev.item_id.clone(),
                        name: "code_interpreter".to_string(),
                    };
                }
                // Interpreting/Completed carry no payload; the result arrives via ResponseOutputItemDone(CodeInterpreterCall) below
                ResponseStreamEvent::ResponseCodeInterpreterCallInterpreting(_)
                | ResponseStreamEvent::ResponseCodeInterpreterCallCompleted(_) => {}

                // OutputItemDone carries the full result for backend tools.
                // For WebSearchCall this includes the query and source URLs.
                // For CustomToolCall this includes x_search results.
                ResponseStreamEvent::ResponseOutputItemDone(done_event) => {
                    match &done_event.item {
                        rs::OutputItem::WebSearchCall(ws) => {
                            let result = serde_json::to_value(ws).ok();
                            yield SamplingEvent::BackendToolCallCompleted {
                                request_id: request_id.clone(),
                                call_id: ws.id.clone(),
                                name: "web_search".to_string(),
                                result,
                            };
                        }
                        // X search results arrive as CustomToolCall with names like x_keyword_search, x_semantic_search, etc
                        // Use "x_search" consistently (matching the Started event)
                        // The specific sub-type is in the serialized result payload and extracted by the pager from raw_output.name
                        rs::OutputItem::CustomToolCall(ct) if is_x_search(&ct.name) => {
                            if x_search_started.insert(ct.id.clone()) {
                                yield SamplingEvent::BackendToolCallStarted {
                                    request_id: request_id.clone(), call_id: ct.id.clone(), name: "x_search".to_string(),
                                };
                            }
                            let result = serde_json::to_value(ct).ok();
                            yield SamplingEvent::BackendToolCallCompleted {
                                request_id: request_id.clone(),
                                call_id: ct.id.clone(),
                                name: "x_search".to_string(),
                                result,
                            };
                        }
                        // Code interpreter: the full call (code and outputs) rides the done item
                        // The completed event uses the shared "code_interpreter" name (matching the Started event)
                        rs::OutputItem::CodeInterpreterCall(ci) => {
                            let result = serde_json::to_value(ci).ok();
                            yield SamplingEvent::BackendToolCallCompleted {
                                request_id: request_id.clone(),
                                call_id: ci.id.clone(),
                                name: "code_interpreter".to_string(),
                                result,
                            };
                        }
                        _ => {}
                    }
                }

                // 只有已确认名称的托管 X 工具才发送开始事件。
                ResponseStreamEvent::ResponseCustomToolCallInputDone(ev)
                    if x_search_items.contains(&ev.item_id) && x_search_started.insert(ev.item_id.clone()) => {
                    yield SamplingEvent::BackendToolCallStarted {
                        request_id: request_id.clone(),
                        call_id: ev.item_id.clone(),
                        name: "x_search".to_string(),
                    };
                }

                // All other events (intermediate progress, annotations, image gen, file search, etc.) need no action
                _ => {}
            }

            if event_has_content {
                last_content_chunk_at = Instant::now();
            } else if last_content_chunk_at.elapsed() > idle_timeout {
                let err = SamplingError::IdleTimeout {
                    elapsed_secs: idle_timeout.as_secs(),
                };
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }

            if should_break {
                break;
            }
        }

        // ── Build the final response ─────────────────────────────────
        let mut response = match final_response {
            Some(r) => r,
            None => {
                let err = SamplingError::Api {
                    status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                    message: "No ResponseCompleted or ResponseIncomplete event received from \
                              Responses API"
                        .to_string(),
                    model_metadata: None,
                    retry_after_secs: None,
                    should_retry: None,
                    // Synthesized client-side; no wire envelope to read.
                    error_code: None,
                };
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }
        };

        // 能力提示只走显示通道；最终历史仍使用未经改写的响应。
        if message_chunk_count == 0
            && content_state.tools.is_empty()
            && hosted_results_shown.is_empty()
            && !response.output.iter().any(|item| matches!(item,
                rs::OutputItem::WebSearchCall(_) | rs::OutputItem::CustomToolCall(_) |
                rs::OutputItem::CodeInterpreterCall(_)))
            && let Some(notice) = unsupported_output_notice(&response)
        {
            if !first_token_emitted {
                yield SamplingEvent::FirstToken { request_id: request_id.clone() };
            }
            chunk_timestamps.push(Instant::now());
            chunk_index += 1;
            message_chunk_count += 1;
            yield SamplingEvent::ChannelToken {
                request_id: request_id.clone(), channel: SamplingChannel::Text,
                text: notice.to_owned(), chunk_index,
            };
        }

        // Billing fields (`prompt_tokens`, `completion_tokens`, `cached_prompt_tokens`, `reasoning_tokens`) are the cumulative wire values
        // The SSE decoder (`deserialize_response_event`) has already rewritten `u.total_tokens` to `context_details.input + output`
        let usage = response.usage.as_ref().map(|u| TokenUsage {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.total_tokens,
            reasoning_tokens: u.output_tokens_details.reasoning_tokens,
            cached_prompt_tokens: u.input_tokens_details.cached_tokens,
            cache_creation_prompt_tokens: 0,
        });

        let cost_usd_ticks = response
            .metadata
            .as_mut()
            .and_then(|m| m.remove(crate::client::COST_USD_TICKS_METADATA_KEY))
            .and_then(|s| s.parse::<i64>().ok());

        let status = response.status.clone();
        // Wire reason for an incomplete response (the `INCOMPLETE_REASON_*` values above)
        // It is captured before `response` is consumed below
        let incomplete_reason = response
            .incomplete_details
            .as_ref()
            .map(|d| d.reason.clone());

        // Convert to ConversationItem(s); patch in accumulated reasoning text as a fallback when the final response lacks `content` or `summary`
        // The streaming deltas may have arrived out of band
        // Splice policy lives in `inject_streaming_reasoning_fallback`.
        let mut items = xai_grok_sampling_types::response_to_conversation_items(response);
        xai_grok_sampling_types::inject_streaming_reasoning_fallback(&mut items, reasoning_acc);

        let has_tool_calls = items.iter().any(|i| match i {
            ConversationItem::Assistant(a) => !a.tool_calls.is_empty(),
            _ => false,
        });

        // The single classification of an Incomplete response: the collapsed [`StopReason`] plus the typed raw reason carried to consumers
        // The Responses wire strings never leave this module; the raw reason reuses the Messages wire strings so the shell speaks one vocabulary
        // Only the strings match: the xAI Messages backend itself reports a context cut as `max_tokens`, and only this mapping splits it
        let incomplete_classification: Option<(StopReason, Option<messages_types::StopReason>)> =
            if matches!(status, Status::Incomplete) {
                Some(match incomplete_reason.as_deref() {
                    // A moderation cut ("content_filter") maps to ContentFilter, not Length
                    // A filter-cut response must never be salvaged and continued by `LengthPolicy`
                    Some(INCOMPLETE_REASON_CONTENT_FILTER) => (StopReason::ContentFilter, None),
                    Some(INCOMPLETE_REASON_MAX_OUTPUT_TOKENS) => (
                        StopReason::Length,
                        Some(messages_types::StopReason::MaxTokens),
                    ),
                    Some(INCOMPLETE_REASON_MAX_PROMPT_TOKENS) => (
                        StopReason::Length,
                        Some(messages_types::StopReason::ModelContextWindowExceeded),
                    ),
                    // A time-limit cut is a Length cut with no Messages vocabulary word
                    // Log it because the truncation notice the user sees says "output limit"
                    Some(INCOMPLETE_REASON_MAX_TIME_LIMIT) => {
                        tracing::info!(
                            request_id = %request_id,
                            "response cut by the server-side time limit"
                        );
                        (StopReason::Length, None)
                    }
                    // An Incomplete response without a reason is a length cut with nothing to carry
                    None => (StopReason::Length, None),
                    Some(other) => {
                        tracing::warn!(
                            reason = %other,
                            "unknown incomplete reason; treating as Length"
                        );
                        (StopReason::Length, None)
                    }
                })
            } else {
                None
            };

        // NOTE: tool calls win even over an Incomplete status, the opposite precedence from the Messages backend
        // On the Messages backend Length wins, so the `LengthPolicy` gate can refuse a possibly argument-truncated trailing call
        // The difference is deliberate; don't "fix" it here
        let (stop_reason, raw_stop_reason) = if has_tool_calls {
            if matches!(incomplete_classification, Some((StopReason::Length, _))) {
                tracing::warn!(
                    request_id = %request_id,
                    "tool calls mask a length-truncated response; arguments may be truncated"
                );
            }
            // Keep the pair coherent: a tool-bearing turn reports ToolCalls with no raw length reason (the warn above is the truncation signal)
            // That preserves the headless output's `tool_use`
            (Some(StopReason::ToolCalls), None)
        } else {
            match status {
                Status::Completed => (Some(StopReason::Stop), None),
                Status::Incomplete => match incomplete_classification {
                    Some((stop, raw)) => (Some(stop), raw.map(|r| r.wire_str())),
                    None => (None, None),
                },
                _ => (None, None),
            }
        };

        let stream_end = Instant::now();
        let metrics =
            InferenceLatencyStats::from_timestamps(stream_start, &chunk_timestamps, stream_end);

        decode_region
            .span()
            .record("ttlb_ms", metrics.time_to_last_byte_ms as i64);
        decode_region
            .span()
            .record("chunk_count", metrics.chunk_count as i64);
        if let Some(ttft) = metrics.time_to_first_token_ms {
            decode_region.span().record("ttft_ms", ttft as i64);
        }
        if let Some(u) = usage.as_ref() {
            decode_region
                .span()
                .record("output_tokens", u.completion_tokens as i64);
        }
        drop(decode_region);

        // Warn-only for now: log the server-reported triggers once per request (raw labels only, ZDR-safe) and attach them for callers
        let doom_loop_signals = doom_loop
            .as_ref()
            .map(|collector| collector.take())
            .unwrap_or_default();
        if !doom_loop_signals.is_empty() {
            tracing::warn!(
                request_id = %request_id,
                triggers = ?doom_loop_signals.iter().map(|s| s.raw.as_str()).collect::<Vec<_>>(),
                "server reported doom-loop triggers for this response"
            );
        }

        let conversation_response = ConversationResponse {
            items,
            stop_reason,
            usage,
            cost_usd_ticks,
            message_chunks_emitted: message_chunk_count,
            doom_loop_signals,
            stop_message: None, // not reported on the Responses API
            message_id: None,   // no provider message id on the Responses API
            raw_stop_reason,
            stop_sequence: None,
        };

        yield SamplingEvent::Completed {
            request_id: request_id.clone(),
            response: Box::new(conversation_response),
            metrics,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::responses as rs_types;
    use futures_util::stream;
    use std::pin::pin;

    fn rid() -> RequestId {
        RequestId::from("resp-test")
    }

    fn build_response(status: rs_types::Status) -> rs_types::Response {
        rs_types::Response {
            background: None,
            billing: None,
            conversation: None,
            created_at: 0,
            completed_at: None,
            error: None,
            id: "resp_1".into(),
            incomplete_details: None,
            instructions: None,
            max_output_tokens: None,
            metadata: None,
            model: "test-model".into(),
            object: "response".into(),
            output: vec![],
            parallel_tool_calls: None,
            previous_response_id: None,
            prompt: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            reasoning: None,
            safety_identifier: None,
            service_tier: None,
            status,
            temperature: None,
            text: None,
            tool_choice: None,
            tools: None,
            top_logprobs: None,
            top_p: None,
            truncation: None,
            usage: None,
        }
    }

    fn empty_completed_response() -> rs_types::Response {
        build_response(rs_types::Status::Completed)
    }

    fn failed_response_with_error(message: &str) -> rs_types::Response {
        let mut r = build_response(rs_types::Status::Failed);
        r.error = Some(rs_types::ErrorObject {
            code: "server_error".into(),
            message: message.into(),
        });
        r
    }

    fn text_delta_event(delta: &str) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputTextDelta(rs_types::ResponseTextDeltaEvent {
            sequence_number: 0,
            item_id: "item-1".into(),
            output_index: 0,
            content_index: 0,
            delta: delta.into(),
            logprobs: None,
        })
    }

    fn completed_event() -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseCompleted(rs_types::ResponseCompletedEvent {
            response: empty_completed_response(),
            sequence_number: 0,
        })
    }

    async fn collect(s: impl Stream<Item = SamplingEvent>) -> Vec<SamplingEvent> {
        let mut out = Vec::new();
        let mut s = pin!(s);
        while let Some(ev) = s.next().await {
            out.push(ev);
        }
        out
    }

    /// A confident signal that aborts on a custom-tool input frame still vetoes the replay.
    /// The frame is the only notice that a call was in flight, and reasoning must never be retried without it.
    /// The same holds for the code-interpreter code frames.
    #[tokio::test]
    async fn an_abort_on_a_tool_input_frame_vetoes_the_replay() {
        for tool_frame in [
            rs::ResponseStreamEvent::ResponseCustomToolCallInputDelta(
                rs_types::ResponseCustomToolCallInputDeltaEvent {
                    sequence_number: 1,
                    output_index: 1,
                    item_id: "custom-1".into(),
                    delta: "{\"q\":".into(),
                },
            ),
            rs::ResponseStreamEvent::ResponseCodeInterpreterCallCodeDelta(
                rs_types::ResponseCodeInterpreterCallCodeDeltaEvent {
                    sequence_number: 1,
                    output_index: 1,
                    item_id: "ci-1".into(),
                    delta: "print(".into(),
                },
            ),
        ] {
            let capture = FailedResponseCapture::armed();
            // A collector that has already seen a confident trigger: the next non-terminal frame aborts the attempt
            let collector = crate::doom_loop::DoomLoopSignalCollector::new(
                xai_grok_sampling_types::DoomLoopRecoveryPolicy::default(),
            );
            collector.absorb(
                xai_grok_sampling_types::doom_loop::DOOM_LOOP_CHECK_EVENT_TYPE,
                r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:8@thinking"]}}"#,
            );

            // Reasoning is already captured, so an intact replay would carry it: only the veto can empty the capture
            // The collector is armed before the stream runs, so the abort lands on the tool frame
            capture.record_reasoning_delta(0, 0, "reasoning-1".into(), "looping thought");
            let raw = stream::iter(vec![Ok(tool_frame), Ok(completed_event())]).boxed();
            let events = collect(stream_responses_tracked(
                raw,
                None,
                rid(),
                Duration::from_secs(60),
                Some(collector),
                Arc::new(AtomicBool::new(false)),
                capture.clone(),
            ))
            .await;

            assert!(
                matches!(events.last(), Some(SamplingEvent::Failed { .. })),
                "the confident signal aborts the attempt"
            );
            assert!(
                capture.take_items().is_empty(),
                "a turn with a call in flight replays nothing"
            );
        }
    }

    #[tokio::test]
    async fn missing_completed_event_yields_failed() {
        let raw =
            stream::iter(Vec::<Result<rs::ResponseStreamEvent, SamplingError>>::new()).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::Api);
                assert_eq!(error.status_code, Some(500));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    fn incomplete_event(reason: &str) -> rs::ResponseStreamEvent {
        let mut response = build_response(rs_types::Status::Incomplete);
        response.incomplete_details = Some(rs_types::IncompleteDetails {
            reason: reason.into(),
        });
        rs::ResponseStreamEvent::ResponseIncomplete(rs_types::ResponseIncompleteEvent {
            response,
            sequence_number: 0,
        })
    }

    /// Returns the (collapsed stop reason, raw wire stop reason) for an Incomplete response ending with the given `incomplete_details.reason`.
    async fn stop_reasons_for_incomplete(reason: &str) -> (Option<StopReason>, Option<String>) {
        let raw = stream::iter(vec![
            Ok(text_delta_event("cut")),
            Ok(incomplete_event(reason)),
        ])
        .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                (response.stop_reason, response.raw_stop_reason.clone())
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    async fn stop_reason_for_incomplete(reason: &str) -> Option<StopReason> {
        stop_reasons_for_incomplete(reason).await.0
    }

    /// A token-budget cut maps to Length (the salvageable class).
    #[tokio::test]
    async fn incomplete_max_output_tokens_maps_to_length() {
        assert_eq!(
            stop_reasons_for_incomplete("max_output_tokens").await,
            (Some(StopReason::Length), Some("max_tokens".to_string()))
        );
    }

    /// Context-window exhaustion ("max_prompt_tokens", the xAI extension) is also a Length cut, not the unknown-reason fallback.
    /// It keeps its wire distinction in `raw_stop_reason`, in the Messages vocabulary.
    #[tokio::test]
    async fn incomplete_max_prompt_tokens_maps_to_length() {
        assert_eq!(
            stop_reasons_for_incomplete("max_prompt_tokens").await,
            (
                Some(StopReason::Length),
                Some("model_context_window_exceeded".to_string())
            )
        );
    }

    /// A server time-limit cut ("max_time_limit", the xAI extension) is a known Length cut, not the unknown-reason fallback.
    /// It carries no raw reason (the Messages vocabulary has no word for it).
    #[tokio::test]
    async fn incomplete_max_time_limit_maps_to_length() {
        assert_eq!(
            stop_reasons_for_incomplete("max_time_limit").await,
            (Some(StopReason::Length), None)
        );
    }

    /// A moderation cut maps to ContentFilter, never Length: a filter-cut response must not be salvaged and continued by `LengthPolicy`.
    #[tokio::test]
    async fn incomplete_content_filter_maps_to_content_filter() {
        assert_eq!(
            stop_reason_for_incomplete("content_filter").await,
            Some(StopReason::ContentFilter)
        );
    }

    /// A missing `incomplete_details` still maps to Length: an Incomplete response must never look like a clean Stop.
    #[tokio::test]
    async fn incomplete_without_details_maps_to_length() {
        let event =
            rs::ResponseStreamEvent::ResponseIncomplete(rs_types::ResponseIncompleteEvent {
                response: build_response(rs_types::Status::Incomplete),
                sequence_number: 0,
            });
        let raw = stream::iter(vec![Ok(text_delta_event("cut")), Ok(event)]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.stop_reason, Some(StopReason::Length));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// Pins the precedence where tool calls beat an Incomplete status.
    /// A truncated response that still carries a function call reports ToolCalls, not Length.
    /// The pair stays coherent: no raw length reason rides along, so the headless output keeps reporting `tool_use` for tool-bearing turns.
    #[tokio::test]
    async fn incomplete_with_tool_calls_maps_to_tool_calls() {
        let mut response = build_response(rs_types::Status::Incomplete);
        response.incomplete_details = Some(rs_types::IncompleteDetails {
            reason: "max_output_tokens".into(),
        });
        response.output = vec![rs_types::OutputItem::FunctionCall(
            rs_types::FunctionToolCall {
                arguments: "{\"x\":1".into(),
                call_id: "call_1".into(),
                name: "do_thing".into(),
                id: None,
                status: None,
            },
        )];
        let event =
            rs::ResponseStreamEvent::ResponseIncomplete(rs_types::ResponseIncompleteEvent {
                response,
                sequence_number: 0,
            });
        let raw = stream::iter(vec![Ok(event)]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
                assert_eq!(response.raw_stop_reason, None);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// The unknown-reason arm is the forward-compatibility story.
    /// A wire value this client has never seen collapses to Length (salvageable, never a parse failure) and carries no raw reason.
    #[tokio::test]
    async fn incomplete_unknown_reason_maps_to_length() {
        assert_eq!(
            stop_reasons_for_incomplete("some_future_reason").await,
            (Some(StopReason::Length), None)
        );
    }

    #[tokio::test]
    async fn text_delta_then_completed_yields_completed_with_stop() {
        let raw = stream::iter(vec![Ok(text_delta_event("hello")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        let text_tokens: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                SamplingEvent::ChannelToken {
                    channel: SamplingChannel::Text,
                    text,
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text_tokens, vec!["hello"]);

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.stop_reason, Some(StopReason::Stop));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn empty_failed_response_is_not_treated_as_output() {
        let event = rs::ResponseStreamEvent::ResponseFailed(rs_types::ResponseFailedEvent {
            response: failed_response_with_error("boom"),
            sequence_number: 0,
        });
        assert!(!responses_event_may_have_output(&event));
    }

    #[tokio::test]
    async fn response_failed_yields_failed_500() {
        let failed = rs::ResponseStreamEvent::ResponseFailed(rs_types::ResponseFailedEvent {
            response: failed_response_with_error("boom"),
            sequence_number: 0,
        });
        let raw = stream::iter(vec![Ok(failed)]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::Api);
                assert_eq!(error.status_code, Some(500));
                assert!(error.message.contains("boom"));
                // The wire code passes through verbatim; dropping it here would disable strip recovery for coded Responses failures
                assert_eq!(
                    error.error_code,
                    Some(xai_grok_sampling_types::ApiErrorCode::Other(
                        "server_error".into()
                    ))
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// A coded `error` event must carry its code into the Failed info.
    /// This is the whole mid-stream strip-recovery chain for the Responses backend (the synthesized 500 with its code classifies as an image error).
    #[tokio::test]
    async fn response_error_event_carries_code_into_failed() {
        let error_event = rs::ResponseStreamEvent::ResponseError(rs_types::ResponseErrorEvent {
            sequence_number: 0,
            code: Some(xai_grok_sampling_types::INVALID_IMAGE_ERROR_CODE.into()),
            message: "could not decode image".into(),
            param: None,
        });
        let raw = stream::iter(vec![Ok(error_event)]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(
                    error.error_code,
                    Some(xai_grok_sampling_types::ApiErrorCode::InvalidImage)
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mid_stream_transport_error_yields_failed() {
        let raw = stream::iter(vec![
            Ok(text_delta_event("hi")),
            Err(SamplingError::EventStreamError("conn reset".into())),
        ])
        .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Failed { .. }))
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Completed { .. }))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_when_stream_stalls() {
        let raw = stream::iter(vec![Ok(text_delta_event("hi"))])
            .chain(stream::pending())
            .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_millis(100),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::IdleTimeout);
            }
            other => panic!("expected Failed(IdleTimeout), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_metadata_yielded_after_stream_started() {
        let raw = stream::iter(vec![Ok(completed_event())]).boxed();
        let metadata = ResponseModelMetadata {
            context_window: Some(8192),
            ..Default::default()
        };
        let events = collect(stream_responses(
            raw,
            Some(metadata),
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(matches!(
            events.first(),
            Some(SamplingEvent::StreamStarted { .. })
        ));
        assert!(matches!(
            events.get(1),
            Some(SamplingEvent::ModelMetadata { .. })
        ));
    }

    #[test]
    fn meaningful_content_classifier_basics() {
        let event = text_delta_event("foo");
        assert!(responses_event_has_meaningful_content(&event));
        let empty = text_delta_event("");
        assert!(!responses_event_has_meaningful_content(&empty));
        assert!(responses_event_has_meaningful_content(&completed_event()));
    }

    #[test]
    fn output_classifier_covers_non_forwarded_backend_events() {
        let queued = rs::ResponseStreamEvent::ResponseQueued(rs_types::ResponseQueuedEvent {
            sequence_number: 0,
            response: empty_completed_response(),
        });
        assert!(!responses_event_may_have_output(&queued));

        let response_error = rs::ResponseStreamEvent::ResponseError(rs_types::ResponseErrorEvent {
            sequence_number: 1,
            code: Some("server_error".into()),
            message: "failed before output".into(),
            param: None,
        });
        assert!(!responses_event_may_have_output(&response_error));

        let refusal =
            rs::ResponseStreamEvent::ResponseRefusalDelta(rs_types::ResponseRefusalDeltaEvent {
                sequence_number: 1,
                item_id: "item-1".into(),
                output_index: 0,
                content_index: 0,
                delta: "no".into(),
            });
        assert!(responses_event_may_have_output(&refusal));

        let backend_progress = rs::ResponseStreamEvent::ResponseWebSearchCallSearching(
            rs_types::ResponseWebSearchCallSearchingEvent {
                sequence_number: 2,
                output_index: 0,
                item_id: "search-1".into(),
            },
        );
        assert!(responses_event_may_have_output(&backend_progress));
    }

    #[tokio::test]
    async fn tracked_stream_marks_refusal_as_output() {
        let output_observed = Arc::new(AtomicBool::new(false));
        let refusal =
            rs::ResponseStreamEvent::ResponseRefusalDelta(rs_types::ResponseRefusalDeltaEvent {
                sequence_number: 0,
                item_id: "item-1".into(),
                output_index: 0,
                content_index: 0,
                delta: "no".into(),
            });
        let raw = stream::iter(vec![Ok(refusal), Ok(completed_event())]).boxed();
        let _ = collect(stream_responses_tracked(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
            Arc::clone(&output_observed),
            FailedResponseCapture::default(),
        ))
        .await;

        assert!(output_observed.load(Ordering::Relaxed));
    }

    /// A server-side code-interpreter run is emitted as a generic backend tool call named "code_interpreter", the same shape as x_search.
    /// It starts on InProgress and completes on OutputItemDone.
    #[tokio::test]
    async fn code_interpreter_forwards_backend_tool_call() {
        let in_progress = rs::ResponseStreamEvent::ResponseCodeInterpreterCallInProgress(
            rs_types::ResponseCodeInterpreterCallInProgressEvent {
                sequence_number: 0,
                output_index: 0,
                item_id: "ci-1".into(),
            },
        );
        let done = rs::ResponseStreamEvent::ResponseOutputItemDone(
            rs_types::ResponseOutputItemDoneEvent {
                sequence_number: 1,
                output_index: 0,
                item: rs_types::OutputItem::CodeInterpreterCall(
                    rs_types::CodeInterpreterToolCall {
                        code: Some("print(1)".into()),
                        container_id: "cont-1".into(),
                        id: "ci-1".into(),
                        outputs: None,
                        status: rs_types::CodeInterpreterToolCallStatus::Completed,
                    },
                ),
            },
        );
        let raw = stream::iter(vec![Ok(in_progress), Ok(done), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(
            events.iter().any(|e| matches!(
                e,
                SamplingEvent::BackendToolCallStarted { call_id, name, .. }
                    if call_id == "ci-1" && name == "code_interpreter"
            )),
            "expected a code_interpreter BackendToolCallStarted, got {events:?}"
        );
        let completed = events.iter().find_map(|e| match e {
            SamplingEvent::BackendToolCallCompleted {
                call_id,
                name,
                result,
                ..
            } if name == "code_interpreter" => Some((call_id.clone(), result.clone())),
            _ => None,
        });
        let (call_id, result) = completed.expect("a code_interpreter BackendToolCallCompleted");
        assert_eq!(call_id, "ci-1");
        let result = result.expect("serialized code-interpreter payload");
        assert_eq!(result.get("code"), Some(&serde_json::json!("print(1)")));
    }

    /// 从协议 JSON 构造帧，覆盖可选字段省略。
    fn wire_event(value: serde_json::Value) -> rs::ResponseStreamEvent {
        serde_json::from_value(value).unwrap()
    }

    /// 收集一次无网络的 Responses 消费结果。
    async fn consume(events: Vec<rs::ResponseStreamEvent>) -> Vec<SamplingEvent> {
        collect(stream_responses(
            stream::iter(events.into_iter().map(Ok)).boxed(),
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await
    }

    /// 提取用户可见正文，不包含推理通道。
    fn visible_text(events: &[SamplingEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                SamplingEvent::ChannelToken {
                    channel: SamplingChannel::Text,
                    text,
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// 构造包含完整正文的消息项。
    fn message_item(text: &str) -> serde_json::Value {
        serde_json::json!({"type":"message", "id":"item-1", "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":text, "annotations":[]}]})
    }

    /// 构造带完整输出的终态帧。
    fn terminal_with(output: serde_json::Value) -> rs::ResponseStreamEvent {
        let mut response = empty_completed_response();
        response.output = serde_json::from_value(output).unwrap();
        rs::ResponseStreamEvent::ResponseCompleted(rs_types::ResponseCompletedEvent {
            response,
            sequence_number: 9,
        })
    }

    #[tokio::test]
    async fn text_done_item_done_and_terminal_only_append_missing_suffixes() {
        let events = consume(vec![
            text_delta_event("你"),
            wire_event(serde_json::json!({"type":"response.output_text.done", "sequence_number":1,
                "item_id":"item-1", "output_index":0, "content_index":0, "text":"你好", "logprobs":[]})),
            wire_event(serde_json::json!({"type":"response.output_item.done", "sequence_number":2,
                "output_index":0, "item":message_item("你好世界")})),
            terminal_with(serde_json::json!([message_item("你好世界！")])),
        ]).await;
        assert_eq!(visible_text(&events), "你好世界！");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, SamplingEvent::FirstToken { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn refusal_delta_done_and_message_snapshot_are_visible_once() {
        let events = consume(vec![
            wire_event(serde_json::json!({"type":"response.refusal.delta", "sequence_number":0,
                "item_id":"item-1", "output_index":0, "content_index":0, "delta":"不能"})),
            wire_event(serde_json::json!({"type":"response.refusal.done", "sequence_number":1,
                "item_id":"item-1", "output_index":0, "content_index":0, "refusal":"不能提供"})),
            terminal_with(serde_json::json!([{"type":"message", "id":"item-1", "role":"assistant", "status":"completed",
                "content":[{"type":"refusal", "refusal":"不能提供"}]}])),
        ]).await;
        assert_eq!(visible_text(&events), "不能提供");
    }

    #[tokio::test]
    async fn terminal_snapshot_is_authoritative_when_stream_text_conflicts() {
        let events = consume(vec![
            text_delta_event("old"),
            terminal_with(serde_json::json!([message_item("new")])),
        ])
        .await;
        assert_eq!(visible_text(&events), "old");
        let Some(SamplingEvent::Completed { response, .. }) = events.last() else {
            panic!("missing completed")
        };
        assert!(response.items.iter().any(
            |item| matches!(item, ConversationItem::Assistant(a) if a.content.as_ref() == "new")
        ));
    }

    #[tokio::test]
    async fn terminal_only_message_and_function_call_are_forwarded() {
        let events = consume(vec![terminal_with(serde_json::json!([
            message_item("final"),
            {"type":"function_call", "id":"fc-1", "call_id":"call-1", "name":"run", "arguments":"{}", "status":"completed"}
        ]))]).await;
        assert_eq!(visible_text(&events), "final");
        let deltas = tool_call_deltas(&events);
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0].1.as_deref(), Some("call-1"));
        assert_eq!(deltas[1].3.as_deref(), Some("{}"));
    }

    #[tokio::test]
    async fn arguments_done_and_item_done_fill_missing_added_without_duplicates() {
        let item = serde_json::json!({"type":"function_call", "id":"fc-1", "call_id":"call-1", "name":"run", "arguments":"{\"x\":1}", "status":"completed"});
        let events = consume(vec![
            function_call_args_delta_event(0, "{\"x\":"),
            wire_event(serde_json::json!({"type":"response.function_call_arguments.done", "sequence_number":1,
                "item_id":"fc-1", "output_index":0, "name":"run", "arguments":"{\"x\":1}"})),
            wire_event(serde_json::json!({"type":"response.output_item.done", "sequence_number":2, "output_index":0, "item":item.clone()})),
            wire_event(serde_json::json!({"type":"response.output_item.done", "sequence_number":3, "output_index":0, "item":item.clone()})),
            terminal_with(serde_json::json!([item])),
        ]).await;
        let deltas = tool_call_deltas(&events);
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[1].3.as_deref(), Some("{\"x\":1}"));
        let Some(SamplingEvent::Completed { response, .. }) = events.last() else {
            panic!("missing completed")
        };
        assert_eq!(
            response
                .items
                .iter()
                .filter_map(|item| match item {
                    ConversationItem::Assistant(a) => Some(a.tool_calls.len()),
                    _ => None,
                })
                .sum::<usize>(),
            1
        );
    }

    #[tokio::test]
    async fn arguments_done_only_emits_suffix_after_live_deltas() {
        let events = consume(vec![
            function_call_added_event(0, "call-1", "run"),
            function_call_added_event(0, "call-1", "run"),
            function_call_args_delta_event(0, "{"),
            wire_event(serde_json::json!({"type":"response.function_call_arguments.done", "sequence_number":1,
                "item_id":"fc-1", "output_index":0, "name":"run", "arguments":"{}"})),
            completed_event(),
        ]).await;
        let deltas = tool_call_deltas(&events);
        assert_eq!(deltas.len(), 3);
        assert_eq!(deltas[2].3.as_deref(), Some("}"));
    }

    #[tokio::test]
    async fn final_filtered_output_keeps_original_function_identity() {
        let item = serde_json::json!({"type":"function_call", "id":"fc-1", "call_id":"call-1", "name":"run", "arguments":"{}"});
        let events = consume(vec![
            function_call_added_event(3, "call-1", "run"),
            function_call_args_delta_event(3, "{}"),
            terminal_with(serde_json::json!([item])),
        ])
        .await;
        assert_eq!(tool_call_deltas(&events).len(), 2);
    }

    #[tokio::test]
    async fn done_only_refusal_is_visible_but_does_not_replace_terminal_event() {
        let events = consume(vec![wire_event(serde_json::json!({
            "type":"response.refusal.done", "sequence_number":0, "item_id":"item-1",
            "output_index":0, "content_index":0, "refusal":"拒绝"
        }))])
        .await;
        assert_eq!(visible_text(&events), "拒绝");
        assert!(matches!(events.last(), Some(SamplingEvent::Failed { .. })));
    }

    #[test]
    fn conflicting_snapshots_never_reemit_already_displayed_content() {
        let mut shown = "original".to_owned();
        assert!(reconcile_snapshot(&mut shown, "different").is_empty());
        assert!(reconcile_snapshot(&mut shown, "different").is_empty());
        assert_eq!(shown, "original");
    }

    #[tokio::test]
    async fn hosted_results_show_bounded_summaries_once_without_client_calls() {
        let payload = "A".repeat(100_000);
        for item in [
            serde_json::json!({"type":"file_search_call", "id":"fs-1", "status":"completed", "queries":["query"], "results":[]}),
            serde_json::json!({"type":"mcp_call", "id":"mcp-1", "name":"remote", "server_label":"server", "arguments":"{}", "output":payload.clone()}),
            serde_json::json!({"type":"image_generation_call", "id":"img-1", "status":"completed", "result":payload.clone()}),
        ] {
            for done in [false, true] {
                let mut frames = Vec::new();
                if done {
                    frames.push(wire_event(
                        serde_json::json!({"type":"response.output_item.done", "sequence_number":1,
                        "output_index":0, "item":item.clone()}),
                    ));
                }
                frames.push(terminal_with(serde_json::json!([item.clone()])));
                let events = consume(frames).await;
                assert!(tool_call_deltas(&events).is_empty());
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(
                            event,
                            SamplingEvent::BackendToolCallStarted { .. }
                        ))
                        .count(),
                    1
                );
                let results: Vec<_> = events
                    .iter()
                    .filter_map(|event| match event {
                        SamplingEvent::BackendToolCallCompleted { result, .. } => Some(result),
                        _ => None,
                    })
                    .collect();
                assert_eq!(results.len(), 1);
                let summary = results[0].as_ref().unwrap().as_str().unwrap();
                assert!(summary.len() < 256);
                assert!(!summary.contains(&payload));
            }
        }
    }

    #[tokio::test]
    async fn unsupported_only_output_emits_notice_without_changing_history() {
        for raw in [
            serde_json::json!([{"type":"message", "id":"audio-1", "role":"assistant", "content":[{"type":"output_audio", "data":"SECRET_BASE64"}]}]),
            serde_json::json!([{"type":"future_output", "data":"SECRET_BASE64"}]),
        ] {
            let mut response = empty_completed_response();
            response.metadata = Some(std::collections::HashMap::from([(
                xai_grok_sampling_types::RESPONSE_RAW_OUTPUT_METADATA_KEY.to_owned(),
                raw.to_string(),
            )]));
            let events = consume(vec![rs::ResponseStreamEvent::ResponseCompleted(
                rs_types::ResponseCompletedEvent {
                    response,
                    sequence_number: 1,
                },
            )])
            .await;
            let text = visible_text(&events);
            assert!(text.contains("客户端能力提示"));
            assert!(!text.contains("SECRET_BASE64"));
            let Some(SamplingEvent::Completed { response, .. }) = events.last() else {
                panic!("missing completed")
            };
            let assistant = response
                .items
                .iter()
                .find_map(|item| match item {
                    ConversationItem::Assistant(assistant) => Some(assistant),
                    _ => None,
                })
                .unwrap();
            assert!(assistant.content.is_empty());
            assert_eq!(assistant.responses_output.as_ref(), raw.as_array());
        }
    }

    #[tokio::test]
    async fn unsupported_output_with_visible_text_does_not_add_notice() {
        let mut response = empty_completed_response();
        response.metadata = Some(std::collections::HashMap::from([(
            xai_grok_sampling_types::RESPONSE_RAW_OUTPUT_METADATA_KEY.to_owned(),
            serde_json::json!([{"type":"output_audio", "data":"SECRET_BASE64"}]).to_string(),
        )]));
        let events = consume(vec![
            text_delta_event("正文"),
            rs::ResponseStreamEvent::ResponseCompleted(rs_types::ResponseCompletedEvent {
                response,
                sequence_number: 1,
            }),
        ])
        .await;
        assert_eq!(visible_text(&events), "正文");
    }

    #[tokio::test]
    async fn only_known_custom_tools_emit_x_search_events() {
        for (name, expected) in [
            ("custom_executor", 0),
            ("x_unknown", 0),
            ("x_keyword_search", 2),
            ("x_search", 2),
        ] {
            let events = consume(vec![
                wire_event(serde_json::json!({"type":"response.custom_tool_call_input.done", "sequence_number":0,
                    "item_id":"custom-1", "output_index":0, "input":"{}"})),
                wire_event(serde_json::json!({"type":"response.output_item.done", "sequence_number":1, "output_index":0,
                    "item":{"type":"custom_tool_call", "id":"custom-1", "call_id":"call-1", "name":name, "input":"{}"}})),
                completed_event(),
            ]).await;
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        event,
                        SamplingEvent::BackendToolCallStarted { .. }
                            | SamplingEvent::BackendToolCallCompleted { .. }
                    ))
                    .count(),
                expected
            );
        }
    }

    fn function_call_added_event(
        output_index: u32,
        call_id: &str,
        name: &str,
    ) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputItemAdded(rs_types::ResponseOutputItemAddedEvent {
            sequence_number: 0,
            output_index,
            item: rs_types::OutputItem::FunctionCall(rs_types::FunctionToolCall {
                arguments: String::new(),
                call_id: call_id.into(),
                name: name.into(),
                id: None,
                status: None,
            }),
        })
    }

    fn function_call_args_delta_event(output_index: u32, delta: &str) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(
            rs_types::ResponseFunctionCallArgumentsDeltaEvent {
                sequence_number: 0,
                item_id: format!("item-{output_index}"),
                output_index,
                delta: delta.into(),
            },
        )
    }

    type Delta = (u32, Option<String>, Option<String>, Option<String>);

    /// Extract all ToolCallDelta events as (tool_index, id, name, arguments_delta).
    fn tool_call_deltas(evs: &[SamplingEvent]) -> Vec<Delta> {
        evs.iter()
            .filter_map(|e| match e {
                SamplingEvent::ToolCallDelta {
                    tool_index,
                    id,
                    name,
                    arguments_delta,
                    ..
                } => Some((
                    *tool_index,
                    id.clone(),
                    name.clone(),
                    arguments_delta.clone(),
                )),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn function_call_emits_initial_id_name_then_arg_deltas() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_added_event(0, "call_xyz", "do_thing")),
            Ok(function_call_args_delta_event(0, "{\"x\":")),
            Ok(function_call_args_delta_event(0, "1}")),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        let deltas = tool_call_deltas(&evs);

        let [d0, d1, d2] = deltas.as_slice() else {
            panic!("expected three deltas: {deltas:?}");
        };
        assert_eq!(d0.0, 0);
        assert_eq!(d0.1.as_deref(), Some("call_xyz"));
        assert_eq!(d0.2.as_deref(), Some("do_thing"));
        assert_eq!(d0.3, None);
        assert_eq!(d1.0, 0);
        assert_eq!(d1.1, None);
        assert_eq!(d1.2, None);
        assert_eq!(d1.3.as_deref(), Some("{\"x\":"));
        assert_eq!(d2.3.as_deref(), Some("1}"));
    }

    #[tokio::test]
    async fn function_call_args_delta_without_added_event_is_dropped() {
        // ArgumentsDelta with no preceding OutputItemAdded has no output_index to tool_index mapping, so it is dropped silently
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_args_delta_event(7, "{\"oops\":1}")),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        assert_eq!(tool_call_deltas(&evs).len(), 0);
    }

    #[tokio::test]
    async fn multiple_function_calls_get_distinct_tool_indices() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_added_event(0, "call_a", "tool_a")),
            Ok(function_call_added_event(1, "call_b", "tool_b")),
            Ok(function_call_args_delta_event(0, "a-args")),
            Ok(function_call_args_delta_event(1, "b-args")),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        let deltas = tool_call_deltas(&evs);

        let [d0, d1, d2, d3] = deltas.as_slice() else {
            panic!("expected four deltas: {deltas:?}");
        };
        assert_eq!(d0.0, 0);
        assert_eq!(d0.1.as_deref(), Some("call_a"));
        assert_eq!(d1.0, 1);
        assert_eq!(d1.1.as_deref(), Some("call_b"));
        assert_eq!(d2.0, 0);
        assert_eq!(d2.3.as_deref(), Some("a-args"));
        assert_eq!(d3.0, 1);
        assert_eq!(d3.3.as_deref(), Some("b-args"));
    }

    #[tokio::test]
    async fn doom_loop_collector_signals_land_on_completed_response() {
        use xai_grok_sampling_types::doom_loop::{
            DOOM_LOOP_CHECK_EVENT_TYPE, SAMPLE_CHECK_EVENT_DATA,
        };
        let collector = crate::doom_loop::DoomLoopSignalCollector::default();
        assert!(collector.absorb(DOOM_LOOP_CHECK_EVENT_TYPE, SAMPLE_CHECK_EVENT_DATA));
        let raw = stream::iter(vec![Ok(text_delta_event("hello")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(collector),
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.doom_loop_signals.len(), 1);
                let Some(signal) = response.doom_loop_signals.first() else {
                    panic!("expected doom loop signal: {response:?}");
                };
                assert_eq!(signal.raw, "tail_repetition:4@response");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// An armed collector holding a confident signal aborts the attempt with a retryable doom-loop failure; all detector labels are emitted first.
    /// After `disarm_abort`, the same stream completes and the signals ride the response.
    #[tokio::test]
    async fn confident_signal_aborts_stream_unless_disarmed() {
        let confident = r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:8@thinking","exact_repetition:42x3@thinking"]}}"#;

        let collector = crate::doom_loop::DoomLoopSignalCollector::default();
        assert!(collector.absorb("response.doom_loop_check", confident));
        let raw = stream::iter(vec![Ok(text_delta_event("hi")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(collector),
        ))
        .await;
        assert!(matches!(
            events.get(events.len().saturating_sub(2)),
            Some(SamplingEvent::DoomLoopSignals { triggers, .. })
                if triggers == &[
                    "tail_repetition:8@thinking".to_string(),
                    "exact_repetition:42x3@thinking".to_string(),
                ]
        ));
        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(
                    error.kind,
                    crate::events::SamplingErrorKind::DoomLoopDetected
                );
                assert!(error.is_retryable);
                assert_eq!(
                    error.doom_loop_triggers.as_deref(),
                    Some(["tail_repetition:8@thinking".to_string()].as_slice())
                );
            }
            other => panic!("expected Failed(DoomLoopDetected), got {other:?}"),
        }
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Completed { .. }))
        );

        let collector = crate::doom_loop::DoomLoopSignalCollector::default();
        assert!(collector.absorb("response.doom_loop_check", confident));
        collector.disarm_abort();
        let raw = stream::iter(vec![Ok(text_delta_event("hi")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(collector),
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.doom_loop_signals.len(), 2);
            }
            other => panic!("expected Completed after disarm, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn doom_loop_signals_empty_without_collector_or_triggers() {
        let raw = stream::iter(vec![Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert!(response.doom_loop_signals.is_empty());
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // A collector that never saw a trigger also leaves the field empty.
        let raw = stream::iter(vec![Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(crate::doom_loop::DoomLoopSignalCollector::default()),
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert!(response.doom_loop_signals.is_empty());
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }
}
