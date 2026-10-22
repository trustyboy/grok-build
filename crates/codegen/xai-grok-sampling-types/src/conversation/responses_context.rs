use super::*;

/// 客户端签发的上下文记录；只持久化，不进入线上 input。
pub const RESPONSE_CONTEXT_CHECKPOINT_FIELD: &str = "__grok_context_checkpoint";
const CONTEXT_RECORD_VERSION: u32 = 1;
const LEGACY_CONTEXT_TOKENS: &str = "__grok_context_tokens";
const LEGACY_CONTEXT_ESTIMATED: &str = "__grok_context_estimated";

/// 有效模型上下文用量，与累计账单分开保存。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponsesContextUsage {
    /// 包含工具定义的上下文 token 数。
    pub tokens: u64,
    /// 密文代理、追加内容或工具预算修正均属于估算。
    pub estimated: bool,
}

/// 完整准备历史的合法 Responses 投影；内部记录只供计数校验。
#[derive(Debug, Clone)]
pub struct ResponsesInputProjection {
    /// 可以直接发送的 input，不含客户端保留字段。
    pub input: Vec<serde_json::Value>,
    /// 实际通过来源、前缀和工具边界检查的压缩点身份。
    pub checkpoint_key: Option<String>,
    /// 历史中的压缩候选数量。
    pub compaction_candidates: usize,
    /// 压缩点失效原因计数，不包含历史正文。
    pub compaction_rejections: std::collections::BTreeMap<String, usize>,
    /// 实际请求来源，关闭原生模式时为空。
    pub(super) source: Option<String>,
    /// 外层来源、完整历史前缀和 sibling 已验证的候选记录。
    pub(super) records: Vec<ContextRecord>,
    /// 用于区分本次响应的新压缩点与历史压缩点。
    pub(super) selected_assistant: Option<usize>,
    /// 所选边界在原响应中的位置。
    pub(super) selected_raw_index: Option<usize>,
    /// 旧客户端保存在所选 compaction 上的用量。
    pub(super) legacy_usage: Option<ResponsesContextUsage>,
    /// 保留的 system 和 compaction 后的首个 input 下标。
    pub(super) checkpoint_tail_start: Option<usize>,
}

/// 记录覆盖响应完成后的有效 input；普通响应不重复存储 input。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ContextRecord {
    /// 不支持的版本保守失效。
    version: u32,
    /// 与外层客户端元数据双向绑定。
    source: String,
    /// 生成该响应的完整准备历史指纹。
    input_fingerprint: String,
    /// 无压缩点时为空，不能跨压缩点复用计数。
    checkpoint_key: Option<String>,
    /// 本记录覆盖的合法 input 前缀长度。
    input_len: usize,
    /// 仅对合法 input 求哈希，不包含内部记录。
    input_hash: String,
    /// 当前请求工具定义的哈希。
    tools_hash: String,
    /// 工具定义变化时从总基线扣除的旧估算。
    tools_tokens: u64,
    /// 包含工具定义的完成后上下文用量，供严格投影预算使用。
    usage: ResponsesContextUsage,
    /// 本次成功响应的可信服务端计数，仅供显示，不参与投影预算。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reported_usage: Option<ResponsesContextUsage>,
    /// 仅新安全压缩点保存投影快照，普通响应省略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replacement_input: Option<Vec<serde_json::Value>>,
}

/// 带版本域的 JSON 摘要，避免不同用途的哈希相互混用。
fn context_hash<T: Serialize + ?Sized>(domain: &str, value: &T) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(serde_json::to_vec(&(domain, value)).expect("context is serializable"))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 剔除上游伪造的客户端字段，包括旧计数格式。
pub(super) fn clear_context_fields(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            object.remove(RESPONSE_CONTEXT_CHECKPOINT_FIELD);
            object.remove(LEGACY_CONTEXT_TOKENS);
            object.remove(LEGACY_CONTEXT_ESTIMATED);
            for value in object.values_mut() {
                clear_context_fields(value);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                clear_context_fields(item);
            }
        }
        _ => {}
    }
}

/// 稳定绑定来源、原请求前缀和响应内边界，不依赖服务端 id。
pub(super) fn checkpoint_key(
    source: &str,
    assistant: &AssistantItem,
    raw_index: usize,
    item: &serde_json::Value,
) -> String {
    context_hash(
        "responses-checkpoint-v1",
        &(
            source,
            &assistant.responses_input_fingerprint,
            raw_index,
            item,
        ),
    )
}

/// 本地记录必须绑定同一个成功响应的客户端来源和历史指纹。
pub(super) fn assistant_record(assistant: &AssistantItem) -> Option<ContextRecord> {
    let record: ContextRecord = serde_json::from_value(
        assistant
            .responses_output
            .as_ref()?
            .first()?
            .get(RESPONSE_CONTEXT_CHECKPOINT_FIELD)?
            .clone(),
    )
    .ok()?;
    (record.version == CONTEXT_RECORD_VERSION
        && !record.source.is_empty()
        && !record.input_fingerprint.is_empty()
        && assistant.responses_source.as_deref() == Some(record.source.as_str())
        && assistant.responses_input_fingerprint.as_deref()
            == Some(record.input_fingerprint.as_str()))
    .then_some(record)
}

/// 兼容旧压缩项的计数；只有外层压缩点安全选择后才会使用。
pub(super) fn legacy_context_usage(item: &serde_json::Value) -> Option<ResponsesContextUsage> {
    Some(ResponsesContextUsage {
        tokens: item.get(LEGACY_CONTEXT_TOKENS)?.as_u64()?,
        estimated: item
            .get(LEGACY_CONTEXT_ESTIMATED)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
    })
}

/// 内容按约三字节一个 token 估算；图片不按 data URL 长度计数。
fn estimate_context_value(value: &serde_json::Value) -> u64 {
    use serde_json::Value;
    match value {
        Value::String(text) => (text.len() as u64).div_ceil(3),
        Value::Array(items) => items.iter().fold(0u64, |sum, item| {
            sum.saturating_add(estimate_context_value(item))
        }),
        Value::Object(object) => {
            let kind = object.get("type").and_then(Value::as_str);
            if matches!(kind, Some("input_image" | "image_url" | "image")) {
                return 1_024;
            }
            let visible = object
                .iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "id" | "call_id"
                            | "type"
                            | "role"
                            | "status"
                            | "phase"
                            | "encrypted_content"
                    )
                })
                .fold(0u64, |sum, (_, value)| {
                    sum.saturating_add(estimate_context_value(value))
                });
            let encrypted = object
                .get("encrypted_content")
                .and_then(Value::as_str)
                .map_or(0, |text| (text.len() as u64).div_ceil(4));
            // 加密项只给出启发式代理，不能视为真实 token 或上界。
            visible.max(encrypted)
        }
        _ => 0,
    }
}

/// 直接遍历合法 input，避免为估算复制图片或密文。
fn estimate_input(items: &[serde_json::Value]) -> u64 {
    items.iter().fold(0u64, |sum, item| {
        sum.saturating_add(estimate_context_value(item))
    })
}

/// 工具定义单独记账，便于 schema 变化后修正历史基线。
fn estimate_tools(tools: &serde_json::Value) -> u64 {
    if tools.is_null() || tools.as_array().is_some_and(Vec::is_empty) {
        return 0;
    }
    (serde_json::to_vec(tools)
        .expect("tools are serializable")
        .len() as u64)
        .div_ceil(3)
}

/// 从最终合法 input 计算上下文；历史记录必须覆盖同来源、同压缩点的完整前缀。
pub fn responses_projection_context(
    projection: &ResponsesInputProjection,
    tools: &serde_json::Value,
) -> ResponsesContextUsage {
    let tools_hash = context_hash("responses-tools-v1", tools);
    let tools_tokens = estimate_tools(tools);
    for record in projection.records.iter().rev() {
        let Some(prefix) = projection.input.get(..record.input_len) else {
            continue;
        };
        if projection.source.as_deref() != Some(record.source.as_str())
            || projection.checkpoint_key != record.checkpoint_key
            || context_hash("responses-input-v1", prefix) != record.input_hash
            || record
                .replacement_input
                .as_deref()
                .is_some_and(|replacement| replacement != prefix)
        {
            continue;
        }
        let tail = projection.input.get(record.input_len..).unwrap_or_default();
        let tools_changed = tools_hash != record.tools_hash;
        let tokens = if tools_changed {
            record
                .usage
                .tokens
                .saturating_sub(record.tools_tokens)
                .saturating_add(tools_tokens)
        } else {
            record.usage.tokens
        };
        return ResponsesContextUsage {
            tokens: tokens.saturating_add(estimate_input(tail)),
            estimated: record.usage.estimated || !tail.is_empty() || tools_changed,
        };
    }
    if let Some(usage) = projection.legacy_usage
        && let Some(tail_start) = projection.checkpoint_tail_start
    {
        let prefix_end = tail_start.saturating_sub(1);
        let prefix = projection.input.get(..prefix_end).unwrap_or_default();
        let tail = projection.input.get(tail_start..).unwrap_or_default();
        return ResponsesContextUsage {
            tokens: usage
                .tokens
                .saturating_add(estimate_input(prefix))
                .saturating_add(estimate_input(tail))
                .saturating_add(tools_tokens),
            estimated: usage.estimated
                || !prefix.is_empty()
                || !tail.is_empty()
                || tools_tokens > 0,
        };
    }
    ResponsesContextUsage {
        tokens: estimate_input(&projection.input).saturating_add(tools_tokens),
        estimated: true,
    }
}

/// 读取适配器原始输出；旧调用方没有原始通道时退回 SDK 输出。
fn response_raw_output(response: &rs::Response) -> Vec<serde_json::Value> {
    response
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(RESPONSE_RAW_OUTPUT_METADATA_KEY))
        .and_then(|raw| serde_json::from_str(raw).ok())
        .or_else(|| {
            serde_json::to_value(&response.output)
                .ok()?
                .as_array()
                .cloned()
        })
        .unwrap_or_default()
}

/// 写回唯一持久化通道，确保转换后的 Assistant 携带相同记录。
fn write_raw_output(response: &mut rs::Response, raw: &[serde_json::Value]) {
    response
        .metadata
        .get_or_insert_with(Default::default)
        .insert(
            RESPONSE_RAW_OUTPUT_METADATA_KEY.to_owned(),
            serde_json::to_string(raw).expect("raw output is serializable"),
        );
}

/// 成功响应完成后签发上下文记录；调用前必须由客户端覆盖来源与输入指纹。
/// 返回安全投影预算，服务端显示计数单独保存。
pub fn checkpoint_responses_context(
    response: &mut rs::Response,
    history: &ConversationRequest,
    source: &str,
    tools: &serde_json::Value,
    context_tokens: Option<u64>,
    threshold: std::num::NonZeroU64,
) -> Option<ResponsesContextUsage> {
    checkpoint_responses_context_for_input(
        response,
        history,
        source,
        tools,
        context_tokens,
        threshold,
        None,
    )
}

/// 将图片预算后的实际 input 纳入计数锚点，归档指纹仍绑定完整历史。
pub fn checkpoint_responses_context_for_input(
    response: &mut rs::Response,
    history: &ConversationRequest,
    source: &str,
    tools: &serde_json::Value,
    context_tokens: Option<u64>,
    threshold: std::num::NonZeroU64,
    sent_input: Option<&[serde_json::Value]>,
) -> Option<ResponsesContextUsage> {
    let mut raw = response_raw_output(response);
    for item in &mut raw {
        clear_context_fields(item);
    }
    write_raw_output(response, &raw);
    if serde_json::to_value(&response.status).ok()?.as_str() != Some("completed")
        || source.is_empty()
        || response
            .metadata
            .as_ref()?
            .get(RESPONSE_SOURCE_METADATA_KEY)
            .map(String::as_str)
            != Some(source)
        || response
            .metadata
            .as_ref()?
            .get(RESPONSE_INPUT_FINGERPRINT_METADATA_KEY)
            != Some(&responses_input_fingerprint(history, source))
    {
        return None;
    }
    let mut completed_history = history.clone();
    completed_history
        .items
        .extend(response_to_conversation_items(response.clone()));
    let mut projection = project_responses_input(&completed_history, Some(source));
    let has_compaction = raw
        .iter()
        .any(|item| item.get("type").and_then(serde_json::Value::as_str) == Some("compaction"));
    let selected_new = projection
        .selected_assistant
        .is_some_and(|index| index >= history.items.len());
    let last_raw_compaction = raw.iter().rposition(|item| {
        item.get("type").and_then(serde_json::Value::as_str) == Some("compaction")
    });
    let selected_latest = selected_new && projection.selected_raw_index == last_raw_compaction;
    let previous = project_responses_input(history, Some(source));
    let same_prefix = projection.checkpoint_key == previous.checkpoint_key
        && projection.input.starts_with(&previous.input);
    if !selected_new
        && same_prefix
        && let Some(sent_input) = sent_input
    {
        projection
            .input
            .splice(..previous.input.len(), sent_input.iter().cloned());
    }
    let lost_checkpoint = !selected_new
        && (projection.checkpoint_key != previous.checkpoint_key
            || sent_input.is_some() && !same_prefix);
    let usage = if (has_compaction && !selected_new) || lost_checkpoint {
        // 不安全的新点或未闭合工具尾部不能用低计数替换回退历史的预算。
        let mut usage = responses_projection_context(&projection, tools);
        usage.estimated = true;
        usage
    } else if let Some(tokens) = context_tokens.filter(|_| !has_compaction || selected_latest) {
        ResponsesContextUsage {
            tokens,
            estimated: false,
        }
    } else if selected_new {
        let tail_start = projection.checkpoint_tail_start?;
        let tail = projection.input.get(tail_start..).unwrap_or_default();
        let systems = projection
            .input
            .get(..tail_start.saturating_sub(1))
            .unwrap_or_default();
        ResponsesContextUsage {
            tokens: threshold
                .get()
                .saturating_add(estimate_input(systems))
                .saturating_add(estimate_input(tail))
                .saturating_add(estimate_tools(tools)),
            estimated: true,
        }
    } else if let Some(usage) = &response.usage {
        ResponsesContextUsage {
            tokens: u64::from(usage.total_tokens),
            estimated: false,
        }
    } else {
        responses_projection_context(&projection, tools)
    };
    let reported_usage = context_tokens
        .or_else(|| {
            if has_compaction {
                None
            } else {
                response
                    .usage
                    .as_ref()
                    .map(|usage| u64::from(usage.total_tokens))
            }
        })
        .map(|tokens| ResponsesContextUsage {
            tokens,
            estimated: false,
        });
    let record = ContextRecord {
        version: CONTEXT_RECORD_VERSION,
        source: source.to_owned(),
        input_fingerprint: responses_input_fingerprint(history, source),
        checkpoint_key: projection.checkpoint_key.clone(),
        input_len: projection.input.len(),
        input_hash: context_hash("responses-input-v1", &projection.input),
        tools_hash: context_hash("responses-tools-v1", tools),
        tools_tokens: estimate_tools(tools),
        usage,
        reported_usage,
        replacement_input: selected_new.then_some(projection.input),
    };
    if has_compaction && !selected_new {
        // 候选只保存压缩项本身的代理；工具尾部和定义在严格选中后另算。
        for item in &mut raw {
            if item.get("type").and_then(serde_json::Value::as_str) == Some("compaction")
                && responses_output_to_input(item).is_some()
                && let Some(object) = item.as_object_mut()
            {
                object.insert(LEGACY_CONTEXT_TOKENS.into(), threshold.get().into());
                object.insert(LEGACY_CONTEXT_ESTIMATED.into(), true.into());
            }
        }
    }
    // 空 output 也可校准用量；空对象只承载本地记录，不投影为模型消息。
    if raw.is_empty() {
        raw.push(serde_json::json!({}));
    }
    raw.first_mut()?.as_object_mut()?.insert(
        RESPONSE_CONTEXT_CHECKPOINT_FIELD.to_owned(),
        serde_json::to_value(record).expect("context record is serializable"),
    );
    write_raw_output(response, &raw);
    Some(usage)
}

/// 优先读取成功响应的服务端显示计数，旧记录回退安全预算。
/// 历史恢复的请求预算仍须通过投影重新校验。
pub fn assistant_context_usage(assistant: &AssistantItem) -> Option<ResponsesContextUsage> {
    let raw = assistant.responses_output.as_ref()?;
    if !responses_output_matches_assistant(raw, assistant) {
        return None;
    }
    let record = assistant_record(assistant)?;
    Some(record.reported_usage.unwrap_or(record.usage))
}

/// 供消费本次成功响应的调用方读取上下文用量，不替代账单 usage。
pub fn response_context_usage(response: &ConversationResponse) -> Option<ResponsesContextUsage> {
    assistant_context_usage(response.assistant()?)
}
