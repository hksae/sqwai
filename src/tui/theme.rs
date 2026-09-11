use ratatui::style::{Color, Modifier, Style};

/// Fixed Codex-style palette for the TUI: ANSI-16 names only, no custom RGB.
///
/// Rules (mirroring `codex-rs/tui/styles.md`):
/// - primary text is the terminal default (`Reset`), secondary is dim gray;
/// - `Cyan` = tips, selection, status, accents; `Green` = success;
/// - `Red` = errors; `Yellow` = busy/attention (dark terminals);
/// - `Magenta` = special headers; `LightBlue` = ordered list markers;
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
    pub const fn MAGENTA() -> Color {
        Color::Magenta
    }
    #[allow(non_snake_case)]
    pub const fn LIGHT_BLUE() -> Color {
        Color::LightBlue
    }
    #[allow(non_snake_case)]
    pub const fn GREEN() -> Color {
        Color::Green
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
