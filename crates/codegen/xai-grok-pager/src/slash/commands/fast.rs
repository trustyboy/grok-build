//! `/fast` 切换或查询当前会话当前模型的优先服务状态。

use crate::app::actions::{Action, FastModeAction};
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};

/// 当前会话模型的 Fast 模式命令。
pub struct FastCommand;

impl SlashCommand for FastCommand {
    slash_meta! {
        name: "fast",
        description: "切换或查询当前模型的快速模式（费用更高）",
        usage: "/fast [on|off|status]",
        takes_args: true,
        session_scoped: true,
        arg_placeholder: "[on|off|status]",
    }

    /// 显式开关和只读查询均可补全。
    fn suggest_args(&self, _ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        Some(
            [
                ("on", "开启快速模式（费用更高）"),
                ("off", "关闭快速模式，沿用上游默认"),
                ("status", "查询快速模式状态"),
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

    /// 不预判本地状态，翻转和协议支持由服务端处理。
    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let action = match args.trim() {
            "" => FastModeAction::Toggle,
            "on" => FastModeAction::On,
            "off" => FastModeAction::Off,
            "status" => FastModeAction::Status,
            _ => return CommandResult::Error("用法：/fast [on|off|status]".into()),
        };
        if ctx.session_id.is_none() {
            return CommandResult::Error("请先进入会话，再查询或修改快速模式。".into());
        }
        CommandResult::Action(Action::FastMode { action })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::slash::commands::tests::make_ctx;

    /// 无参翻转不同于 status 查询；多余参数不能触发修改。
    #[test]
    fn fast_mode_arguments() {
        let models = ModelState::default();
        let session_id = agent_client_protocol::SessionId::new("test");
        let mut ctx = make_ctx(&models);
        ctx.session_id = Some(&session_id);
        for (args, expected) in [
            ("", FastModeAction::Toggle),
            ("   ", FastModeAction::Toggle),
            (" on ", FastModeAction::On),
            ("off", FastModeAction::Off),
            ("status", FastModeAction::Status),
        ] {
            assert!(matches!(FastCommand.run(&mut ctx, args),
                CommandResult::Action(Action::FastMode { action }) if action == expected));
        }
        for args in ["toggle", "true", "ON", "on off", "status extra"] {
            assert!(matches!(FastCommand.run(&mut ctx, args),
                CommandResult::Error(message) if message == "用法：/fast [on|off|status]"));
        }
        ctx.session_id = None;
        assert!(matches!(FastCommand.run(&mut ctx, ""),
            CommandResult::Error(message) if message.contains("先进入会话")));
    }
}
