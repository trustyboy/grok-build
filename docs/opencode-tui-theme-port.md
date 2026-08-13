# OpenCode TUI 主题移植计划

## 目标

将 OpenCode 默认 TUI 的配色、Markdown 文字层级和代码语法高亮移植为 grok 的独立 `OpenCode` 主题，同时尽量降低后续合并上游版本时的冲突成本。

本次颜色和文字规则基于 OpenCode 提交 `cc4b45612974f735ddec46009ede07729511fba4`，主要参考：

- `packages/tui/src/theme/assets/opencode.json`
- `packages/tui/src/theme/index.ts`

## 低侵入原则

1. 不修改通用 `Theme` 结构，不为 OpenCode 专属样式增加公共字段。
2. OpenCode 的颜色构造和 Markdown 映射集中在 `theme/opencode.rs`。
3. OpenCode 的代码语法配色集中在独立的 `assets/opencode.tmTheme`。
4. 现有模块只添加必要的枚举注册、缓存解码、样式分派和设置项。
5. 不覆盖现有主题，也不改变默认主题。

## 实现步骤

1. 新增 `ThemeKind::OpenCode`，接入名称解析、主题列表、持久化和终端色彩能力筛选。
2. 在独立模块中实现 OpenCode 深色配色及专属 Markdown 样式。
3. 新增 OpenCode Syntect 主题，并按当前主题选择对应语法高亮。
4. 在设置界面和 `/theme` 选择器中公开 `opencode`。
5. 补充主题解析、颜色映射、Markdown 样式和语法主题测试。
6. 运行格式化、渲染层测试和 pager 相关检查。

## 预期上游冲突面

长期维护时，主要关注以下薄注册点：

- `theme/mod.rs`：主题枚举及构造分派。
- `theme/cache.rs`：枚举的原子缓存解码。
- `syntax.rs`：代码语法主题分派。
- `settings/defs.rs`：设置界面的主题目录。

OpenCode 的具体视觉实现位于独立文件中，通常不会与上游主题调整发生内容冲突。

## 后续同步

更新 OpenCode 版本时，优先对比上述两个源文件；若只是颜色值或 Markdown 规则变化，通常只需修改 `theme/opencode.rs` 和 `assets/opencode.tmTheme`。若 grok 上游调整了主题枚举或设置目录，再复核四个薄注册点即可。
