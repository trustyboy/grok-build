//! Responses 的扩展事件与原始输出兼容边界。

use serde_json::Value;
use xai_grok_sampling_types::{SamplingError, rs};

/// 当前 SDK 能表示的事件；已知事件仍执行严格字段校验。
const KNOWN_EVENTS: &[&str] = &[
    "response.created",
    "response.in_progress",
    "response.completed",
    "response.failed",
    "response.incomplete",
    "response.output_item.added",
    "response.output_item.done",
    "response.content_part.added",
    "response.content_part.done",
    "response.output_text.delta",
    "response.output_text.done",
    "response.refusal.delta",
    "response.refusal.done",
    "response.function_call_arguments.delta",
    "response.function_call_arguments.done",
    "response.file_search_call.in_progress",
    "response.file_search_call.searching",
    "response.file_search_call.completed",
    "response.web_search_call.in_progress",
    "response.web_search_call.searching",
    "response.web_search_call.completed",
    "response.reasoning_summary_part.added",
    "response.reasoning_summary_part.done",
    "response.reasoning_summary_text.delta",
    "response.reasoning_summary_text.done",
    "response.reasoning_text.delta",
    "response.reasoning_text.done",
    "response.image_generation_call.completed",
    "response.image_generation_call.generating",
    "response.image_generation_call.in_progress",
    "response.image_generation_call.partial_image",
    "response.mcp_call_arguments.delta",
    "response.mcp_call_arguments.done",
    "response.mcp_call.completed",
    "response.mcp_call.failed",
    "response.mcp_call.in_progress",
    "response.mcp_list_tools.completed",
    "response.mcp_list_tools.failed",
    "response.mcp_list_tools.in_progress",
    "response.code_interpreter_call.in_progress",
    "response.code_interpreter_call.interpreting",
    "response.code_interpreter_call.completed",
    "response.code_interpreter_call_code.delta",
    "response.code_interpreter_call_code.done",
    "response.output_text.annotation.added",
    "response.queued",
    "response.custom_tool_call_input.delta",
    "response.custom_tool_call_input.done",
    "error",
];

/// SDK 已建模的输出类型；字段损坏不能按未知类型略过。
const KNOWN_ITEMS: &[&str] = &[
    "message",
    "file_search_call",
    "function_call",
    "web_search_call",
    "computer_call",
    "reasoning",
    "compaction",
    "image_generation_call",
    "code_interpreter_call",
    "local_shell_call",
    "shell_call",
    "shell_call_output",
    "apply_patch_call",
    "apply_patch_call_output",
    "mcp_call",
    "mcp_list_tools",
    "mcp_approval_request",
    "custom_tool_call",
];

/// 未实现的客户端执行能力必须明确失败，防止跳过调用后继续推理。
fn check_execution(item: &Value) -> Result<(), SamplingError> {
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
    let unsupported = match kind {
        "computer_call" | "local_shell_call" | "apply_patch_call" | "mcp_approval_request" => true,
        "shell_call" => {
            item.pointer("/environment/type").and_then(Value::as_str) != Some("container_reference")
        }
        "tool_search_call" => item.get("execution").and_then(Value::as_str) != Some("server"),
        "custom_tool_call" => !item
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| {
                matches!(
                    name,
                    "x_search"
                        | "x_keyword_search"
                        | "x_semantic_search"
                        | "x_user_search"
                        | "x_thread_fetch"
                )
            }),
        "function_call" => {
            item.get("namespace")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty() && s != "functions")
                || item
                    .pointer("/caller/type")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s != "direct")
        }
        _ => {
            !KNOWN_ITEMS.contains(&kind)
                && kind.ends_with("_call")
                && item.get("execution").and_then(Value::as_str) != Some("server")
        }
    };
    if unsupported {
        return Err(SamplingError::Api {
            status: reqwest::StatusCode::BAD_REQUEST,
            message: format!("当前客户端尚未支持 Responses 执行能力：{kind}；调用未执行"),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: Some(false),
            error_code: None,
        });
    }
    Ok(())
}

/// 仅将错误码中的 JSON 数字转为文本，其他字段继续严格校验。
fn prepare_error_code(error: &mut Value) {
    if let Some(code) = error.get_mut("code") {
        if let Value::Number(number) = code {
            *code = Value::String(number.to_string());
        }
    }
}

/// 保留原始输出，再构造 SDK 能消费的投影；原始数据由历史转换层保存。
pub(crate) fn prepare_response(response: &mut Value) -> Result<(), SamplingError> {
    if let Some(error) = response.get_mut("error") {
        prepare_error_code(error);
    }
    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for item in output {
            check_execution(item)?;
        }
        let raw = serde_json::to_string(output).map_err(SamplingError::Serialization)?;
        let object = response
            .as_object_mut()
            .expect("response with output is an object");
        let metadata = object
            .entry("metadata")
            .or_insert_with(|| serde_json::json!({}));
        if metadata.is_null() {
            *metadata = serde_json::json!({});
        }
        if let Some(metadata) = metadata.as_object_mut() {
            metadata.insert(
                xai_grok_sampling_types::RESPONSE_RAW_OUTPUT_METADATA_KEY.to_string(),
                Value::String(raw),
            );
        }
        if let Some(output) = object.get_mut("output").and_then(Value::as_array_mut) {
            output.retain(|item| {
                item.get("type")
                    .and_then(Value::as_str)
                    .is_none_or(|kind| KNOWN_ITEMS.contains(&kind))
            });
            for item in output {
                prepare_item(item);
            }
        }
    }
    if let Some(tools) = response.get_mut("tools").and_then(Value::as_array_mut) {
        tools.retain(|tool| serde_json::from_value::<rs::Tool>(tool.clone()).is_ok());
    }
    Ok(())
}

/// 消息中的新内容保留于原始输出，通用投影只消费已有文字和拒绝。
fn prepare_item(item: &mut Value) {
    if item.get("type").and_then(Value::as_str) == Some("message") {
        if let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) {
            content.retain(|part| {
                part.get("type")
                    .and_then(Value::as_str)
                    .is_none_or(|kind| matches!(kind, "output_text" | "refusal"))
            });
        }
    }
}

/// 扩展进度事件不影响终止判定；仅已知业务事件进入 SDK 消费链。
pub(crate) fn prepare_event(mut event: Value) -> Result<Option<Value>, SamplingError> {
    let Some(kind) = event.get("type").and_then(Value::as_str) else {
        return Ok(Some(event));
    };
    if !KNOWN_EVENTS.contains(&kind) {
        tracing::warn!(
            event_type = kind,
            "已接收 Responses 扩展事件，当前无对应消费功能"
        );
        return Ok(None);
    }
    if kind == "error" {
        prepare_error_code(&mut event);
    }
    if let Some(item) = event.get_mut("item") {
        check_execution(item)?;
        if item
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| !KNOWN_ITEMS.contains(&kind))
        {
            return Ok(None);
        }
        prepare_item(item);
    }
    if let Some(part) = event.get("part") {
        if part
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                !matches!(
                    kind,
                    "output_text" | "refusal" | "reasoning_text" | "summary_text"
                )
            })
        {
            return Ok(None);
        }
    }
    Ok(Some(event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn responses_compat_extension_events_do_not_finish_stream() {
        for kind in [
            "response.metadata",
            "response.audio.delta",
            "response.audio.done",
            "response.audio.transcript.delta",
            "response.audio.transcript.done",
            "response.compaction.compacting",
            "response.shell_call_command.added",
            "response.shell_call_command.delta",
            "response.shell_call_command.done",
            "response.shell_call_output_content.delta",
            "response.shell_call_output_content.done",
            "response.future_progress",
        ] {
            assert!(prepare_event(json!({"type":kind})).unwrap().is_none());
        }
    }

    #[test]
    fn responses_compat_preserves_unknown_output_and_phase() {
        let raw = json!([{"type":"message","phase":"commentary","content":[{"type":"future_text","text":"x"}]}, {"type":"additional_tools","tools":[]}]);
        let mut response = json!({"output":raw,"metadata":null});
        prepare_response(&mut response).unwrap();
        let saved = response
            .pointer(&format!(
                "/metadata/{}",
                xai_grok_sampling_types::RESPONSE_RAW_OUTPUT_METADATA_KEY
            ))
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(saved).unwrap(), raw);
        assert_eq!(response.get("output").unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn responses_compat_unsupported_calls_are_explicit() {
        for kind in [
            "shell_call",
            "computer_call",
            "apply_patch_call",
            "custom_tool_call",
            "tool_search_call",
            "future_call",
        ] {
            let error =
                prepare_event(json!({"type":"response.output_item.added","item":{"type":kind}}))
                    .unwrap_err();
            assert!(error.to_string().contains("调用未执行"));
        }
    }

    #[test]
    fn responses_compat_deserializer_keeps_original_output() {
        let raw = json!([
            {"type":"message","id":"m1","role":"assistant","status":"completed","phase":"commentary","content":[{"type":"output_text","text":"正文","annotations":[]}]},
            {"type":"additional_tools","id":"t1","role":"assistant","tools":[]}
        ]);
        let event = json!({"type":"response.completed","sequence_number":1,"response":{
            "id":"r1","object":"response","created_at":1,"model":"test","status":"completed","output":raw,
            "metadata":{xai_grok_sampling_types::RESPONSE_RAW_OUTPUT_METADATA_KEY:"[]"}
        }});
        let event = crate::client::deserialize_response_event(&event.to_string()).unwrap();
        let rs::ResponseStreamEvent::ResponseCompleted(event) = event else {
            panic!("终止事件类型错误")
        };
        assert_eq!(event.response.output.len(), 1);
        let items = xai_grok_sampling_types::response_to_conversation_items(event.response);
        let assistant = items
            .iter()
            .find_map(|item| match item {
                xai_grok_sampling_types::ConversationItem::Assistant(a) => Some(a),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            assistant.responses_output.as_ref().unwrap(),
            raw.as_array().unwrap()
        );
    }

    /// 提供失败响应的最小合法结构。
    fn failed_response() -> Value {
        json!({
            "id":"r1", "object":"response", "created_at":1, "model":"test",
            "status":"failed", "output":[],
            "error":{"code":1400,"message":"原始上游错误：请求被拒绝"}
        })
    }

    #[test]
    fn responses_compat_top_level_error_preserves_numeric_and_string_codes() {
        for (code, expected) in [
            (json!(1400), "1400"),
            (json!(-1400), "-1400"),
            (json!(1400.5), "1400.5"),
            (json!(u64::MAX), "18446744073709551615"),
            (json!("1400"), "1400"),
            (json!("invalid_request_error"), "invalid_request_error"),
        ] {
            let raw = json!({
                "type":"error", "sequence_number":1, "code":code,
                "message":"原始上游错误：请求被拒绝"
            });
            assert!(
                xai_grok_sampling_types::error::try_parse_stream_error(&raw.to_string()).is_none()
            );
            let prepared = prepare_event(raw).unwrap().unwrap();
            let event = crate::client::deserialize_response_event(&prepared.to_string()).unwrap();
            let rs::ResponseStreamEvent::ResponseError(error) = event else {
                panic!("错误事件被丢弃或改写")
            };
            assert_eq!(error.code.as_deref(), Some(expected));
            assert_eq!(error.message, "原始上游错误：请求被拒绝");
        }
    }

    #[test]
    fn responses_compat_top_level_error_allows_null_or_missing_code() {
        for raw in [
            json!({"type":"error","sequence_number":1,"message":"boom","code":null}),
            json!({"type":"error","sequence_number":1,"message":"boom"}),
        ] {
            let prepared = prepare_event(raw).unwrap().unwrap();
            let event = crate::client::deserialize_response_event(&prepared.to_string()).unwrap();
            let rs::ResponseStreamEvent::ResponseError(error) = event else {
                panic!("错误事件被丢弃或改写")
            };
            assert_eq!(error.code, None);
            assert_eq!(error.message, "boom");
        }
    }

    #[test]
    fn responses_compat_failed_event_preserves_numeric_and_string_codes() {
        for (code, expected) in [
            (json!(1400), "1400"),
            (json!("1400"), "1400"),
            (json!("server_error"), "server_error"),
        ] {
            let mut response = failed_response();
            response["error"]["code"] = code;
            let raw = json!({"type":"response.failed","sequence_number":1,"response":response});
            assert!(
                xai_grok_sampling_types::error::try_parse_stream_error(&raw.to_string()).is_none()
            );
            let prepared = prepare_event(raw).unwrap().unwrap();
            let event = crate::client::deserialize_response_event(&prepared.to_string()).unwrap();
            let rs::ResponseStreamEvent::ResponseFailed(failed) = event else {
                panic!("失败事件被丢弃或改写")
            };
            let error = failed.response.error.unwrap();
            assert_eq!(error.code, expected);
            assert_eq!(error.message, "原始上游错误：请求被拒绝");
        }
    }

    #[test]
    fn responses_compat_full_response_preserves_error_code_and_message() {
        for (code, expected) in [
            (json!(1400), "1400"),
            (json!("server_error"), "server_error"),
        ] {
            let mut response = failed_response();
            response["error"]["code"] = code;
            prepare_response(&mut response).unwrap();
            let response = serde_json::from_value::<rs::Response>(response).unwrap();
            assert!(matches!(response.status, rs::Status::Failed));
            let error = response.error.unwrap();
            assert_eq!(error.code, expected);
            assert_eq!(error.message, "原始上游错误：请求被拒绝");
        }
        for error in [Some(Value::Null), None] {
            let mut response = failed_response();
            if let Some(error) = error {
                response["error"] = error;
            } else {
                response.as_object_mut().unwrap().remove("error");
            }
            prepare_response(&mut response).unwrap();
            let response = serde_json::from_value::<rs::Response>(response).unwrap();
            assert!(response.error.is_none());
        }
    }

    #[test]
    fn responses_compat_error_fields_keep_sdk_validation() {
        for code in [json!(true), json!([]), json!({})] {
            let raw = json!({"type":"error","sequence_number":1,"code":code,"message":"boom"});
            let prepared = prepare_event(raw).unwrap().unwrap();
            assert!(matches!(
                crate::client::deserialize_response_event(&prepared.to_string()),
                Err(SamplingError::Serialization(_))
            ));
        }
        for field in ["code", "message"] {
            for value in [json!(null), json!(true), json!([]), json!({})] {
                let mut response = failed_response();
                response["error"][field] = value;
                prepare_response(&mut response).unwrap();
                assert!(serde_json::from_value::<rs::Response>(response.clone()).is_err());
                let raw = json!({"type":"response.failed","sequence_number":1,"response":response});
                assert!(matches!(
                    crate::client::deserialize_response_event(&raw.to_string()),
                    Err(SamplingError::Serialization(_))
                ));
            }
            let mut response = failed_response();
            response["error"].as_object_mut().unwrap().remove(field);
            prepare_response(&mut response).unwrap();
            assert!(serde_json::from_value::<rs::Response>(response).is_err());
        }
        for raw in [
            json!({"type":"error","sequence_number":1,"code":1400,"message":1400}),
            json!({"type":"error","sequence_number":"1","code":1400,"message":"boom"}),
        ] {
            let prepared = prepare_event(raw).unwrap().unwrap();
            assert!(matches!(
                crate::client::deserialize_response_event(&prepared.to_string()),
                Err(SamplingError::Serialization(_))
            ));
        }
    }

    #[test]
    fn responses_compat_numeric_error_code_does_not_relax_success_or_tools() {
        for output in [
            json!([{"type":"function_call","id":"f1","call_id":"c1","name":1400,"arguments":"{}","status":"completed"}]),
            json!([{"type":"function_call","id":"f1","call_id":"c1","name":"lookup","arguments":1400,"status":"completed"}]),
        ] {
            let mut response = failed_response();
            response["status"] = json!("completed");
            response["output"] = output;
            prepare_response(&mut response).unwrap();
            assert!(serde_json::from_value::<rs::Response>(response).is_err());
        }
        let mut response = failed_response();
        response["status"] = json!("completed");
        response["model"] = json!(1400);
        prepare_response(&mut response).unwrap();
        assert!(serde_json::from_value::<rs::Response>(response).is_err());
    }

    #[test]
    fn responses_compat_known_malformed_event_stays_strict() {
        let value = prepare_event(json!({"type":"response.output_text.delta","delta":null}))
            .unwrap()
            .unwrap();
        assert!(serde_json::from_value::<rs::ResponseStreamEvent>(value).is_err());
    }
}
