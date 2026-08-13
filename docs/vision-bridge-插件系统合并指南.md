# vision-bridge 插件系统合并指南

## 适用范围

本文用于将 `gf` 分支中的插件系统改动（UserPromptTransform 消息变换钩子 + vision-bridge 图片桥接插件）迁移到更新后的 grok 源码，或者在 grok 上游更新后维护该功能。

基线信息：

- grok 基线提交：`873c831`（opencode-theme 分支 HEAD，插件系统改动起点）
- 功能分支：`gf`（opencode 主题移植 + 插件系统改动）
- 补丁文件：`patches/user-prompt-transform-hooks.diff`
- 插件目录（用户环境，非仓库）：`~/.grok/plugins/vision-bridge/`

## 功能概述

vision-bridge 解决"主模型不支持视觉时，用户粘贴图片如何被识别"的问题：

1. grok 在用户消息发送前触发 `UserPromptTransform` 钩子（消息变换门，opencode `messages.transform` 的等价物）。
2. 钩子载荷携带结构化图片信息（`images`）与当前模型是否多模态（`supports_vision`）。
3. 插件 `bridge.js` 据此在消息末尾注入提示词，引导不支持视觉的模型调用 vision MCP 工具（如 `opencode-vision-lite` 的 `vision_analyze`）。
4. 模型支持视觉（`supports_vision=true`）时插件直接放行，不注入。

## 设计边界

- hooks 框架新增**第四种门** `Transform`（`UserPromptTransform` 事件），与既有 `Observe` / `Tool` / `Stop` 并列，不改变既有事件语义。
- payload 扩展向后兼容：`images` 为 `Option`（缺省省略），`supports_vision` 为 `bool`（旧调用方不传时默认放行语义由消费方决定）；序列化均带 `skip_serializing_if`。
- 插件钩子**启动注册**与 mid-session reload 统一走 `append_plugin_hooks_to()`，保证启动与会话中重载行为一致。
- 插件本体（`bridge.js`、`hooks.json`）在用户主目录 `~/.grok/plugins/vision-bridge`，**不在仓库内**；仓库只提供 grok 侧的钩子框架与集成。
- 插件钩子命令必须使用 `${GROK_PLUGIN_ROOT}` 占位符解析为绝对路径。grok 执行钩子命令时 cwd 是工作区，相对路径（如 `node bin/bridge.js`）会找不到脚本而静默失败。

## 文件清单

### 新文件

- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/user_prompt_transform.rs` — `UserPromptTransform` 派发入口（活性检查 → client 通知 → 注册表钩子链）

### 修改文件（hooks 框架）

- `crates/codegen/xai-grok-hooks/src/event.rs` — `UserPromptTransform` 事件定义、`HookPayload` 扩展 `images`/`supports_vision`、新增 `UserPromptImage` 结构
- `crates/codegen/xai-grok-hooks/src/dispatcher.rs` — `dispatch_user_prompt_transform`（链式聚合、失败 fail-open、字段透传）
- `crates/codegen/xai-grok-hooks/src/discovery.rs` — 事件注册表
- `crates/codegen/xai-grok-hooks/src/runner/mod.rs` / `runner/command.rs` / `runner/http.rs` — `Transform` gate 的解析与执行（`TransformHookJson`、`parse_transform_result`）

### 修改文件（shell 集成）

- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs` — 图片持久化后、消息写会话前调用 `dispatch_user_prompt_transform`，并计算 `supports_vision`
- `crates/codegen/xai-grok-shell/src/session/image_describe.rs` — `persist_and_prepend_image_files` 返回 `(消息, 图片路径列表)`，路径供钩子载荷使用
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/hooks_plugins.rs` — 新增公共辅助函数 `append_plugin_hooks_to()`（合并 active 插件的文件/内联钩子），`reload_hooks_impl` 复用
- `crates/codegen/xai-grok-shell/src/session/acp_session_impl/spawn.rs` — 会话启动构建 hook_registry 后合并插件钩子

### 插件侧（用户环境，非仓库）

- `~/.grok/plugins/vision-bridge/hooks/bin/bridge.js` — 三通道检测（结构化 `payload.images` / `<image_files>` 块 / 启发式路径正则）+ `supports_vision` 放行
- `~/.grok/plugins/vision-bridge/hooks/hooks.json` — 钩子命令 `node ${GROK_PLUGIN_ROOT}/hooks/bin/bridge.js`

### 说明文档

- `docs/用户提示词变换钩子实现计划.md`
- `docs/vision-bridge-插件系统合并指南.md`（本文）

## 推荐合并流程

### 方法一：合并功能分支

适用于本仓库仍保留 `gf` 分支的情况：

```powershell
git switch gf
git merge <上游分支>
```

发生冲突时，不要整文件选择任一侧。先保留上游对 hooks 系统的结构调整，再按下方"冲突处理检查表"重新接入注册点。

### 方法二：应用补丁

适用于把该功能迁移到另一份同源源码：

```powershell
git apply --check patches/user-prompt-transform-hooks.diff
git apply --3way patches/user-prompt-transform-hooks.diff
```

如果 `--check` 失败，使用下方文件清单手工接入。

## 冲突处理检查表

### 1. 事件与 gate

在 `xai-grok-hooks/src/event.rs` 中确认：

1. `hook_events!` 表包含 `UserPromptTransform`（别名 `user_prompt_transform` / `userPromptTransform`）。
2. `GateKind` 包含 `Transform`。
3. `HookPayload` 的 `UserPromptTransform` 变体包含 `prompt`、`images: Option<Vec<UserPromptImage>>`、`supports_vision: bool`。
4. `match_value()` 对 `UserPromptTransform` 返回 `None`（Ignored 策略，fire-all）。
5. `UserPromptImage` 结构存在（`path` / `mimeType` / `uri`，均 `Option` 且 `skip_serializing_if`）。

### 2. transform 链式派发

在 `dispatcher.rs` 的 `dispatch_user_prompt_transform` 中确认：

- 链式重建信封时**透传**原始 `images` / `supports_vision`（不要只保留 `prompt`，否则后续钩子丢失字段）。
- 任一钩子失败（非零退出、超时、非法输出、空 `text`）均 fail-open，保持当前文本。

### 3. runner 的 Transform gate

在 `runner/mod.rs` / `command.rs` / `http.rs` 中确认 `GateKind::Transform` 分支存在，输出协议为 stdout JSON `{"text": "..."}`。

### 4. 图片路径返回值

`persist_and_prepend_image_files` 返回 `(String, Vec<String>)`（消息文本 + 持久化图片路径，与输入同序）。若上游改其签名，需同步 `turn.rs` 调用点与 `image_describe.rs` 测试。

### 5. 插件钩子启动注册

在 `spawn.rs` 中确认构建 hook_registry 后调用了 `append_plugin_hooks_to(&mut registry, plugin_registry.as_deref())`。若上游重构 spawn，需保证插件钩子与会话一起加载，而不是只依赖 `/hooks reload` 等 mid-session 事件。

### 6. 插件侧（用户环境）

- `hooks.json` 命令必须为 `node ${GROK_PLUGIN_ROOT}/hooks/bin/bridge.js`，不能写相对路径。
- `bridge.js` 的 `supports_vision === true` 时直接放行（多模态模型不注入）。
- 插件目录不在仓库，迁移时需单独复制到目标机器的 `~/.grok/plugins/vision-bridge`。

## 验证

至少执行：

```powershell
cargo check -p xai-grok-hooks
cargo check -p xai-grok-shell
cargo test -p xai-grok-hooks
```

Windows Release 构建入口（正式入口）：

```powershell
& .\tools\windows-protoc\build-release.ps1 --locked
```

bridge.js 手动验证（模拟钩子载荷，`supports_vision=false` 且消息含图片路径时应注入）：

```powershell
$body = '{"prompt":"分析 R:\file.png","supports_vision":false}'
$body | node "$env:USERPROFILE\.grok\plugins\vision-bridge\hooks\bin\bridge.js"
```

端到端验证（TUI 中）：

1. 以**附件形式**粘贴图片 → 结构化/`<image_files>` 通道注入强提示。
2. 发送**纯文本图片路径**（如 `R:\file.png`）→ 启发式通道注入弱提示。
3. 配置 `supports_vision = true` 的模型（如 local-gpt）→ 插件放行不注入。

## 更新补丁文件

功能提交变化后，从共同基线重新生成补丁：

```powershell
git diff --binary --full-index 873c831...HEAD --output=patches/user-prompt-transform-hooks.diff -- . ":(exclude)patches/**"
```

随后验证：

```powershell
git apply --check --reverse patches/user-prompt-transform-hooks.diff
```
