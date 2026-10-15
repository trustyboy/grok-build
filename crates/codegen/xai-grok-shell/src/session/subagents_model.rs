//! 当前会话的新建子代理模型选择，不写入持久化配置。
use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;
use xai_grok_sampling_types::ReasoningEffort;

/// 父会话保存的子代理模型覆盖。
#[derive(Debug, Clone, Default, PartialEq)]
pub enum SubagentsModelSelection {
    /// 使用配置和代理定义的默认规则。
    #[default]
    Default,
    /// 跳过模型默认值，直接继承父会话实时模型。
    Inherit,
    /// 临时参数仅适用于同一目录目标模型。
    Model {
        model_id: String,
        reasoning_effort: Option<ReasoningEffort>,
        context_window: Option<NonZeroU64>,
    },
}

/// 子代理模型扩展请求。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(
    tag = "action",
    rename_all = "lowercase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SubagentsModelAction {
    #[default]
    Status,
    Inherit,
    Reset,
    Set {
        model_id: String,
        #[serde(default)]
        reasoning_effort: Option<ReasoningEffort>,
        #[serde(default)]
        context_window: Option<NonZeroU64>,
    },
}

/// 空结构变体严格拒绝控制操作携带的额外字段。
impl<'de> Deserialize<'de> for SubagentsModelAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(
            tag = "action",
            rename_all = "lowercase",
            rename_all_fields = "camelCase",
            deny_unknown_fields
        )]
        enum WireAction {
            Status {},
            Inherit {},
            Reset {},
            Set {
                model_id: String,
                reasoning_effort: Option<ReasoningEffort>,
                context_window: Option<NonZeroU64>,
            },
        }
        Ok(match WireAction::deserialize(deserializer)? {
            WireAction::Status {} => Self::Status,
            WireAction::Inherit {} => Self::Inherit,
            WireAction::Reset {} => Self::Reset,
            WireAction::Set {
                model_id,
                reasoning_effort,
                context_window,
            } => Self::Set {
                model_id,
                reasoning_effort,
                context_window,
            },
        })
    }
}

/// 默认 general-purpose 子代理的实际模型、参数与来源。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentsModelState {
    /// 目录模型键，可能不同于实际请求中的路由名称。
    pub model_id: String,
    /// 模型来源：session、role、persona、type_config、parent_model_config、config、definition 或 parent。
    pub model_source: String,
    /// 最终实际推理强度。
    pub reasoning_effort: Option<ReasoningEffort>,
    /// 推理强度来源；基础配置使用 parent 或 model_default。
    pub effort_source: String,
    /// 实际上下文窗口。
    pub context_window: u64,
    /// 状态所属父会话的当前模型目录键，兼容旧服务端时可缺省。
    #[serde(default)]
    pub parent_model_id: Option<String>,
    /// 当前主模型配置的子代理默认值。
    #[serde(default)]
    pub parent_configured_model: Option<String>,
    /// 全局模型配置原值，不包含临时选择。
    pub configured_model: Option<String>,
    /// 全局推理强度原值，保留无效字符串供诊断。
    pub configured_effort: Option<String>,
    /// 合并无效配置和不支持参数的原因。
    pub warning: Option<String>,
    /// 由请求处理层按当前认证填充。
    pub available_models: Vec<acp::ModelInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagents_model_action_accepts_optional_parameters() {
        let action: SubagentsModelAction = serde_json::from_value(serde_json::json!({
            "action": "set", "modelId": "alias"
        }))
        .unwrap();
        assert_eq!(
            action,
            SubagentsModelAction::Set {
                model_id: "alias".into(),
                reasoning_effort: None,
                context_window: None,
            }
        );
        let action: SubagentsModelAction = serde_json::from_value(serde_json::json!({
            "action": "set", "modelId": "alias", "reasoningEffort": "high", "contextWindow": 128000
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(action).unwrap()["contextWindow"],
            128000
        );
    }

    #[test]
    fn subagents_model_action_rejects_unknown_or_invalid_parameters() {
        for value in [
            serde_json::json!({"action": "set", "modelId": "alias", "contextWindow": 0}),
            serde_json::json!({"action": "set", "modelId": "alias", "reasoningEffort": "invalid"}),
            serde_json::json!({"action": "set", "modelId": "alias", "unexpected": true}),
            serde_json::json!({"action": "reset", "contextWindow": 128000}),
        ] {
            assert!(serde_json::from_value::<SubagentsModelAction>(value).is_err());
        }
    }
}
