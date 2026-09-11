use ratatui::style::Color;
use xai_grok_pager_render::syntax::Syntect;
use xai_grok_pager_render::theme::cache;
use xai_grok_pager_render::theme::md_style;
use xai_grok_pager_render::theme::{Theme, ThemeKind};

/// 验证公开主题目录能够解析 OpenCode，并保留源主题的关键颜色。
#[test]
fn opencode_theme_public_contract() {
    assert_eq!(ThemeKind::from_name("opencode"), Some(ThemeKind::OpenCode));
    assert_eq!(ThemeKind::from_name("open-code"), Some(ThemeKind::OpenCode));
    assert_eq!(ThemeKind::from_name("oc"), Some(ThemeKind::OpenCode));
    assert!(ThemeKind::selectable().contains(&ThemeKind::OpenCode));
    assert!(ThemeKind::available().contains(&ThemeKind::OpenCode));
    assert_ne!(ThemeKind::OpenCode as u8, ThemeKind::Terminal as u8);

    let theme = Theme::opencode();
    assert_eq!(theme.bg_base, Color::Rgb(0x0a, 0x0a, 0x0a));
    assert_eq!(theme.bg_light, Color::Rgb(0x14, 0x14, 0x14));
    assert_eq!(theme.accent_user, Color::Rgb(0x5c, 0x9c, 0xf5));
    assert_eq!(theme.text_primary, Color::Rgb(0xee, 0xee, 0xee));
}

/// 验证独立 TextMate 资源能够由生产语法高亮器加载。
#[test]
fn opencode_syntax_theme_loads() {
    let syntect = Syntect::new(include_bytes!("../assets/opencode.tmTheme"));
    assert!(syntect.syntax_set.find_syntax_by_token("rust").is_some());
}

/// 验证 Markdown 渲染入口会分派到 OpenCode 专属文字规则。
#[test]
fn opencode_markdown_dispatches_to_dedicated_style() {
    cache::set(ThemeKind::OpenCode);
    assert_eq!(cache::current_kind(), ThemeKind::OpenCode);
    let style = md_style::style();
    cache::set(ThemeKind::GrokNight);

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
    assert!(
        style
            .emphasis_inner
            .get_effects()
            .contains(anstyle::Effects::ITALIC)
    );
    assert!(
        !style
            .inline_code_inner
            .get_effects()
            .contains(anstyle::Effects::BOLD)
    );
}
