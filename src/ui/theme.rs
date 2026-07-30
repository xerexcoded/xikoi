use std::env;

use ratatui::{
    style::{Color, Modifier, Style},
    symbols,
};

use crate::{config::ColorMode, domain::Role};

use super::{ActivityState, LinkState};

const ASCII_BORDER: symbols::border::Set<'static> = symbols::border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

/// Resolved color capability used for a render.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorProfile {
    Truecolor,
    Ansi,
    Mono,
}

/// User and environment display preferences.
#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThemeOptions {
    pub color_mode: ColorMode,
    /// `NO_COLOR` always wins, including over an explicit color mode.
    pub no_color: bool,
    /// Avoid Braille, box-drawing, and other enhanced glyphs.
    pub ascii: bool,
    /// Whether `Auto` may select truecolor.
    pub truecolor_available: bool,
}

impl ThemeOptions {
    pub const fn new(color_mode: ColorMode) -> Self {
        Self {
            color_mode,
            no_color: false,
            ascii: false,
            truecolor_available: false,
        }
    }

    /// Resolve conventional terminal capability environment variables once at
    /// application startup and pass the returned value into each [`UiView`].
    pub fn from_env(color_mode: ColorMode) -> Self {
        let no_color = env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        let term_is_dumb = env::var("TERM").is_ok_and(|value| value == "dumb");
        let truecolor_available = env::var("COLORTERM").is_ok_and(|value| {
            value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit")
        });

        Self {
            color_mode,
            no_color,
            ascii: term_is_dumb,
            truecolor_available,
        }
    }

    pub const fn with_ascii(mut self, ascii: bool) -> Self {
        self.ascii = ascii;
        self
    }

    pub const fn with_no_color(mut self, no_color: bool) -> Self {
        self.no_color = no_color;
        self
    }

    pub const fn with_truecolor_available(mut self, available: bool) -> Self {
        self.truecolor_available = available;
        self
    }

    pub const fn profile(self) -> ColorProfile {
        if self.no_color {
            return ColorProfile::Mono;
        }
        match self.color_mode {
            ColorMode::Truecolor => ColorProfile::Truecolor,
            ColorMode::Mono => ColorProfile::Mono,
            ColorMode::Auto if self.truecolor_available => ColorProfile::Truecolor,
            ColorMode::Ansi | ColorMode::Auto => ColorProfile::Ansi,
        }
    }
}

/// Neuromancer-inspired styles and glyphs resolved for one capability profile.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub profile: ColorProfile,
    pub ascii: bool,
    pub base: Style,
    pub panel: Style,
    pub text: Style,
    pub muted: Style,
    pub border: Style,
    pub accent: Style,
    pub secondary: Style,
    pub warning: Style,
    pub danger: Style,
    pub code: Style,
    pub code_block: Style,
    pub selected: Style,
    pub scrollbar_track: Style,
    pub scrollbar_thumb: Style,
}

impl Theme {
    #[allow(clippy::too_many_lines)]
    pub fn new(options: ThemeOptions) -> Self {
        let profile = options.profile();
        let (
            base,
            panel,
            text,
            muted,
            border,
            accent,
            secondary,
            warning,
            danger,
            code,
            code_block,
        ) = match profile {
            ColorProfile::Truecolor => (
                Style::new()
                    .fg(Color::Rgb(170, 224, 184))
                    .bg(Color::Rgb(3, 8, 5)),
                Style::new()
                    .fg(Color::Rgb(174, 230, 188))
                    .bg(Color::Rgb(4, 12, 7)),
                Style::new()
                    .fg(Color::Rgb(174, 230, 188))
                    .bg(Color::Rgb(4, 12, 7)),
                Style::new()
                    .fg(Color::Rgb(73, 126, 86))
                    .bg(Color::Rgb(4, 12, 7)),
                Style::new()
                    .fg(Color::Rgb(37, 184, 91))
                    .bg(Color::Rgb(4, 12, 7)),
                Style::new()
                    .fg(Color::Rgb(78, 255, 135))
                    .bg(Color::Rgb(4, 12, 7))
                    .add_modifier(Modifier::BOLD),
                Style::new()
                    .fg(Color::Rgb(151, 255, 181))
                    .bg(Color::Rgb(4, 12, 7)),
                Style::new()
                    .fg(Color::Rgb(255, 191, 55))
                    .bg(Color::Rgb(4, 12, 7))
                    .add_modifier(Modifier::BOLD),
                Style::new()
                    .fg(Color::Rgb(255, 67, 91))
                    .bg(Color::Rgb(4, 12, 7))
                    .add_modifier(Modifier::BOLD),
                Style::new()
                    .fg(Color::Rgb(106, 255, 159))
                    .bg(Color::Rgb(9, 28, 15))
                    .add_modifier(Modifier::BOLD),
                Style::new()
                    .fg(Color::Rgb(161, 255, 190))
                    .bg(Color::Rgb(6, 22, 11)),
            ),
            ColorProfile::Ansi => (
                Style::new().fg(Color::Green).bg(Color::Black),
                Style::new().fg(Color::Green).bg(Color::Black),
                Style::new().fg(Color::Green).bg(Color::Black),
                Style::new().fg(Color::DarkGray).bg(Color::Black),
                Style::new().fg(Color::Green).bg(Color::Black),
                Style::new()
                    .fg(Color::LightGreen)
                    .bg(Color::Black)
                    .add_modifier(Modifier::BOLD),
                Style::new().fg(Color::LightGreen).bg(Color::Black),
                Style::new()
                    .fg(Color::LightYellow)
                    .bg(Color::Black)
                    .add_modifier(Modifier::BOLD),
                Style::new()
                    .fg(Color::LightRed)
                    .bg(Color::Black)
                    .add_modifier(Modifier::BOLD),
                Style::new()
                    .fg(Color::LightGreen)
                    .bg(Color::Black)
                    .add_modifier(Modifier::BOLD),
                Style::new().fg(Color::Green).bg(Color::Black),
            ),
            ColorProfile::Mono => (
                Style::new(),
                Style::new(),
                Style::new(),
                Style::new().add_modifier(Modifier::DIM),
                Style::new(),
                Style::new().add_modifier(Modifier::BOLD),
                Style::new().add_modifier(Modifier::UNDERLINED),
                Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED),
                Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED),
                Style::new().add_modifier(Modifier::REVERSED),
            ),
        };

        Self {
            profile,
            ascii: options.ascii,
            base,
            panel,
            text,
            muted,
            border,
            accent,
            secondary,
            warning,
            danger,
            code,
            code_block,
            selected: accent.add_modifier(Modifier::REVERSED),
            scrollbar_track: muted,
            scrollbar_thumb: accent,
        }
    }

    pub const fn border_set(self) -> symbols::border::Set<'static> {
        if self.ascii {
            ASCII_BORDER
        } else {
            symbols::border::ROUNDED
        }
    }

    pub const fn role_style(self, role: Role) -> Style {
        match role {
            Role::System | Role::User => self.secondary,
            Role::Assistant => self.accent,
        }
    }

    pub const fn link_style(self, state: LinkState) -> Style {
        match state {
            LinkState::Online => self.accent,
            LinkState::Connecting => self.secondary,
            LinkState::Offline => self.danger,
        }
    }

    pub const fn activity_style(self, state: ActivityState) -> Style {
        match state {
            ActivityState::Idle => self.muted,
            ActivityState::Thinking | ActivityState::Streaming => self.secondary,
            ActivityState::Cancelling => self.warning,
            ActivityState::Failed => self.danger,
        }
    }

    pub const fn role_glyph(self, role: Role) -> &'static str {
        match (self.ascii, role) {
            (true, Role::System) => "SYS",
            (true, Role::User) => ">",
            (true, Role::Assistant) => "AI",
            (false, Role::System) => "⌁",
            (false, Role::User) => "λ",
            (false, Role::Assistant) => "◆",
        }
    }

    pub const fn link_glyph(self, state: LinkState) -> &'static str {
        match (self.ascii, state) {
            (true, LinkState::Online) => "*",
            (true, LinkState::Connecting) => "~",
            (true, LinkState::Offline) => "x",
            (false, LinkState::Online) => "●",
            (false, LinkState::Connecting) => "◐",
            (false, LinkState::Offline) => "○",
        }
    }

    pub const fn activity_glyph(self, state: ActivityState) -> &'static str {
        match (self.ascii, state) {
            (true, ActivityState::Idle) => "-",
            (true, ActivityState::Thinking | ActivityState::Streaming) => "~",
            (true, ActivityState::Cancelling) => "!",
            (true, ActivityState::Failed) => "x",
            (false, ActivityState::Idle) => "·",
            (false, ActivityState::Thinking) => "∴",
            (false, ActivityState::Streaming) => "∿",
            (false, ActivityState::Cancelling) => "◌",
            (false, ActivityState::Failed) => "×",
        }
    }

    pub const fn bullet(self) -> &'static str {
        if self.ascii { "- " } else { "• " }
    }

    pub const fn quote(self) -> &'static str {
        if self.ascii { "| " } else { "│ " }
    }

    pub const fn horizontal_rule(self) -> &'static str {
        if self.ascii {
            "--------------------------------"
        } else {
            "────────────────────────────────"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_overrides_every_requested_mode() {
        let options = ThemeOptions::new(ColorMode::Truecolor).with_no_color(true);
        assert_eq!(options.profile(), ColorProfile::Mono);
        let theme = Theme::new(options);
        for style in [
            theme.base,
            theme.panel,
            theme.text,
            theme.muted,
            theme.border,
            theme.accent,
            theme.secondary,
            theme.warning,
            theme.danger,
            theme.code,
            theme.code_block,
        ] {
            assert_eq!(style.fg, None);
            assert_eq!(style.bg, None);
        }
    }

    #[test]
    fn auto_only_uses_truecolor_when_capability_is_known() {
        assert_eq!(
            ThemeOptions::new(ColorMode::Auto).profile(),
            ColorProfile::Ansi
        );
        assert_eq!(
            ThemeOptions::new(ColorMode::Auto)
                .with_truecolor_available(true)
                .profile(),
            ColorProfile::Truecolor
        );
    }

    #[test]
    fn ascii_profile_avoids_enhanced_status_glyphs() {
        let theme = Theme::new(ThemeOptions::new(ColorMode::Mono).with_ascii(true));
        assert!(theme.role_glyph(Role::Assistant).is_ascii());
        assert!(theme.link_glyph(LinkState::Online).is_ascii());
        assert!(theme.activity_glyph(ActivityState::Streaming).is_ascii());
        assert_eq!(theme.border_set(), ASCII_BORDER);
    }

    #[test]
    fn normal_truecolor_roles_stay_in_the_phosphor_green_family() {
        let theme = Theme::new(ThemeOptions::new(ColorMode::Truecolor));
        for style in [
            theme.base,
            theme.panel,
            theme.text,
            theme.muted,
            theme.border,
            theme.accent,
            theme.secondary,
            theme.code,
            theme.code_block,
        ] {
            let Some(Color::Rgb(red, green, blue)) = style.fg else {
                panic!("truecolor normal style must use an RGB foreground");
            };
            assert!(
                green >= red && green >= blue,
                "normal foreground must be green-dominant: {red},{green},{blue}"
            );
        }

        assert!(matches!(theme.warning.fg, Some(Color::Rgb(255, 191, 55))));
        assert!(matches!(theme.danger.fg, Some(Color::Rgb(255, 67, 91))));
    }

    #[test]
    fn ansi_profile_uses_only_green_normals_with_amber_and_crimson_alerts() {
        let theme = Theme::new(ThemeOptions::new(ColorMode::Ansi));
        for style in [
            theme.base,
            theme.panel,
            theme.text,
            theme.muted,
            theme.border,
            theme.accent,
            theme.secondary,
            theme.code,
            theme.code_block,
        ] {
            assert!(matches!(
                style.fg,
                Some(Color::Green | Color::LightGreen | Color::DarkGray)
            ));
        }
        assert_eq!(theme.warning.fg, Some(Color::LightYellow));
        assert_eq!(theme.danger.fg, Some(Color::LightRed));
    }
}
