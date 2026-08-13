# OpenCode Theme 源码合并指南

## 适用范围

本文用于将 `opencode-theme` 分支中的 OpenCode TUI 主题移植到更新后的 grok 源码，或者在 grok 上游更新后维护该功能。

初始移植基线：

- grok 基线提交：`e5fd481`
- OpenCode 参考提交：`cc4b45612974f735ddec46009ede07729511fba4`
- 功能分支：`opencode-theme`
- 补丁文件：`patches/opencode-theme.diff`

## 设计边界

该功能采用“独立实现、薄层注册”的结构：

- `theme/opencode.rs` 集中保存 OpenCode 配色与 Markdown 文字规则。
- `assets/opencode.tmTheme` 集中保存代码语法高亮规则。
- `tests/opencode_theme.rs` 独立验证公开主题契约、Markdown 分派和语法资源加载。
- 通用代码只增加主题枚举、缓存解码、样式分派和设置目录等必要注册点。
- 不扩张公共 `Theme` 结构，不改变现有主题，不改变默认主题。

因此，上游合并时应优先保留三个独立文件，再逐一重放薄注册点。不要把 OpenCode 专属颜色拆散到通用渲染模块中。

## 文件清单

### 独立实现文件

以下文件通常可以直接保留或复制到新版本：

- `crates/codegen/xai-grok-pager-render/src/theme/opencode.rs`
- `crates/codegen/xai-grok-pager-render/assets/opencode.tmTheme`
- `crates/codegen/xai-grok-pager-render/tests/opencode_theme.rs`

### 薄注册点

以下文件可能随上游调整而产生冲突：

- `crates/codegen/xai-grok-pager-render/src/theme/mod.rs`
- `crates/codegen/xai-grok-pager-render/src/theme/cache.rs`
- `crates/codegen/xai-grok-pager-render/src/theme/md_style.rs`
- `crates/codegen/xai-grok-pager-render/src/syntax.rs`
- `crates/codegen/xai-grok-pager/src/settings/defs.rs`
- `crates/codegen/xai-grok-pager/src/scrollback/blocks/mermaid_content.rs`
- `crates/codegen/xai-grok-pager/src/views/settings_modal/tests.rs`

### 说明文档

- `docs/opencode-tui-theme-port.md`
- `docs/opencode-theme-源码合并指南.md`

## 推荐合并流程

### 方法一：合并功能分支

适用于本仓库仍保留 `opencode-theme` 分支的情况：

```powershell
git switch opencode-theme
git merge main
```

发生冲突时，不要整文件选择任一侧。先保留上游对通用主题系统的结构调整，再按照下文“冲突处理检查表”重新接入 OpenCode 注册点。

合并与验证完成后，将功能分支合入目标分支：

```powershell
git switch <目标分支>
git merge --no-ff opencode-theme
```

### 方法二：应用补丁

适用于把该功能迁移到另一份同源源码：

```powershell
git apply --check patches/opencode-theme.diff
git apply --3way patches/opencode-theme.diff
```

如果 `--check` 失败，使用下方文件清单手工接入。补丁基于 `e5fd481` 生成，上游距离该基线越远，薄注册点越可能需要人工处理。

## 冲突处理检查表

### 1. 主题枚举和构造

在 `theme/mod.rs` 中确认：

1. `ThemeKind` 包含唯一判别值的 `OpenCode`。
2. `ThemeKind::ALL` 包含 `OpenCode`。
3. `available()` 的非真彩色目录包含 `OpenCode`。
4. `display_name()` 返回规范名 `opencode`。
5. `from_name()` 至少接受 `opencode`，并保留 `open-code`、`oc` 别名。
6. `requires_truecolor()` 对 OpenCode 返回 `false`。
7. `Theme::opencode()` 调用独立模块构造器。
8. `Theme::current()` 能将 `ThemeKind::OpenCode` 分派到该构造器。

如果上游新增了主题，应为 `OpenCode` 重新选择未占用的判别值；不要改动已有已发布判别值，否则缓存中的数值含义可能变化。

### 2. 内存缓存

在 `theme/cache.rs` 的 `theme_kind_from_u8()` 中保留 `OpenCode` 解码分支。若上游改为序列化枚举或其他缓存结构，应按新的统一机制注册，不要保留重复解码路径。

### 3. Markdown 文字规则

在 `theme/md_style.rs` 中，OpenCode 必须分派到 `opencode::markdown_style()`。这一步负责还原下列 OpenCode 文字特征：

- 标题为紫色粗体，一级标题额外添加下划线。
- 粗体为橙色粗体。
- 强调为黄色斜体。
- 行内代码为绿色且不额外加粗。
- 引用为黄色斜体。
- 链接使用橙色与青色下划线。

如果上游把 Markdown 样式改成主题字段驱动，应优先让 OpenCode 独立模块实现新的样式接口，避免把专属规则加入所有主题共享的数据结构。

### 4. 代码语法高亮

在 `syntax.rs` 中确认：

- 存在独立的 `SYNTECT_OPENCODE` 缓存。
- `ThemeKind::OpenCode` 加载 `assets/opencode.tmTheme`。
- 终端原生锁定模式仍按上游逻辑工作，不强制加载 OpenCode RGB 主题。

### 5. 设置与持久化

在 `settings/defs.rs` 的主题目录中保留：

```text
canonical: opencode
display: OpenCode
```

规范名必须与 `ThemeKind::display_name()` 完全一致。主题持久化沿用现有通用设置链路，不应增加 OpenCode 专属配置文件或 setter。

### 6. 下游穷举分支

编译器通常会指出遗漏的 `ThemeKind` 穷举匹配。重点复核：

- Mermaid 的深色主题判断。
- 设置预览测试中的主题构造匹配。
- 上游新增的主题诊断、选择器或快照测试。

OpenCode 是深色主题。

## 同步 OpenCode 上游视觉规则

更新 OpenCode 版本时，对比以下源文件：

- `packages/tui/src/theme/assets/opencode.json`
- `packages/tui/src/theme/index.ts`

若只是颜色值变化，更新 `theme/opencode.rs` 与 `assets/opencode.tmTheme`。若 Markdown scope 或字体效果变化，同时更新 `markdown_style()` 与对应测试。终端字体家族由用户终端控制，本功能只移植终端能够表达的颜色、粗体、斜体、下划线和文字层级。

## 验证

至少执行：

```powershell
cargo fmt --all -- --check
cargo check -p xai-grok-pager-render
cargo check -p xai-grok-pager
cargo test -p xai-grok-pager-render --test opencode_theme
```

Release 构建入口：

```powershell
cargo build -p xai-grok-pager-bin --release
```

Windows 是上游声明的 best-effort 构建环境。当前仓库的 `xai-proto-build` 使用 `/dev/stdout` 和 `/dev/null` 生成 proto 依赖信息，原生 Windows `protoc` 无法直接处理这些路径；若遇到该问题，应在隔离环境中使用兼容转接程序，或者在 macOS/Linux 上执行正式构建。不要为了主题移植修改 proto 构建系统。

验证界面时，通过设置页选择 `OpenCode`，或执行：

```text
/theme opencode
```

重点检查普通正文、六级标题、粗体、斜体、引用、链接、行内代码、代码块、Diff、用户消息背景和工具状态颜色。

## 更新补丁文件

功能提交变化后，从共同基线重新生成补丁，确保补丁不包含自身：

```powershell
git diff --binary --full-index main...HEAD --output=patches/opencode-theme.diff -- . ":(exclude)patches/opencode-theme.diff"
```

随后验证：

```powershell
git apply --check --reverse patches/opencode-theme.diff
```

`--reverse` 检查用于确认补丁与当前已应用状态一致。若目标是干净的基线工作树，则去掉 `--reverse`。
