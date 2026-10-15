//! `/subagent-model` 查询或设置新建子代理的会话级模型选择。

use crate::acp::model_state::ModelState;
use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};
use crate::slash::commands::model::{
    ChainedPhase, chained_phase, parse_known_model_choice, preselected_model_arg,
    suggest_model_args,
};
use xai_grok_shell::session::subagents_model::SubagentsModelAction;

/// 子代理选择仅由服务端保存，不修改主模型或用户默认配置。
pub struct SubagentsModelCommand;

/// 控制词保留为命令语义，不能作为模型名称或发现 ID。
fn is_control(token: &str) -> bool {
    ["status", "inherit", "reset"]
        .iter()
        .any(|word| word.eq_ignore_ascii_case(token))
}

/// 控制词起始的输入不进入模型参数链。
fn starts_with_control(args: &str) -> bool {
    args.split_whitespace().next().is_some_and(is_control)
}

/// 补全只使用目录默认值，不把主会话的当前选择标为子代理选择。
fn catalog_only(models: &ModelState) -> ModelState {
    let mut catalog = ModelState::default();
    catalog.update_catalog(models.available.clone());
    catalog
        .available
        .retain(|id, info| !starts_with_control(id.0.as_ref()) && !starts_with_control(&info.name));
    catalog
}

/// 子菜单标题沿用模型命令的阶段判断。
pub(crate) fn picker_title(models: &ModelState, args_query: &str) -> &'static str {
    let catalog = catalog_only(models);
    if starts_with_control(args_query) {
        return "选择子代理模型或操作";
    }
    match chained_phase(&catalog, args_query) {
        Some(ChainedPhase::Window { .. }) => "选择子代理上下文窗口",
        Some(ChainedPhase::Effort { .. }) => "选择子代理推理强度",
        Some(ChainedPhase::Done) | None => "选择子代理模型或操作",
    }
}

impl SlashCommand for SubagentsModelCommand {
    slash_meta! {
        name: "subagent-model",
        aliases: ["subagents-model"],
        description: "查询或设置新建子代理的会话级模型",
        usage: "/subagent-model [status|inherit|reset|<model> [window] [effort]]",
        takes_args: true,
        session_scoped: true,
        arg_placeholder: "[status|inherit|reset|<model> [window] [effort]]",
    }

    /// 模型阶段同时提供控制词，窗口和 effort 阶段复用模型命令。
    fn suggest_args(&self, ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        let catalog = catalog_only(ctx.models);
        if !starts_with_control(args_query) && chained_phase(&catalog, args_query).is_some() {
            return suggest_model_args(&catalog, args_query);
        }
        let mut items = suggest_model_args(&catalog, "").unwrap_or_default();
        items.extend(
            [
                ("status", "查询有效模型、推理强度、窗口及来源"),
                ("inherit", "强制默认跟随主模型，仅影响新建子代理"),
                ("reset", "清除会话选择，恢复配置规则"),
            ]
            .into_iter()
            .map(|(word, description)| ArgItem {
                display: word.into(),
                match_text: word.into(),
                insert_text: word.into(),
                description: description.into(),
            }),
        );
        Some(items)
    }

    /// 不使用主模型的当前 effort/window 预选子代理参数。
    fn preselected_arg(&self, ctx: &AppCtx, args_query: &str) -> Option<String> {
        if starts_with_control(args_query) {
            return None;
        }
        preselected_model_arg(&catalog_only(ctx.models), args_query)
    }

    /// 已知选择和未知精确 ID 均交给同一个 Set 接口。
    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        if ctx.session_id.is_none() {
            return CommandResult::Error("请先进入会话，再查询或设置子代理模型。".into());
        }
        let args = args.trim();
        let action = match args.to_ascii_lowercase().as_str() {
            "" | "status" => SubagentsModelAction::Status,
            "inherit" => SubagentsModelAction::Inherit,
            "reset" => SubagentsModelAction::Reset,
            _ => {
                if starts_with_control(args) {
                    return CommandResult::Error(format!("用法：{}", self.usage()));
                }
                match parse_known_model_choice(ctx.models, args) {
                    Ok(Some(choice)) => SubagentsModelAction::Set {
                        model_id: choice.model_id.0.to_string(),
                        reasoning_effort: choice.effort,
                        context_window: choice.context_window_selection,
                    },
                    Err(error) => return CommandResult::Error(error),
                    Ok(None) if !args.chars().any(char::is_whitespace) => {
                        SubagentsModelAction::Set {
                            model_id: args.to_owned(),
                            reasoning_effort: None,
                            context_window: None,
                        }
                    }
                    Ok(None) => return CommandResult::Error(format!("未知模型或无效参数：{args}")),
                }
            }
        };
        CommandResult::Action(Action::SubagentsModel { action })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::commands::tests::make_ctx;
    use agent_client_protocol as acp;
    use std::num::NonZeroU64;
    use xai_grok_shell::sampling::types::ReasoningEffort;
    use xai_grok_test_support::acp_fixtures;

    /// 主模型有非默认参数，用于发现意外继承。
    fn models() -> ModelState {
        let mut models = ModelState::default();
        for (id, name) in [("reason", "Reason"), ("reason-pro", "Reason Pro")] {
            let info = acp_fixtures::model_info_with_meta(
                id,
                name,
                serde_json::json!({
                    "supportsReasoningEffort": true, "reasoningEffort": "high",
                    "contextWindows": [256000, 500000], "totalContextTokens": 256000
                }),
            );
            models.available.insert(info.model_id.clone(), info);
        }
        let plain = acp::ModelInfo::new(acp::ModelId::new("plain"), "Plain Model");
        models.available.insert(plain.model_id.clone(), plain);
        let fixed = acp_fixtures::model_info_with_meta(
            "fixed",
            "Fixed Window",
            serde_json::json!({
                "contextWindows": [256000], "totalContextTokens": 256000
            }),
        );
        models.available.insert(fixed.model_id.clone(), fixed);
        models.set_current(acp::ModelId::new("reason-pro"), Some(ReasoningEffort::Low));
        models.context_window_selection = Some(500000);
        models
    }

    /// 补全上下文不保存子代理选择。
    fn app_ctx(models: &ModelState) -> AppCtx<'_> {
        AppCtx {
            models,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            saved_workflows: &[],
            workflow_runs: &[],
            screen_mode: crate::app::ScreenMode::Fullscreen,
            current_title: None,
        }
    }

    /// 已知名称优先精确匹配；省略的参数不会继承主模型的 live 值。
    #[test]
    fn subagents_model_parses_known_choices_without_inheritance() {
        let models = models();
        let sid = acp::SessionId::new("s");
        let mut ctx = make_ctx(&models);
        ctx.session_id = Some(&sid);
        for (input, id, effort, window) in [
            ("Reason Pro", "reason-pro", None, None),
            ("reason-pro", "reason-pro", None, None),
            (" reason pro ", "reason-pro", None, None),
            ("Reason Pro 256k", "reason-pro", None, Some(256000)),
            (
                "Reason Pro high",
                "reason-pro",
                Some(ReasoningEffort::High),
                None,
            ),
            (
                "Reason Pro 500k high",
                "reason-pro",
                Some(ReasoningEffort::High),
                Some(500000),
            ),
            (
                "reason-pro 256000 xhigh",
                "reason-pro",
                Some(ReasoningEffort::Xhigh),
                Some(256000),
            ),
            ("Plain Model", "plain", None, None),
            ("Fixed Window 256k", "fixed", None, Some(256000)),
            ("Vendor/Exact-ID", "Vendor/Exact-ID", None, None),
        ] {
            let result = SubagentsModelCommand.run(&mut ctx, input);
            let CommandResult::Action(Action::SubagentsModel { action }) = result else {
                panic!("{input}: {result:?}");
            };
            assert_eq!(
                action,
                SubagentsModelAction::Set {
                    model_id: id.into(),
                    reasoning_effort: effort,
                    context_window: window.and_then(NonZeroU64::new),
                },
                "{input}"
            );
        }
        assert_eq!(models.reasoning_effort, Some(ReasoningEffort::Low));
        assert_eq!(models.context_window_selection, Some(500000));
    }

    /// 控制词及其后续输入不能变成模型发现；错误 effort 同样不能回退。
    #[test]
    fn subagents_model_controls_and_invalid_arguments() {
        let mut models = models();
        for control in ["status", "inherit", "reset"] {
            let info = acp::ModelInfo::new(acp::ModelId::new(control), control);
            models.available.insert(info.model_id.clone(), info);
        }
        let items = SubagentsModelCommand
            .suggest_args(&app_ctx(&models), "")
            .unwrap();
        for control in ["status", "inherit", "reset"] {
            assert_eq!(
                items
                    .iter()
                    .filter(|item| item.insert_text == control)
                    .count(),
                1
            );
        }
        let sid = acp::SessionId::new("s");
        let mut ctx = make_ctx(&models);
        ctx.session_id = Some(&sid);
        for (input, expected) in [
            ("", SubagentsModelAction::Status),
            ("   ", SubagentsModelAction::Status),
            ("status", SubagentsModelAction::Status),
            (" STATUS ", SubagentsModelAction::Status),
            ("inherit", SubagentsModelAction::Inherit),
            ("reset", SubagentsModelAction::Reset),
        ] {
            assert!(matches!(SubagentsModelCommand.run(&mut ctx, input),
                CommandResult::Action(Action::SubagentsModel { action }) if action == expected));
        }
        for input in [
            "status extra",
            "inherit high",
            "reset 256k",
            "unknown high",
            "Unknown Name",
            "Reason Pro invalid",
            "reason-pro 500k invalid",
            "Reason Pro 1m",
            "Plain Model high",
        ] {
            assert!(
                matches!(
                    SubagentsModelCommand.run(&mut ctx, input),
                    CommandResult::Error(_)
                ),
                "{input}"
            );
        }
        ctx.session_id = None;
        for input in [
            "",
            "status",
            "inherit",
            "reset",
            "reason-pro",
            "Vendor/Exact-ID",
        ] {
            assert!(
                matches!(
                    SubagentsModelCommand.run(&mut ctx, input),
                    CommandResult::Error(_)
                ),
                "{input}"
            );
        }
    }

    /// 模型、窗口、effort 链均复用现有补全，预选只取目录默认值。
    #[test]
    fn subagents_model_chained_completion_and_titles() {
        let models = models();
        let ctx = app_ctx(&models);
        let cmd = SubagentsModelCommand;
        let items = cmd.suggest_args(&ctx, "").unwrap();
        for token in ["status", "inherit", "reset", "Reason Pro ", "Plain Model"] {
            assert!(items.iter().any(|item| item.insert_text == token));
        }
        assert!(items.iter().all(|item| !item.display.contains("current")));
        let windows = cmd.suggest_args(&ctx, "Reason Pro ").unwrap();
        assert_eq!(windows.len(), 2);
        assert!(
            windows
                .iter()
                .any(|item| item.insert_text == "Reason Pro 500k ")
        );
        assert_eq!(
            cmd.preselected_arg(&ctx, "Reason Pro ").as_deref(),
            Some("Reason Pro 256k ")
        );
        let efforts = cmd.suggest_args(&ctx, "Reason Pro 500k ").unwrap();
        assert!(
            efforts
                .iter()
                .any(|item| item.insert_text == "Reason Pro 500k high")
        );
        assert_eq!(
            cmd.preselected_arg(&ctx, "Reason Pro 500k ").as_deref(),
            Some("Reason Pro 500k high")
        );
        assert!(cmd.preselected_arg(&ctx, "Reason Pro h").is_none());
        assert!(
            cmd.suggest_args(&ctx, "Reason Pro h")
                .unwrap()
                .iter()
                .any(|item| item.insert_text == "Reason Pro high")
        );
        assert_eq!(picker_title(&models, ""), "选择子代理模型或操作");
        assert_eq!(picker_title(&models, "Reason Pro "), "选择子代理上下文窗口");
        assert_eq!(
            picker_title(&models, "Reason Pro 500k "),
            "选择子代理推理强度"
        );
        assert_eq!(picker_title(&models, "status "), "选择子代理模型或操作");
        let empty = ModelState::default();
        assert_eq!(cmd.suggest_args(&app_ctx(&empty), "").unwrap().len(), 3);
        assert!(!cmd.args_required());
        assert!(cmd.session_scoped());
        assert!(!cmd.offered_when_session_less());
    }
}
