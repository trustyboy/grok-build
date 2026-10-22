use super::*;
use sha2::{Digest, Sha256};
use std::io::{self, Write};

#[path = "responses_context.rs"]
mod context;
pub use context::{
    RESPONSE_CONTEXT_CHECKPOINT_FIELD, ResponsesContextUsage, ResponsesInputProjection,
    assistant_context_usage, checkpoint_responses_context, checkpoint_responses_context_for_input,
    response_context_usage, responses_projection_context,
};

/// 流适配器通过 metadata 传递完整 output 数组的 JSON 字符串。
pub const RESPONSE_RAW_OUTPUT_METADATA_KEY: &str = "__grok_responses_raw_output";
/// 客户端覆盖的来源哈希；不得直接信任上游 metadata。
pub const RESPONSE_SOURCE_METADATA_KEY: &str = "__grok_responses_source";
/// 客户端发送前计算的本地历史指纹。
pub const RESPONSE_INPUT_FINGERPRINT_METADATA_KEY: &str = "__grok_responses_input_fingerprint";

/// 对准备完成的完整请求历史和来源求指纹，而非已裁剪的线上 input。
/// 调用前须完成提示注入、历史归一化及图片过滤；工具定义不属于历史。
/// 已有提示或内容变化时保守失效，新追加的提示不影响此前压缩点。
pub fn responses_input_fingerprint(req: &ConversationRequest, source: &str) -> String {
    responses_history_fingerprint(&req.items, source)
}

/// 保留图片、原始输出及 sibling 内容，任何删除或改写均使指纹失效。
fn responses_history_fingerprint(items: &[ConversationItem], source: &str) -> String {
    let mut writer = ResponsesHistoryHasher::default();
    serde_json::to_writer(&mut writer, &("responses-history-v1", source, items))
        .expect("conversation history is serializable");
    writer.finish()
}

#[cfg(test)]
std::thread_local! {
    /// 仅统计历史指纹写入量，避免用耗时断言验证线性复杂度。
    static RESPONSES_HISTORY_HASHED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 读取并清零当前测试线程的历史指纹写入量。
#[cfg(test)]
pub(super) fn take_responses_history_hashed_bytes() -> usize {
    RESPONSES_HISTORY_HASHED_BYTES.with(|bytes| bytes.replace(0))
}

/// 将 JSON 直接写入摘要，不分配完整历史缓冲区。
#[derive(Clone, Default)]
struct ResponsesHistoryHasher {
    hash: Sha256,
}

impl Write for ResponsesHistoryHasher {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.hash.update(bytes);
        #[cfg(test)]
        RESPONSES_HISTORY_HASHED_BYTES.with(|count| count.set(count.get() + bytes.len()));
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ResponsesHistoryHasher {
    /// 保持已持久化的十六进制 SHA-256 格式。
    fn finish(self) -> String {
        self.hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// 未闭合的 v1 元组与历史数组，按递增前缀共享 SHA-256 状态。
struct ResponsesHistoryFingerprint {
    writer: ResponsesHistoryHasher,
    item_count: usize,
}

impl ResponsesHistoryFingerprint {
    /// 固定元组头与 serde_json 的紧凑序列化逐字节一致。
    fn new(source: &str) -> Self {
        let mut writer = ResponsesHistoryHasher::default();
        writer.write_all(b"[\"responses-history-v1\",").unwrap();
        serde_json::to_writer(&mut writer, source).expect("source is serializable");
        writer.write_all(b",[").unwrap();
        Self {
            writer,
            item_count: 0,
        }
    }

    /// 每项仅序列化一次；只在状态副本上闭合元组，不影响后续追加。
    fn prefix(&mut self, items: &[ConversationItem]) -> String {
        for item in &items[self.item_count..] {
            if self.item_count > 0 {
                self.writer.write_all(b",").unwrap();
            }
            serde_json::to_writer(&mut self.writer, item)
                .expect("conversation history is serializable");
            self.item_count += 1;
        }
        let mut snapshot = self.writer.clone();
        snapshot.write_all(b"]]").unwrap();
        snapshot.finish()
    }
}

/// X 搜索服务端工具名；保留旧历史中的聚合名称。
pub fn is_responses_x_search_tool(name: &str) -> bool {
    matches!(
        name,
        "x_search" | "x_keyword_search" | "x_semantic_search" | "x_user_search" | "x_thread_fetch"
    )
}

/// 原始消息投影与跨协议通用文本保持一致。
fn responses_output_text(raw: &[serde_json::Value]) -> String {
    raw.iter()
        .filter(|item| item.get("type").and_then(|v| v.as_str()) == Some("message"))
        .filter_map(|item| item.get("content").and_then(|v| v.as_array()))
        .flatten()
        .filter_map(|part| match part.get("type").and_then(|v| v.as_str()) {
            Some("output_text") => part.get("text").and_then(|v| v.as_str()),
            Some("refusal") => part.get("refusal").and_then(|v| v.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 通用历史被裁剪或修复后，旧协议快照不得覆盖当前内容。
fn responses_output_matches_assistant(
    raw: &[serde_json::Value],
    assistant: &AssistantItem,
) -> bool {
    if responses_output_text(raw) != assistant.content.as_ref() {
        return false;
    }
    let mut calls = assistant.tool_calls.iter();
    for item in raw
        .iter()
        .filter(|item| item.get("type").and_then(|v| v.as_str()) == Some("function_call"))
    {
        let Some(call) = calls.next() else {
            return false;
        };
        if item.get("call_id").and_then(|v| v.as_str()) != Some(call.id.as_ref())
            || item.get("name").and_then(|v| v.as_str()) != Some(call.name.as_str())
            || item.get("arguments").and_then(|v| v.as_str()) != Some(call.arguments.as_ref())
        {
            return false;
        }
    }
    calls.next().is_none()
}

/// Flatten `response.output` into `ConversationItem`s, preserving emission order.
/// Replaying that order byte for byte on the next turn is what keeps the server-side prefix cache hot.
pub fn response_to_conversation_items(response: rs::Response) -> Vec<ConversationItem> {
    let responses_output = response
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(RESPONSE_RAW_OUTPUT_METADATA_KEY))
        .and_then(|raw| serde_json::from_str::<Vec<serde_json::Value>>(raw).ok())
        .or_else(|| {
            serde_json::to_value(&response.output)
                .ok()
                .and_then(|raw| raw.as_array().cloned())
        });
    // 不完整响应即使携带内部字段也不能提交压缩点。
    let completed = serde_json::to_value(&response.status).ok()
        == Some(serde_json::Value::String("completed".into()));
    let trusted_metadata = |key: &str| {
        completed
            .then(|| response.metadata.as_ref()?.get(key).cloned())
            .flatten()
            .filter(|value| !value.is_empty())
    };
    let responses_source = trusted_metadata(RESPONSE_SOURCE_METADATA_KEY);
    let responses_input_fingerprint = trusted_metadata(RESPONSE_INPUT_FINGERPRINT_METADATA_KEY);
    let model_id = response.model.clone();
    let model_fingerprint = response
        .metadata
        .as_ref()
        .and_then(|m| m.get("system_fingerprint"))
        .cloned()
        .filter(|s| !s.is_empty());
    let reasoning_effort = response
        .reasoning
        .as_ref()
        .and_then(|r| r.effort.clone())
        .map(crate::ReasoningEffort::from_responses_api);

    let mut items: Vec<ConversationItem> = Vec::with_capacity(response.output.len() + 1);
    let mut content = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut backend_tool_count: usize = 0;

    for item in response.output {
        match item {
            rs::OutputItem::Message(msg) => {
                for content_part in msg.content {
                    if let rs::OutputMessageContent::OutputText(text_content) = content_part {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(&text_content.text);
                    }
                }
            }
            rs::OutputItem::FunctionCall(fc) => {
                // Tied to the assistant turn: a ToolResult must follow each one in conversation order, so they are not siblings
                tool_calls.push(ToolCall {
                    id: Arc::<str>::from(fc.call_id),
                    name: fc.name,
                    arguments: Arc::<str>::from(fc.arguments),
                });
            }
            rs::OutputItem::Reasoning(r) => {
                items.push(ConversationItem::Reasoning(r));
            }
            // These calls already ran server-side; they are kept so later turns replay the same context
            rs::OutputItem::WebSearchCall(ws) => {
                backend_tool_count += 1;
                items.push(ConversationItem::BackendToolCall(BackendToolCallItem {
                    kind: BackendToolKind::WebSearch(ws),
                }));
            }
            rs::OutputItem::CustomToolCall(ct) if is_responses_x_search_tool(&ct.name) => {
                backend_tool_count += 1;
                items.push(ConversationItem::BackendToolCall(BackendToolCallItem {
                    kind: BackendToolKind::XSearch(ct),
                }));
            }
            rs::OutputItem::CodeInterpreterCall(ci) => {
                backend_tool_count += 1;
                items.push(ConversationItem::BackendToolCall(BackendToolCallItem {
                    kind: BackendToolKind::CodeInterpreter(ci),
                }));
            }
            rs::OutputItem::McpCall(_) => {
                backend_tool_count += 1;
            }
            _ => {}
        }
    }

    if backend_tool_count > 0 {
        tracing::info!(
            backend_tool_count,
            "response contained backend-executed tool calls"
        );
    }

    // 原始消息包含 SDK 尚不能表达的拒绝和分阶段文本。
    if let Some(raw) = &responses_output {
        content = responses_output_text(raw);
    }
    tracing::info!(model_id = %model_id, ?model_fingerprint, ?reasoning_effort, "response_to_conversation_items setting model metadata on AssistantItem");
    items.push(ConversationItem::Assistant(AssistantItem {
        responses_output,
        responses_source,
        responses_input_fingerprint,
        content: Arc::<str>::from(content),
        tool_calls,
        model_id: Some(model_id),
        model_fingerprint,
        reasoning_effort,
    }));

    items
}

impl From<&ConversationRequest> for rs::CreateResponse {
    fn from(req: &ConversationRequest) -> Self {
        let input = build_responses_input(req);
        let tools = build_responses_tools(req);

        let tool_choice = req.tool_choice.as_ref().map(|tc| match tc {
            ConversationToolChoice::Auto => rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::Auto),
            ConversationToolChoice::None => rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::None),
            ConversationToolChoice::Required => {
                rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::Required)
            }
            ConversationToolChoice::Function(name) => {
                rs::ToolChoiceParam::Function(rs::ToolChoiceFunction { name: name.clone() })
            }
        });

        let text = req
            .json_schema
            .as_ref()
            .map(|schema| rs::ResponseTextParam {
                format: rs::TextResponseFormatConfiguration::JsonSchema(
                    rs::ResponseFormatJsonSchema {
                        description: None,
                        name: STRUCTURED_OUTPUT_SCHEMA_NAME.to_string(),
                        schema: Some(schema.clone()),
                        strict: Some(true),
                    },
                ),
                verbosity: None,
            });

        rs::CreateResponse {
            background: None,
            conversation: None,
            include: None,
            input,
            instructions: None,
            max_output_tokens: req.max_output_tokens,
            max_tool_calls: None,
            metadata: None,
            model: req.model.clone(),
            parallel_tool_calls: None,
            previous_response_id: None,
            prompt: None,
            prompt_cache_key: req
                .prompt_cache_key
                .clone()
                .or_else(|| req.x_grok_conv_id.clone()),
            prompt_cache_retention: None,
            reasoning: Some(rs::Reasoning {
                effort: req.reasoning_effort.map(|e| e.to_responses_api()),
                summary: crate::ReasoningSummary::default().to_responses_api(),
            }),
            safety_identifier: None,
            service_tier: None,
            store: None,
            stream: None,
            stream_options: None,
            temperature: req.temperature,
            text,
            tool_choice,
            tools: if tools.is_empty() { None } else { Some(tools) },
            top_logprobs: None,
            top_p: req.top_p,
            truncation: None,
        }
    }
}

/// Reasoning items stay top-level siblings rather than folding into the assistant, so the input replays the model's original order.
pub(super) fn build_responses_input(req: &ConversationRequest) -> rs::InputParam {
    let items: Vec<rs::InputItem> = req
        .items
        .iter()
        .flat_map(conversation_item_to_input_items)
        .collect();
    rs::InputParam::Items(items)
}

/// Inject the `type: "reasoning_text"` discriminator the API requires.
/// `async-openai`'s `ReasoningTextContent` has no `type` field, so it serializes to `{"text": ...}` and the API answers 400.
/// Delete this once upstream grows the field.
pub fn patch_reasoning_text_types(body: &mut serde_json::Value) {
    let Some(input) = body.get_mut("input").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for item in input.iter_mut() {
        if item.get("type").and_then(|t| t.as_str()) != Some("reasoning") {
            continue;
        }
        let Some(content) = item.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for c in content.iter_mut() {
            if let Some(obj) = c.as_object_mut() {
                obj.entry("type")
                    .or_insert_with(|| serde_json::Value::String("reasoning_text".into()));
            }
        }
    }
}

/// 序列化后恢复 Responses 历史；不影响其他请求字段及跨协议转换。
pub fn patch_responses_input(body: &mut serde_json::Value, req: &ConversationRequest) {
    let projection = project_responses_history(req, None, true);
    apply_responses_projection(body, projection);
}

/// 仅启用且来源、完整本地前缀均匹配时重放最新安全压缩点。
/// `None` 表示关闭原生压缩，使用通用历史。
pub fn patch_responses_input_for_source(
    body: &mut serde_json::Value,
    req: &ConversationRequest,
    source: Option<&str>,
) {
    apply_responses_projection(body, project_responses_input(req, source));
}

/// 检查裁剪边界两侧是否产生孤立调用或结果。
fn responses_calls_paired(input: &[serde_json::Value]) -> bool {
    let mut pending = std::collections::HashSet::new();
    for item in input {
        match item.get("type").and_then(|v| v.as_str()) {
            Some("function_call") => {
                let Some(id) = item.get("call_id").and_then(|v| v.as_str()) else {
                    return false;
                };
                if !pending.insert(id) {
                    return false;
                }
            }
            Some("function_call_output") => {
                let Some(id) = item.get("call_id").and_then(|v| v.as_str()) else {
                    return false;
                };
                if !pending.remove(id) {
                    return false;
                }
            }
            _ => {}
        }
    }
    pending.is_empty()
}

/// 检查响应自身的 sibling 未被删除或改写，不能由原始快照恢复。
fn responses_siblings_match(raw: &[serde_json::Value], siblings: &[serde_json::Value]) -> bool {
    let expected: Vec<_> = raw
        .iter()
        .filter(|entry| responses_owned_sibling(entry))
        .filter_map(responses_output_to_input)
        .collect();
    expected == siblings
}

/// SDK 可表达且作为本地独立历史保存的输出。
fn responses_owned_sibling(entry: &serde_json::Value) -> bool {
    matches!(
        entry.get("type").and_then(|v| v.as_str()),
        Some("reasoning" | "web_search_call" | "code_interpreter_call")
    ) || entry.get("type").and_then(|v| v.as_str()) == Some("custom_tool_call")
        && entry
            .get("name")
            .and_then(|v| v.as_str())
            .is_some_and(is_responses_x_search_tool)
}

/// 原始历史兼容投影与来源校验共用排序逻辑。
fn project_responses_history(
    req: &ConversationRequest,
    source: Option<&str>,
    legacy: bool,
) -> ResponsesInputProjection {
    let mut input = Vec::new();
    let mut checkpoints = Vec::new();
    let mut records = Vec::new();
    let mut siblings = Vec::new();
    let mut sibling_start = None;
    let mut history_fingerprint = None;
    let mut compaction_candidates = 0;
    let mut compaction_rejections = std::collections::BTreeMap::<String, usize>::new();
    for (index, item) in req.items.iter().enumerate() {
        if matches!(
            item,
            ConversationItem::Reasoning(_) | ConversationItem::BackendToolCall(_)
        ) {
            sibling_start.get_or_insert(index);
            siblings.extend(
                conversation_item_to_input_items(item)
                    .into_iter()
                    .filter_map(|item| serde_json::to_value(item).ok()),
            );
            continue;
        }
        let candidate_count = match item {
            ConversationItem::Assistant(assistant) => {
                assistant.responses_output.as_ref().map_or(0, |raw| {
                    raw.iter()
                        .filter(|entry| {
                            entry.get("type").and_then(|v| v.as_str()) == Some("compaction")
                        })
                        .count()
                })
            }
            _ => 0,
        };
        compaction_candidates += candidate_count;
        if let ConversationItem::Assistant(assistant) = item
            && let Some(raw) = &assistant.responses_output
            && (legacy || source.is_some())
            && responses_output_matches_assistant(raw, assistant)
        {
            let rejection = match source {
                None => Some("native_disabled"),
                Some(source) if assistant.responses_source.as_deref() != Some(source) => {
                    Some("source_changed")
                }
                Some(source) => {
                    let matches = assistant
                        .responses_input_fingerprint
                        .as_deref()
                        .is_some_and(|expected| {
                            history_fingerprint
                                .get_or_insert_with(|| ResponsesHistoryFingerprint::new(source))
                                .prefix(&req.items[..sibling_start.unwrap_or(index)])
                                == expected
                        });
                    if !matches {
                        Some("history_changed")
                    } else if !responses_siblings_match(raw, &siblings) {
                        Some("siblings_changed")
                    } else {
                        None
                    }
                }
            };
            let valid_checkpoint = rejection.is_none();
            if candidate_count > 0
                && let Some(reason) = rejection
            {
                *compaction_rejections.entry(reason.to_owned()).or_default() += candidate_count;
            }
            if valid_checkpoint && let Some(record) = context::assistant_record(assistant) {
                records.push(record);
            }
            if !legacy
                && !valid_checkpoint
                && raw
                    .iter()
                    .any(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("compaction"))
            {
                input.append(&mut siblings);
                input.extend(
                    conversation_item_to_input_items(item)
                        .into_iter()
                        .filter_map(|item| serde_json::to_value(item).ok()),
                );
                sibling_start = None;
                continue;
            }
            // 原有 sibling 是裁剪后的权威数据；原始输出只恢复其顺序。
            let mut replay = Vec::new();
            let mut replay_checkpoints = Vec::new();
            for (raw_index, entry) in raw.iter().enumerate() {
                let kind = entry.get("type").and_then(|v| v.as_str());
                let owned_sibling = matches!(
                    kind,
                    Some("reasoning" | "web_search_call" | "code_interpreter_call")
                ) || kind == Some("custom_tool_call")
                    && entry
                        .get("name")
                        .and_then(|v| v.as_str())
                        .is_some_and(is_responses_x_search_tool);
                if owned_sibling {
                    if let Some(position) =
                        siblings.iter().position(|sibling: &serde_json::Value| {
                            entry.get("type") == sibling.get("type")
                                && entry.get("id").is_some()
                                && entry.get("id") == sibling.get("id")
                        })
                    {
                        replay.push(siblings.remove(position));
                    }
                } else if kind == Some("compaction") {
                    if valid_checkpoint && let Some(item) = responses_output_to_input(entry) {
                        let key = context::checkpoint_key(
                            source.unwrap_or_default(),
                            assistant,
                            raw_index,
                            &item,
                        );
                        let old_usage = context::legacy_context_usage(entry);
                        replay_checkpoints.push((replay.len(), key, index, raw_index, old_usage));
                        replay.push(item);
                    }
                } else if let Some(item) = responses_output_to_input(entry) {
                    replay.push(item);
                }
            }
            input.append(&mut siblings);
            checkpoints.extend(replay_checkpoints.into_iter().map(
                |(offset, key, owner, raw_index, usage)| {
                    (input.len() + offset, key, owner, raw_index, usage)
                },
            ));
            input.append(&mut replay);
        } else {
            if candidate_count > 0 {
                let reason = if source.is_none() {
                    "native_disabled"
                } else {
                    "output_changed"
                };
                *compaction_rejections.entry(reason.to_owned()).or_default() += candidate_count;
            }
            input.append(&mut siblings);
            input.extend(
                conversation_item_to_input_items(item)
                    .into_iter()
                    .filter_map(|item| serde_json::to_value(item).ok()),
            );
        }
        sibling_start = None;
    }
    input.append(&mut siblings);
    // 只保留所选边界的加密项，其他来源或较旧压缩项均不发送。
    let is_compaction =
        |item: &serde_json::Value| item.get("type").and_then(|v| v.as_str()) == Some("compaction");
    let mut selected = None;
    let mut checkpoint_key = None;
    let mut selected_assistant = None;
    let mut selected_raw_index = None;
    let mut legacy_usage = None;
    let mut checkpoint_tail_start = None;
    for (boundary, key, owner, raw_index, usage) in checkpoints.into_iter().rev() {
        let prefix: Vec<_> = input[..boundary]
            .iter()
            .filter(|item| !is_compaction(item))
            .cloned()
            .collect();
        let suffix: Vec<_> = input[boundary..]
            .iter()
            .enumerate()
            .filter(|(offset, item)| *offset == 0 || !is_compaction(item))
            .map(|(_, item)| item.clone())
            .collect();
        if responses_calls_paired(&prefix) && responses_calls_paired(&suffix) {
            let mut projected: Vec<_> = input[..boundary]
                .iter()
                .filter(|item| item.get("role").and_then(|v| v.as_str()) == Some("system"))
                .cloned()
                .collect();
            checkpoint_tail_start = Some(projected.len() + 1);
            projected.extend(suffix);
            checkpoint_key = Some(key);
            selected_assistant = Some(owner);
            selected_raw_index = Some(raw_index);
            legacy_usage = usage;
            selected = Some(projected);
            break;
        }
        *compaction_rejections
            .entry("tool_boundary_unpaired".to_owned())
            .or_default() += 1;
    }
    if let Some(projected) = selected {
        input = projected;
    } else {
        input.retain(|item| !is_compaction(item));
    }
    let mut body = serde_json::json!({"input": input});
    patch_reasoning_text_types(&mut body);
    context::clear_context_fields(&mut body);
    let input = body
        .get_mut("input")
        .and_then(serde_json::Value::as_array_mut)
        .map(std::mem::take)
        .unwrap_or_default();
    ResponsesInputProjection {
        input,
        checkpoint_key,
        compaction_candidates,
        compaction_rejections,
        source: source.map(str::to_owned),
        records,
        selected_assistant,
        selected_raw_index,
        legacy_usage,
        checkpoint_tail_start,
    }
}

/// 对完整准备历史求有效投影；不修改通用历史或请求的其他字段。
pub fn project_responses_input(
    req: &ConversationRequest,
    source: Option<&str>,
) -> ResponsesInputProjection {
    project_responses_history(req, source.filter(|s| !s.is_empty()), false)
}

/// 旧补丁入口与纯投影共用同一份合法 input。
fn apply_responses_projection(body: &mut serde_json::Value, projection: ResponsesInputProjection) {
    if let Some(object) = body.as_object_mut() {
        object.insert("input".into(), serde_json::Value::Array(projection.input));
    }
}

/// 只放行已知输入类型；原始未知数据只持久化，不猜测协议语义。
fn responses_output_to_input(raw: &serde_json::Value) -> Option<serde_json::Value> {
    use serde_json::{Value, json};
    let kind = raw.get("type")?.as_str()?;
    match kind {
        "message" => {
            if raw.get("role")?.as_str()? != "assistant" {
                return None;
            }
            let content: Vec<_> = raw.get("content")?.as_array()?.iter().filter_map(|part| {
                match part.get("type")?.as_str()? {
                    "output_text" => Some(json!({"type":"output_text", "text":part.get("text")?.as_str()?,
                        "annotations":part.get("annotations").cloned().unwrap_or_else(|| json!([]))})),
                    "refusal" => Some(json!({"type":"refusal", "refusal":part.get("refusal")?.as_str()?})),
                    _ => None,
                }
            }).collect();
            if content.is_empty() {
                return None;
            }
            let mut message = json!({"type":"message", "role":"assistant", "content":content});
            for key in ["id", "phase", "status"] {
                if let Some(value) = raw.get(key) {
                    message[key] = value.clone();
                }
            }
            Some(message)
        }
        "function_call" => {
            let id = raw.get("call_id")?.as_str()?;
            let name = raw.get("name")?.as_str()?;
            let arguments =
                sanitize_tool_arguments(id, name, Arc::from(raw.get("arguments")?.as_str()?));
            let mut call = json!({"type":"function_call", "call_id":id, "name":name, "arguments":arguments.as_ref()});
            for key in ["id", "namespace", "caller"] {
                if let Some(value) = raw.get(key) {
                    call[key] = value.clone();
                }
            }
            Some(call)
        }
        "compaction" => {
            let encrypted = raw.get("encrypted_content")?.as_str()?;
            if encrypted.trim().is_empty() {
                return None;
            }
            let mut item = json!({"type":"compaction", "encrypted_content":encrypted});
            if let Some(id) = raw.get("id") {
                item["id"] = id.clone();
            }
            Some(item)
        }
        "reasoning"
        | "web_search_call"
        | "file_search_call"
        | "code_interpreter_call"
        | "image_generation_call"
        | "mcp_call"
        | "mcp_list_tools"
        | "mcp_approval_request" => {
            // SDK 输入模型负责过滤输出专用及未知字段。
            let mut value = raw.clone();
            context::clear_context_fields(&mut value);
            if kind == "reasoning" {
                value.as_object_mut()?.remove("status");
            }
            let item = serde_json::from_value::<rs::Item>(value).ok()?;
            serde_json::to_value(item).ok()
        }
        "custom_tool_call"
            if raw
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(is_responses_x_search_tool) =>
        {
            let mut value = raw.clone();
            context::clear_context_fields(&mut value);
            serde_json::from_value::<rs::Item>(value)
                .ok()
                .and_then(|item| serde_json::to_value(item).ok())
        }
        _ => None,
    }
}

fn conversation_item_to_input_items(item: &ConversationItem) -> Vec<rs::InputItem> {
    match item {
        ConversationItem::System(s) => {
            vec![rs::InputItem::EasyMessage(rs::EasyInputMessage {
                r#type: rs::MessageType::Message,
                role: rs::Role::System,
                content: rs::EasyInputContent::Text(s.content.as_ref().to_owned()),
            })]
        }
        ConversationItem::User(u) => {
            let content = content_parts_to_easy_input_content(&u.content);
            vec![rs::InputItem::EasyMessage(rs::EasyInputMessage {
                r#type: rs::MessageType::Message,
                role: rs::Role::User,
                content,
            })]
        }
        ConversationItem::Reasoning(r) => {
            // `status` is output-only and rejected on input.
            let mut r = r.clone();
            r.status = None;
            vec![rs::InputItem::Item(rs::Item::Reasoning(r))]
        }
        ConversationItem::Assistant(a) => {
            let mut items = Vec::new();

            if !a.content.is_empty() {
                items.push(rs::InputItem::EasyMessage(rs::EasyInputMessage {
                    r#type: rs::MessageType::Message,
                    role: rs::Role::Assistant,
                    content: rs::EasyInputContent::Text(a.content.as_ref().to_owned()),
                }));
            }

            for tc in &a.tool_calls {
                let arguments = sanitize_tool_arguments(&tc.id, &tc.name, tc.arguments.clone());
                items.push(rs::InputItem::Item(rs::Item::FunctionCall(
                    rs::FunctionToolCall {
                        call_id: tc.id.as_ref().to_owned(),
                        name: tc.name.clone(),
                        arguments: arguments.as_ref().to_owned(),
                        id: None,
                        status: None,
                    },
                )));
            }

            items
        }
        ConversationItem::ToolResult(t) => {
            let output = if t.images.is_empty() {
                rs::FunctionCallOutput::Text(t.content.as_ref().to_owned())
            } else {
                let mut parts: Vec<rs::InputContent> =
                    vec![rs::InputContent::InputText(rs::InputTextContent {
                        text: t.content.as_ref().to_owned(),
                    })];
                for img in &t.images {
                    if let ContentPart::Image { url } = img {
                        parts.push(rs::InputContent::InputImage(rs::InputImageContent {
                            detail: rs::ImageDetail::Auto,
                            file_id: None,
                            image_url: Some(url.as_ref().to_owned()),
                        }));
                    }
                }
                rs::FunctionCallOutput::Content(parts)
            };
            vec![rs::InputItem::Item(rs::Item::FunctionCallOutput(
                rs::FunctionCallOutputItemParam {
                    call_id: t.tool_call_id.clone(),
                    output,
                    id: None,
                    status: None,
                },
            ))]
        }
        ConversationItem::BackendToolCall(b) => {
            vec![match &b.kind {
                BackendToolKind::WebSearch(ws) => {
                    rs::InputItem::Item(rs::Item::WebSearchCall(ws.clone()))
                }
                BackendToolKind::XSearch(ct) => {
                    rs::InputItem::Item(rs::Item::CustomToolCall(ct.clone()))
                }
                BackendToolKind::CodeInterpreter(ci) => {
                    rs::InputItem::Item(rs::Item::CodeInterpreterCall(ci.clone()))
                }
            }]
        }
    }
}

fn content_parts_to_easy_input_content(parts: &[ContentPart]) -> rs::EasyInputContent {
    if let [ContentPart::Text { text }] = parts {
        return rs::EasyInputContent::Text(text.as_ref().to_owned());
    }

    let items: Vec<rs::InputContent> = parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => rs::InputContent::InputText(rs::InputTextContent {
                text: text.as_ref().to_owned(),
            }),
            ContentPart::Image { url } => rs::InputContent::InputImage(rs::InputImageContent {
                image_url: Some(url.as_ref().to_owned()),
                file_id: None,
                detail: rs::ImageDetail::default(),
            }),
        })
        .collect();

    rs::EasyInputContent::ContentList(items)
}

/// The request's client function tools.
/// A function tool whose name collides with a backend-hosted tool is dropped: sending both is rejected as a duplicate, so the hosted tool wins.
/// Both ride the raw-JSON [`extra_tool_entries`] channel instead.
fn build_responses_tools(req: &ConversationRequest) -> Vec<rs::Tool> {
    let tools: Vec<rs::Tool> = req
        .tools
        .iter()
        .filter(|t| {
            let collides = req.hosted_tools.iter().any(|h| h.wire_name() == t.name);
            if collides {
                tracing::warn!(
                    tool = %t.name,
                    "dropping function tool that collides with a backend-hosted tool"
                );
            }
            !collides
        })
        .map(|t| {
            rs::Tool::Function(rs::FunctionTool {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: Some(t.parameters.clone()),
                strict: None,
            })
        })
        .collect();

    tools
}

/// Every hosted tool as a raw JSON entry, which the sampler client splices into the serialized `tools` array.
/// `web_search` rides it because async_openai's `rs::WebSearchToolFilters` models only `allowed_domains` and cannot carry `excluded_domains`.
/// Emitting either as a typed `rs::Tool` as well would send it twice, which the API rejects as a duplicate.
pub fn extra_tool_entries(hosted_tools: &[HostedTool]) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();
    for tool in hosted_tools {
        match tool {
            HostedTool::WebSearch { options } => {
                entries.push(match options {
                    Some(o) => o.to_tool_entry(),
                    None => WebSearchOptions::default().to_tool_entry(),
                });
            }
            HostedTool::XSearch { options } => {
                entries.push(match options {
                    Some(o) => o.to_tool_entry(),
                    None => XSearchOptions::default().to_tool_entry(),
                });
            }
        }
    }
    entries
}
