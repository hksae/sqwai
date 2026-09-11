use ratatui::style::{Color, Modifier, Style};

use crate::config::EffortLevel;

/// Fixed Codex-style palette for the TUI: ANSI-16 names only, no custom RGB.
///
/// Rules (mirroring `codex-rs/tui/styles.md`):
/// - primary text is the terminal default (`Reset`), secondary is dim gray;
/// - `Cyan` = tips, selection, status, accents, tool names; `Green` = success;
/// - `Red` = errors; `Yellow` = busy/attention (dark terminals);
/// - `LightBlue` = ordered list markers; `Green` = blockquotes;
/// - no painted backgrounds: everything is transparent over the terminal.
/// Dark terminal is assumed (no light-theme probing like Codex `color.rs`).
pub struct Theme;

impl Theme {
    #[allow(non_snake_case)]
    pub const fn BG() -> Color {
        Color::Reset
    }
    #[allow(non_snake_case)]
    pub const fn SURFACE() -> Color {
        Color::Reset
    }
    /// User message band: Codex-style white-12%-over-dark (~#262626).
    /// A 256-color gray (not a custom RGB): present on every modern terminal.
    #[allow(non_snake_case)]
    pub const fn USER_SURFACE() -> Color {
        Color::Indexed(235)
    }
    #[allow(non_snake_case)]
    pub const fn FG() -> Color {
        Color::Reset
    }
    #[allow(non_snake_case)]
    pub const fn DIM() -> Color {
        Color::DarkGray
    }
    #[allow(non_snake_case)]
    pub const fn ACCENT() -> Color {
        Color::Cyan
    }
    #[allow(non_snake_case)]
    pub const fn ACCENT_SOFT() -> Color {
        Color::Cyan
    }
    #[allow(non_snake_case)]
    #[allow(dead_code)]
    pub const fn BORDER() -> Color {
        Color::Reset
    }
    #[allow(non_snake_case)]
    pub const fn BORDER_DIM() -> Color {
        Color::DarkGray
    }
    #[allow(non_snake_case)]
    pub const fn OK() -> Color {
        Color::Green
    }
    #[allow(non_snake_case)]
    pub const fn ERR() -> Color {
        Color::Red
    }
    #[allow(non_snake_case)]
    pub const fn WARN() -> Color {
        Color::Yellow
    }
    #[allow(non_snake_case)]
    pub const fn LIGHT_BLUE() -> Color {
        Color::LightBlue
    }
    #[allow(non_snake_case)]
    pub const fn GREEN() -> Color {
        Color::Green
    }
    /// Blockquote band: dark translucent-green on dark terminals. The `▐`
    /// rail itself stays on the default background; the band starts right
    /// of it. Text stays the bright green fg.
    #[allow(non_snake_case)]
    pub const fn QUOTE_BG() -> Color {
        Color::Rgb(18, 31, 18)
    }
    /// Slider/status color per effort level: off gray → low green →
    /// medium cyan (house accent) → high light-blue → xhigh yellow →
    /// max magenta. First cut for live review; tune after seeing it.
    pub const fn effort_color(level: EffortLevel) -> Color {
        match level {
            EffortLevel::Off => Color::DarkGray,
            EffortLevel::Low => Color::Green,
            EffortLevel::Medium => Color::Cyan,
            EffortLevel::High => Color::LightBlue,
            EffortLevel::Xhigh => Color::Yellow,
            EffortLevel::Max => Color::Magenta,
        }
    }

    pub fn effort(level: EffortLevel) -> Style {
        Style::new()
            .fg(Self::effort_color(level))
            .add_modifier(Modifier::BOLD)
    }

    pub fn base() -> Style {
        Style::new()
    }
    pub fn dim() -> Style {
        Style::new().fg(Self::DIM())
    }
    pub fn accent() -> Style {
        Style::new().fg(Self::ACCENT())
    }
    pub fn accent_bold() -> Style {
        Style::new()
            .fg(Self::ACCENT())
            .add_modifier(Modifier::BOLD)
    }
    #[allow(dead_code)]
    pub fn label_user() -> Style {
        Self::accent_bold()
    }
    #[allow(dead_code)]
    pub fn label_agent() -> Style {
        Style::new()
            .fg(Self::ACCENT())
            .add_modifier(Modifier::BOLD)
    }
    #[allow(dead_code)]
    pub fn border_focused() -> Style {
        Style::new().fg(Self::BORDER())
    }
    #[allow(dead_code)]
    pub fn border_dim() -> Style {
        Style::new().fg(Self::BORDER_DIM())
    }
    /// Frames of every modal menu and popup: plain white, so overlays read
    /// as a surface above the dimmed chat.
    pub fn border_popup() -> Style {
        Style::new().fg(Color::White)
    }
    /// Busy/attention status: bold yellow, no chip background (Codex status).
    pub fn status_chip() -> Style {
        Style::new()
            .fg(Self::WARN())
            .add_modifier(Modifier::BOLD)
    }
    pub fn ok() -> Style {
        Style::new()
            .fg(Self::OK())
            .add_modifier(Modifier::BOLD)
    }
    pub fn err() -> Style {
        Style::new()
            .fg(Self::ERR())
            .add_modifier(Modifier::BOLD)
    }
    pub fn warn() -> Style {
        Style::new()
            .fg(Self::WARN())
            .add_modifier(Modifier::BOLD)
    }

    /// table separators / horizontal rules
    pub fn rule_color() -> Color {
        Self::BORDER_DIM()
    }
}
