//! `/retry` 切换或查询当前会话的无限重试模式。

use crate::app::actions::{Action, UnlimitedRetryAction};
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};

/// 当前会话的无限重试命令。
pub struct RetryCommand;

impl SlashCommand for RetryCommand {
    slash_meta! {
        name: "retry",
        description: "切换或查询无限重试（上游 503/429 时每 1 秒重试）",
        usage: "/retry [on|off|status]",
        takes_args: true,
        session_scoped: true,
        arg_placeholder: "[on|off|status]",
    }

    /// 显式开关和只读查询均可补全。
    fn suggest_args(&self, _ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        Some(
            [
                ("on", "开启无限重试，长任务不因上游不稳定中断"),
                ("off", "关闭无限重试，恢复默认重试预算"),
                ("status", "查询无限重试状态"),
            ]
            .into_iter()
            .map(|(value, description)| ArgItem {
                display: value.into(),
                match_text: value.into(),
                insert_text: value.into(),
                description: description.into(),
            })
            .collect(),
        )
    }

    /// 不预判本地状态，翻转由服务端处理。
    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let action = match args.trim() {
            "" => UnlimitedRetryAction::Toggle,
            "on" => UnlimitedRetryAction::On,
            "off" => UnlimitedRetryAction::Off,
            "status" => UnlimitedRetryAction::Status,
            _ => return CommandResult::Error("用法：/retry [on|off|status]".into()),
        };
        if ctx.session_id.is_none() {
            return CommandResult::Error("请先进入会话，再查询或修改无限重试。".into());
        }
        CommandResult::Action(Action::UnlimitedRetry { action })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::slash::commands::tests::make_ctx;

    /// 无参翻转不同于 status 查询；多余参数不能触发修改。
    #[test]
    fn retry_mode_arguments() {
        let models = ModelState::default();
        let session_id = agent_client_protocol::SessionId::new("test");
        let mut ctx = make_ctx(&models);
        ctx.session_id = Some(&session_id);
        for (args, expected) in [
            ("", UnlimitedRetryAction::Toggle),
            ("   ", UnlimitedRetryAction::Toggle),
            (" on ", UnlimitedRetryAction::On),
            ("off", UnlimitedRetryAction::Off),
            ("status", UnlimitedRetryAction::Status),
        ] {
            assert!(matches!(RetryCommand.run(&mut ctx, args),
                CommandResult::Action(Action::UnlimitedRetry { action }) if action == expected));
        }
        for args in ["toggle", "true", "ON", "on off", "status extra"] {
            assert!(matches!(RetryCommand.run(&mut ctx, args),
                CommandResult::Error(message) if message == "用法：/retry [on|off|status]"));
        }
        ctx.session_id = None;
        assert!(matches!(RetryCommand.run(&mut ctx, ""),
            CommandResult::Error(message) if message.contains("先进入会话")));
    }
}
