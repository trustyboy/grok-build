//! `UserPromptTransform` 消息变换钩子的派发入口。
//!
//! 在用户消息组装完成（图片已持久化、`<image_files>` 块已注入）之后、
//! 消息写入会话之前调用。把完整用户消息文本交给所有匹配的
//! `UserPromptTransform` 钩子，钩子可通过 stdout 输出 `{"text": "..."}`
//! 替换该消息（多个钩子链式应用）。任何失败都保持原消息（fail-open），
//! 与 grok 其余 hooks 的安全语义一致。
//!
//! 对应 opencode 插件的 `experimental.chat.messages.transform` 钩子，
//! 让"消息发送前强制注入"类插件（如 vision-attachment-bridge）可以
//! 以 grok 插件（hooks/hooks.json + 任意语言脚本）的方式实现。

use xai_grok_hooks::event::{HookEventName, HookPayload, UserPromptImage};

use super::SessionActor;

impl SessionActor {
    /// 运行 `UserPromptTransform` 钩子链并返回（可能被替换的）用户消息文本。
    ///
    /// - 无匹配钩子 / 全部钩子未变换：原样返回 `prompt`。
    /// - 钩子失败（超时、崩溃、输出非法）：fail-open，保持当前文本。
    ///
    /// `user_images` 是已规范化存活的图片附件（含 MIME/URI），`image_paths`
    /// 是它们在 `<image_files>` 块中列出的磁盘路径（与 `user_images` 同序；
    /// 无持久化路径的 harness 传入空列表）。两者组装为结构化 `images` 载荷，
    /// 使 vision-bridge 类钩子无需再解析 `<image_files>` 文本。`supports_vision`
    /// 表示当前后端模型是否多模态，钩子据此跳过对多模态模型的提示注入。
    pub(super) async fn dispatch_user_prompt_transform(
        &self,
        prompt: String,
        prompt_id: Option<&str>,
        user_images: &[agent_client_protocol::ImageContent],
        image_paths: &[String],
        supports_vision: bool,
    ) -> String {
        if !self.hook_event_active(HookEventName::UserPromptTransform) {
            return prompt;
        }
        let images = (!user_images.is_empty()).then(|| {
            user_images
                .iter()
                .enumerate()
                .map(|(i, img)| UserPromptImage {
                    path: image_paths.get(i).cloned(),
                    mime_type: img.mime_type.clone(),
                    uri: img.uri.clone(),
                })
                .collect()
        });
        // 先通知 client（observe-only），再运行磁盘注册表上的钩子链。
        let envelope = self.fire_hook(
            HookEventName::UserPromptTransform,
            prompt_id.map(|s| s.to_string()),
            HookPayload::UserPromptTransform {
                prompt: Some(prompt.clone()),
                images,
                supports_vision,
            },
        );
        let Some(registry) = self.hook_registry.borrow().clone() else {
            return prompt;
        };
        let ctx = self.hook_run_ctx();
        match xai_grok_hooks::dispatcher::dispatch_user_prompt_transform(
            &registry, &envelope, &ctx,
        )
        .await
        {
            Some(transformed) => transformed,
            None => prompt,
        }
    }
}
