//! 原生 Responses 请求投影、容量保护和完整响应检查点。

#[cfg(test)]
#[path = "native_responses_tests.rs"]
mod tests;

use std::num::NonZeroU64;

use serde_json::Value;
use xai_grok_sampling_types::{
    ApiErrorCode, ConversationRequest, RESPONSE_CONTEXT_CHECKPOINT_FIELD,
    RESPONSE_INPUT_FINGERPRINT_METADATA_KEY, RESPONSE_RAW_OUTPUT_METADATA_KEY,
    RESPONSE_SOURCE_METADATA_KEY, ResponseModelMetadata, Result, SamplingError,
    checkpoint_responses_context, project_responses_input, responses_input_fingerprint,
    responses_projection_context, rs,
};

use super::SamplingClient;

/// 本地原生容量保护的稳定错误前缀。
pub const NATIVE_CONTEXT_OVERFLOW_PREFIX: &str = "原生压缩请求超出上下文窗口";

/// 记录实际发送时的历史、认证来源及工具，避免响应绑定到后续变化的状态。
pub(super) struct PreparedResponses {
    history: Box<ConversationRequest>,
    source: String,
    fingerprint: String,
    tools: Value,
    threshold: NonZeroU64,
}

impl SamplingClient {
    /// 在实际认证确定且请求默认值生效后，统一准备两种传输方式的原生请求。
    pub(super) fn prepare_responses_request(
        &self,
        builder: &reqwest::RequestBuilder,
        history: Option<Box<ConversationRequest>>,
        body: &mut Value,
    ) -> Result<Option<PreparedResponses>> {
        let Some(threshold) = self.defaults.responses_compact_threshold else {
            if let Some(history) = history {
                xai_grok_sampling_types::patch_responses_input(body, &history);
            }
            return Ok(None);
        };
        body["context_management"] = serde_json::json!([
            {"type": "compaction", "compact_threshold": threshold.get()}
        ]);
        // 辅助或直接 SDK 调用没有会话历史，不参与本地原生容量保护及检查点签发。
        let Some(mut history) = history else {
            return Ok(None);
        };
        self.apply_conversation_defaults(&mut history)?;
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let source = self.responses_source(builder, model)?;
        let projection = project_responses_input(&history, Some(&source));
        let tools = body
            .get("tools")
            .filter(|tools| !tools.is_null())
            .cloned()
            .unwrap_or_else(|| serde_json::json!([]));
        let usage = responses_projection_context(&projection, &tools);
        let reserve = body
            .get("max_output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let window = self.defaults.context_window;
        if usage.tokens > window || reserve > window.saturating_sub(usage.tokens) {
            return Err(SamplingError::Api {
                status: reqwest::StatusCode::BAD_REQUEST,
                message: format!(
                    "{NATIVE_CONTEXT_OVERFLOW_PREFIX}：输入与工具 {} tokens，输出预留 {reserve} tokens，窗口 {window} tokens",
                    usage.tokens,
                ),
                model_metadata: Some(ResponseModelMetadata {
                    context_window: Some(window),
                    ..Default::default()
                }),
                retry_after_secs: None,
                should_retry: Some(false),
                error_code: Some(ApiErrorCode::ContextOverflow(
                    "context_length_exceeded".into(),
                )),
            });
        }
        body["input"] = Value::Array(projection.input);
        let fingerprint = responses_input_fingerprint(&history, &source);
        Ok(Some(PreparedResponses {
            history,
            source,
            fingerprint,
            tools,
            threshold,
        }))
    }
}

/// 清除线上响应中的客户端专用标识，随后由兼容层重新生成 raw output。
pub(super) fn sanitize_wire_response(response: &mut Value) {
    if let Some(metadata) = response.get_mut("metadata").and_then(Value::as_object_mut) {
        for key in [
            RESPONSE_SOURCE_METADATA_KEY,
            RESPONSE_INPUT_FINGERPRINT_METADATA_KEY,
            RESPONSE_RAW_OUTPUT_METADATA_KEY,
            RESPONSE_CONTEXT_CHECKPOINT_FIELD,
            "__grok_context_tokens",
            "__grok_context_estimated",
        ] {
            metadata.remove(key);
        }
    }
    if let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) {
        for item in output {
            sanitize_output_item(item);
        }
    }
}

/// 内部上下文标记只允许客户端签发，旧版本计数也不能直接信任上游。
fn sanitize_output_item(item: &mut Value) {
    match item {
        Value::Object(object) => {
            object.remove(RESPONSE_CONTEXT_CHECKPOINT_FIELD);
            object.remove("__grok_context_tokens");
            object.remove("__grok_context_estimated");
            for value in object.values_mut() {
                sanitize_output_item(value);
            }
        }
        Value::Array(items) => {
            for item in items {
                sanitize_output_item(item);
            }
        }
        _ => {}
    }
}

/// 仅完整成功响应绑定来源并记录上下文；失败及中断响应始终不签发检查点。
pub(super) fn finish_response(
    response: &mut rs::Response,
    prepared: Option<&PreparedResponses>,
    context_tokens: Option<u64>,
) {
    if let Some(metadata) = response.metadata.as_mut() {
        metadata.remove(RESPONSE_SOURCE_METADATA_KEY);
        metadata.remove(RESPONSE_INPUT_FINGERPRINT_METADATA_KEY);
        metadata.remove(RESPONSE_CONTEXT_CHECKPOINT_FIELD);
        metadata.remove("__grok_context_tokens");
        metadata.remove("__grok_context_estimated");
        if let Some(raw) = metadata.get_mut(RESPONSE_RAW_OUTPUT_METADATA_KEY) {
            if let Ok(mut output) = serde_json::from_str::<Vec<Value>>(raw) {
                for item in &mut output {
                    sanitize_output_item(item);
                }
                *raw = serde_json::to_string(&output).expect("JSON 输出可以序列化");
            }
        }
    }
    let Some(prepared) = prepared.filter(|_| matches!(response.status, rs::Status::Completed))
    else {
        return;
    };
    let metadata = response.metadata.get_or_insert_with(Default::default);
    metadata.insert(RESPONSE_SOURCE_METADATA_KEY.into(), prepared.source.clone());
    metadata.insert(
        RESPONSE_INPUT_FINGERPRINT_METADATA_KEY.into(),
        prepared.fingerprint.clone(),
    );
    let _ = checkpoint_responses_context(
        response,
        &prepared.history,
        &prepared.source,
        &prepared.tools,
        context_tokens,
        prepared.threshold,
    );
}

/// 读取原始线上上下文计数，独立于用于计费的 usage.total_tokens。
pub(super) fn context_tokens(response: &Value) -> Option<u64> {
    let context = response.pointer("/usage/context_details")?;
    Some(
        context
            .get("input_tokens")?
            .as_u64()?
            .saturating_add(context.get("output_tokens")?.as_u64()?),
    )
}
