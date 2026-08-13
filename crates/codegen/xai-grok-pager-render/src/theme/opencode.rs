//! OpenCode TUI 视觉主题。
//!
//! 本模块集中保存从 OpenCode 默认 TUI 移植的颜色和 Markdown 文字规则，
//! 避免将下游定制散落到通用主题结构中，降低后续合并上游版本时的冲突面。

use anstyle::{Ansi256Color, AnsiColor, Color as AnsiStyleColor, Style as AnsiStyle};
use ratatui::style::{Color, Modifier};
use xai_grok_markdown::MarkdownStyle;

use super::Theme;

/// 创建简洁的 RGB 颜色常量。
const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// OpenCode 默认深色主题的原始颜色。
mod palette {
    use super::*;

    pub const BACKGROUND: Color = rgb(0x0a, 0x0a, 0x0a);
    pub const BACKGROUND_PANEL: Color = rgb(0x14, 0x14, 0x14);
    pub const BACKGROUND_ELEMENT: Color = rgb(0x1e, 0x1e, 0x1e);
    pub const BACKGROUND_HOVER: Color = rgb(0x28, 0x28, 0x28);
    pub const BACKGROUND_SELECTED: Color = rgb(0x32, 0x32, 0x32);
    pub const BORDER_SUBTLE: Color = rgb(0x3c, 0x3c, 0x3c);
    pub const BORDER: Color = rgb(0x48, 0x48, 0x48);
    pub const BORDER_ACTIVE: Color = rgb(0x60, 0x60, 0x60);
    pub const TEXT: Color = rgb(0xee, 0xee, 0xee);
    pub const TEXT_MUTED: Color = rgb(0x80, 0x80, 0x80);
    pub const PRIMARY: Color = rgb(0xfa, 0xb2, 0x83);
    pub const SECONDARY: Color = rgb(0x5c, 0x9c, 0xf5);
    pub const ACCENT: Color = rgb(0x9d, 0x7c, 0xd8);
    pub const ERROR: Color = rgb(0xe0, 0x6c, 0x75);
    pub const WARNING: Color = rgb(0xf5, 0xa7, 0x42);
    pub const SUCCESS: Color = rgb(0x7f, 0xd8, 0x8f);
    pub const INFO: Color = rgb(0x56, 0xb6, 0xc2);
    pub const MARKDOWN_YELLOW: Color = rgb(0xe5, 0xc0, 0x7b);
    pub const DIFF_ADDED: Color = rgb(0x4f, 0xd6, 0xbe);
    pub const DIFF_REMOVED: Color = rgb(0xc5, 0x3b, 0x53);
    pub const DIFF_ADDED_BG: Color = rgb(0x20, 0x30, 0x3b);
    pub const DIFF_REMOVED_BG: Color = rgb(0x37, 0x22, 0x2c);
    pub const DIFF_CONTEXT: Color = rgb(0x82, 0x8b, 0xb8);
    pub const DIFF_GUTTER: Color = rgb(0x8f, 0x8f, 0x8f);
}

use palette::*;

/// 构建供 grok 通用渲染组件使用的 OpenCode 语义主题。
pub(super) const fn theme() -> Theme {
    Theme {
        bg_base: BACKGROUND,
        bg_light: BACKGROUND_PANEL,
        bg_dark: BACKGROUND_ELEMENT,
        bg_highlight: BACKGROUND_ELEMENT,
        bg_hover: BACKGROUND_HOVER,
        bg_terminal: BACKGROUND,

        accent_user: SECONDARY,
        accent_assistant: ACCENT,
        accent_thinking: MARKDOWN_YELLOW,
        accent_tool: TEXT_MUTED,
        accent_system: INFO,
        accent_error: ERROR,
        accent_success: SUCCESS,
        accent_running: ACCENT,
        accent_skill: INFO,

        text_primary: TEXT,
        text_secondary: TEXT_MUTED,

        gray_dim: BORDER_ACTIVE,
        gray: TEXT_MUTED,
        gray_bright: rgb(0xa0, 0xa0, 0xa0),

        command: MARKDOWN_YELLOW,
        path: PRIMARY,
        running: INFO,
        warning: WARNING,

        fuzzy_accent: PRIMARY,
        accent_plan: WARNING,
        accent_verify: ACCENT,
        accent_remember: SUCCESS,

        selection_border: BORDER,
        hover_border: BORDER_SUBTLE,
        prompt_border: BORDER,
        prompt_border_active: BORDER_ACTIVE,

        accent_model: INFO,

        scrollbar_bg: BACKGROUND_PANEL,
        scrollbar_fg: BORDER_ACTIVE,

        diff_delete_bg: DIFF_REMOVED_BG,
        diff_delete_fg: DIFF_REMOVED,
        diff_insert_bg: DIFF_ADDED_BG,
        diff_insert_fg: DIFF_ADDED,
        diff_equal_fg: DIFF_CONTEXT,
        diff_gutter_fg: DIFF_GUTTER,

        bg_visual: BACKGROUND_SELECTED,

        paste_bg: BACKGROUND_ELEMENT,
        paste_fg: WARNING,
        paste_dim: TEXT_MUTED,

        // 通用字段保留合理回退值；精确的 OpenCode Markdown 规则由
        // `markdown_style` 独立生成，不扩张上游的 Theme 数据结构。
        md_heading_h1: ACCENT,
        md_heading_h1_mod: Modifier::BOLD,
        md_heading_h2: ACCENT,
        md_heading_h2_mod: Modifier::BOLD,
        md_heading_h3: ACCENT,
        md_heading_h3_mod: Modifier::BOLD,
        md_heading_h4: ACCENT,
        md_heading_h4_mod: Modifier::BOLD,
        md_heading_h5: ACCENT,
        md_heading_h5_mod: Modifier::BOLD,
        md_heading_h6: ACCENT,
        md_heading_h6_mod: Modifier::BOLD,
        md_code: SUCCESS,
        md_task_checked: SUCCESS,
        md_task_unchecked: TEXT_MUTED,
        md_muted: TEXT_MUTED,
        md_code_bg: BACKGROUND_PANEL,
        md_text: TEXT,
        link_fg: INFO,
    }
}

/// 将已按终端能力量化的 Ratatui 颜色转换为 Markdown 渲染器颜色。
fn to_anstyle(color: Color) -> Option<AnsiStyleColor> {
    Some(match color {
        Color::Reset => return None,
        Color::Rgb(r, g, b) => AnsiStyleColor::Rgb(anstyle::RgbColor(r, g, b)),
        Color::Indexed(index) => AnsiStyleColor::Ansi256(Ansi256Color(index)),
        Color::Black => AnsiStyleColor::Ansi(AnsiColor::Black),
        Color::Red => AnsiStyleColor::Ansi(AnsiColor::Red),
        Color::Green => AnsiStyleColor::Ansi(AnsiColor::Green),
        Color::Yellow => AnsiStyleColor::Ansi(AnsiColor::Yellow),
        Color::Blue => AnsiStyleColor::Ansi(AnsiColor::Blue),
        Color::Magenta => AnsiStyleColor::Ansi(AnsiColor::Magenta),
        Color::Cyan => AnsiStyleColor::Ansi(AnsiColor::Cyan),
        Color::Gray => AnsiStyleColor::Ansi(AnsiColor::White),
        Color::DarkGray => AnsiStyleColor::Ansi(AnsiColor::BrightBlack),
        Color::LightRed => AnsiStyleColor::Ansi(AnsiColor::BrightRed),
        Color::LightGreen => AnsiStyleColor::Ansi(AnsiColor::BrightGreen),
        Color::LightYellow => AnsiStyleColor::Ansi(AnsiColor::BrightYellow),
        Color::LightBlue => AnsiStyleColor::Ansi(AnsiColor::BrightBlue),
        Color::LightMagenta => AnsiStyleColor::Ansi(AnsiColor::BrightMagenta),
        Color::LightCyan => AnsiStyleColor::Ansi(AnsiColor::BrightCyan),
        Color::White => AnsiStyleColor::Ansi(AnsiColor::BrightWhite),
    })
}

/// 创建仅包含指定前景色的 Markdown 样式。
fn fg(color: Color) -> AnsiStyle {
    AnsiStyle::new().fg_color(to_anstyle(color))
}

/// 创建仅包含指定背景色的 Markdown 样式。
fn bg(color: Color) -> AnsiStyle {
    AnsiStyle::new().bg_color(to_anstyle(color))
}

/// 使用 OpenCode 的文字层级构建 Markdown 样式。
///
/// `theme` 必须来自 `Theme::current()`，因此这里使用的颜色已经过终端
/// 真彩色、256 色或 ANSI-16 能力适配。
pub(super) fn markdown_style(theme: &Theme) -> MarkdownStyle {
    let heading = fg(theme.md_heading_h1).bold();
    let headings = [
        heading.underline(),
        heading,
        heading,
        heading,
        heading,
        heading,
    ];
    let heading_outer = [
        fg(theme.md_heading_h1).dimmed().hidden(),
        fg(theme.md_heading_h2).dimmed().hidden(),
        fg(theme.md_heading_h3).dimmed().hidden(),
        fg(theme.md_heading_h4).dimmed().hidden(),
        fg(theme.md_heading_h5).dimmed().hidden(),
        fg(theme.md_heading_h6).dimmed().hidden(),
    ];

    MarkdownStyle {
        heading_inner: headings,
        heading_outer,
        strong_inner: fg(theme.warning).bold(),
        strong_outer: AnsiStyle::new().dimmed().hidden(),
        emphasis_inner: fg(theme.command).italic(),
        emphasis_outer: AnsiStyle::new().dimmed().hidden(),
        strikethrough_inner: fg(theme.gray).strikethrough(),
        strikethrough_outer: AnsiStyle::new().dimmed().hidden(),
        inline_code_inner: fg(theme.md_code),
        inline_code_outer: fg(theme.md_code).dimmed().hidden(),
        blockquote_outer: fg(theme.command).italic(),
        task_checked: fg(theme.md_task_checked),
        task_unchecked: fg(theme.md_task_unchecked).dimmed(),
        list_item: fg(theme.path),
        rule: fg(theme.gray),
        link_outer: fg(theme.path).underline(),
        link_text: fg(theme.link_fg).underline(),
        link_url: fg(theme.path).underline(),
        link_title: fg(theme.gray),
        code_outer: fg(theme.md_code).dimmed().hidden(),
        code_language: fg(theme.gray).hidden(),
        code_untagged: fg(theme.md_text),
        code_background: bg(theme.md_code_bg),
        table_outer: fg(theme.gray).hidden(),
        text: fg(theme.md_text),
        math: fg(theme.md_text).italic(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证核心语义字段保留 OpenCode 默认深色配色。
    #[test]
    fn palette_matches_opencode_dark_defaults() {
        let theme = theme();
        assert_eq!(theme.bg_base, Color::Rgb(0x0a, 0x0a, 0x0a));
        assert_eq!(theme.bg_light, Color::Rgb(0x14, 0x14, 0x14));
        assert_eq!(theme.accent_user, Color::Rgb(0x5c, 0x9c, 0xf5));
        assert_eq!(theme.text_primary, Color::Rgb(0xee, 0xee, 0xee));
    }

    /// 验证 Markdown 标题、粗体和行内代码的文字层级。
    #[test]
    fn markdown_preserves_opencode_emphasis_hierarchy() {
        let theme = theme();
        let style = markdown_style(&theme);
        assert_eq!(style.heading_inner[0].get_fg_color(), to_anstyle(ACCENT));
        assert!(
            style.heading_inner[0]
                .get_effects()
                .contains(anstyle::Effects::BOLD)
        );
        assert!(
            style.heading_inner[0]
                .get_effects()
                .contains(anstyle::Effects::UNDERLINE)
        );
        assert_eq!(style.strong_inner.get_fg_color(), to_anstyle(WARNING));
        assert_eq!(style.inline_code_inner.get_fg_color(), to_anstyle(SUCCESS));
    }
}
