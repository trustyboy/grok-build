//! 仅对原生 Responses 最终 input 执行图片预算，保持归档前缀稳定。

use serde_json::Value;

const OMITTED_IMAGE: &str = "[An earlier image was removed to keep the request within its size limit and is no longer visible. Do not describe or reason about its contents from memory.]";

/// 最终请求体大小及本次剔除的图片数量，不包含消息内容。
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ImageBudget {
    pub body_bytes: usize,
    pub evicted: usize,
}

/// 按最旧图片优先回收；只替换协议图片项，不遍历工具参数或加密内容。
pub(super) fn apply(
    input: &mut [Value],
    envelope_bytes: usize,
    cap: usize,
) -> Result<ImageBudget, serde_json::Error> {
    let mut budget = ImageBudget {
        body_bytes: envelope_bytes.saturating_add(serde_json::to_vec(input)?.len()),
        evicted: 0,
    };
    let trigger = cap.saturating_sub((3 * 1024 * 1024).min(cap / 10));
    if budget.body_bytes < trigger {
        return Ok(budget);
    }
    let target = cap / 2;
    let replacement = serde_json::json!({"type":"input_text", "text":OMITTED_IMAGE});
    let replacement_bytes = serde_json::to_vec(&replacement)?.len();
    for item in input {
        let field = match item.get("type").and_then(Value::as_str) {
            Some("function_call_output") => "output",
            Some("message") | None => "content",
            _ => continue,
        };
        let Some(parts) = item.get_mut(field).and_then(Value::as_array_mut) else {
            continue;
        };
        for part in parts {
            if budget.body_bytes <= target {
                return Ok(budget);
            }
            if part.get("type").and_then(Value::as_str) != Some("input_image") {
                continue;
            }
            let old_bytes = serde_json::to_vec(part)?.len();
            if old_bytes <= replacement_bytes {
                continue;
            }
            *part = replacement.clone();
            budget.body_bytes -= old_bytes - replacement_bytes;
            budget.evicted += 1;
        }
    }
    Ok(budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_image_budget_only_changes_protocol_images() {
        let mut input = vec![
            json!({"type":"message","role":"user","content":[
                {"type":"input_image","image_url":format!("data:image/png;base64,{}", "a".repeat(4000))},
                {"type":"input_text","text":"保留文字"}
            ]}),
            json!({"type":"function_call_output","call_id":"c1","output":[
                {"type":"input_image","image_url":format!("data:image/png;base64,{}", "b".repeat(4000))}
            ]}),
            json!({"type":"compaction","encrypted_content":"opaque"}),
        ];
        let budget = apply(&mut input, 100, 5000).unwrap();
        assert_eq!(budget.evicted, 2);
        assert_eq!(input[0]["content"][1]["text"], "保留文字");
        assert_eq!(input[1]["output"][0]["type"], "input_text");
        assert_eq!(input[2]["encrypted_content"], "opaque");
        assert_eq!(
            budget.body_bytes,
            100 + serde_json::to_vec(&input).unwrap().len()
        );
    }

    #[test]
    fn native_image_budget_preserves_small_input() {
        let mut input = vec![json!({"type":"message","role":"user","content":[
            {"type":"input_image","image_url":"https://example.test/image.png"}
        ]})];
        let original = input.clone();
        assert_eq!(apply(&mut input, 100, 5000).unwrap().evicted, 0);
        assert_eq!(input, original);
    }
}
