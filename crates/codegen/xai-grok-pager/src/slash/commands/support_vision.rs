//! `/support_vision` 查询或修改当前会话当前模型的视觉输入设置。

use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};

/// 当前会话的视觉开关命令。
pub struct SupportVisionCommand;

impl SlashCommand for SupportVisionCommand {
    slash_meta! {
        name: "support_vision",
        description: "查询或设置当前模型的视觉输入能力",
        usage: "/support_vision [on|off]",
        takes_args: true,
        session_scoped: true,
        arg_placeholder: "[on|off]",
    }

    /// 只提供明确的开关值，不支持隐式翻转。
    fn suggest_args(&self, _ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        Some(
            [
                ("on", "启用原生视觉"),
                ("off", "关闭原生视觉，仍可添加附件"),
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

    /// 无参只查询；修改值由服务端确认后显示。
    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let enabled = match args.trim() {
            "" => None,
            "on" => Some(true),
            "off" => Some(false),
            _ => return CommandResult::Error("用法：/support_vision [on|off]".into()),
        };
        if ctx.session_id.is_none() {
            return CommandResult::Error("请先进入会话，再查询或修改视觉输入设置。".into());
        }
        CommandResult::Action(Action::SupportVision { enabled })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::slash::commands::tests::make_ctx;

    /// 查询、修改和非法参数走独立分支。
    #[test]
    fn support_vision_arguments() {
        let models = ModelState::default();
        let session_id = agent_client_protocol::SessionId::new("test");
        let mut ctx = make_ctx(&models);
        ctx.session_id = Some(&session_id);
        for (args, expected) in [("", None), (" on ", Some(true)), ("off", Some(false))] {
            assert!(matches!(SupportVisionCommand.run(&mut ctx, args),
                CommandResult::Action(Action::SupportVision { enabled }) if enabled == expected));
        }
        for args in ["true", "ON", "on off", "off extra"] {
            assert!(matches!(
                SupportVisionCommand.run(&mut ctx, args),
                CommandResult::Error(_)
            ));
        }
        ctx.session_id = None;
        assert!(matches!(SupportVisionCommand.run(&mut ctx, ""),
            CommandResult::Error(message) if message.contains("先进入会话")));
    }
}
