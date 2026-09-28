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
///
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
    /// Raised surface for flat modal panels (menus, completion popups):
    /// one step above the terminal background, one below USER_SURFACE, two
    /// below the selection fill. Dim-only separation merges on terminals
    /// with a weak DIM — the surface guarantees depth everywhere.
    #[allow(non_snake_case)]
    pub const fn MENU_BG() -> Color {
        Color::Indexed(233)
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
    /// Second rung of the gray ladder: metadata, secondary columns, key
    /// states. FG = data, META = meta, DIM = hints/chrome. Three rungs,
    /// no more — everything else earns color by semantics.
    #[allow(non_snake_case)]
    pub const fn META() -> Color {
        Color::Gray
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
    /// Slider/status color per effort level. Bright variants throughout:
    /// bold text gets brightened by most terminals anyway (Windows Terminal
    /// default), so the palette bakes the light hues in directly — the same
    /// hue everywhere, bold or not, instead of two faces per level.
    pub const fn effort_color(level: EffortLevel) -> Color {
        match level {
            EffortLevel::Off => Color::Gray,
            EffortLevel::Low => Color::LightGreen,
            EffortLevel::Medium => Color::LightCyan,
            EffortLevel::High => Color::LightBlue,
            EffortLevel::Xhigh => Color::LightYellow,
            EffortLevel::Max => Color::LightMagenta,
        }
    }
    /// Truecolor twin of [`Self::effort_color`] for the slider sweep:
    /// close to the indexed colors so the settle frame does not pop.
    pub const fn effort_rgb(level: EffortLevel) -> (u8, u8, u8) {
        match level {
            EffortLevel::Off => (170, 170, 170),
            EffortLevel::Low => (140, 240, 140),
            EffortLevel::Medium => (140, 240, 240),
            EffortLevel::High => (110, 165, 255),
            EffortLevel::Xhigh => (250, 250, 160),
            EffortLevel::Max => (232, 160, 232),
        }
    }

    pub fn effort(level: EffortLevel) -> Style {
        // no BOLD: the hues are bright-baked already, and bold would let
        // brightening terminals show a second face of the same level
        Style::new().fg(Self::effort_color(level))
    }

    pub fn base() -> Style {
        Style::new()
    }
    pub fn dim() -> Style {
        Style::new().fg(Self::DIM())
    }
    pub fn meta() -> Style {
        Style::new().fg(Self::META())
    }
    /// Selection marker (numbers): house accent, never LightBlue.
    pub fn marker() -> Style {
        Style::new().fg(Self::ACCENT()).add_modifier(Modifier::BOLD)
    }
    /// Pinned-row marker: plain white dot, deliberately quiet — it marks
    /// state, not focus, so it must not compete with the cyan accent.
    pub fn pin_marker() -> Style {
        Style::new().fg(Color::White)
    }
    /// Selected menu row: subtle fill + bright text. Fill is a 256-color
    /// gray like USER_SURFACE (no custom RGB, no transparency games), so
    /// the row reads as one band on every dark terminal.
    #[allow(non_snake_case)]
    pub const fn SELECTION_BG() -> Color {
        Color::Indexed(237)
    }
    pub fn selection() -> Style {
        Style::new()
            .bg(Self::SELECTION_BG())
            .add_modifier(Modifier::BOLD)
    }
    /// Hint chips: `key` bright + `desc` dim, joined with a dim `·`.
    /// `(key, desc)` pairs keep one visual language for every footer.
    pub fn hints(pairs: &[(&str, &str)]) -> ratatui::text::Line<'static> {
        use ratatui::text::Span;
        let mut spans = vec![Span::styled(" ", Self::dim())];
        for (i, (key, desc)) in pairs.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ", Self::dim()));
            }
            spans.push(Span::styled(
                key.to_string(),
                Style::new().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(format!(": {desc}"), Self::dim()));
        }
        spans.push(Span::styled(" ", Self::dim()));
        ratatui::text::Line::from(spans)
    }
    pub fn accent() -> Style {
        Style::new().fg(Self::ACCENT())
    }
    pub fn accent_bold() -> Style {
        Style::new().fg(Self::ACCENT()).add_modifier(Modifier::BOLD)
    }
    #[allow(dead_code)]
    pub fn label_user() -> Style {
        Self::accent_bold()
    }
    #[allow(dead_code)]
    pub fn label_agent() -> Style {
        Style::new().fg(Self::ACCENT()).add_modifier(Modifier::BOLD)
    }
    #[allow(dead_code)]
    pub fn border_focused() -> Style {
        Style::new().fg(Self::BORDER())
    }
    #[allow(dead_code)]
    pub fn border_dim() -> Style {
        Style::new().fg(Self::BORDER_DIM())
    }
    /// ACT mode chip: bold bright-green ("go/active"). Yellow read as a
    /// permanent warning; green states the mode without shouting. Plain
    /// Green is too dark on black terminals — LightGreen carries it.
    pub fn mode_chip_act() -> Style {
        Style::new()
            .fg(Color::LightGreen)
            .add_modifier(Modifier::BOLD)
    }
    /// PLAN mode chip: bold light-blue, mirrors the yellow ACT chip.
    pub fn mode_chip_plan() -> Style {
        Style::new()
            .fg(Self::LIGHT_BLUE())
            .add_modifier(Modifier::BOLD)
    }
    /// Tool call head rows: calm white, so calls scan as one list. State
    /// lives in the marker shape (✓/✗/spinner) and the finish wave —
    /// not in the row color.
    pub fn tool_head() -> Style {
        Style::new().fg(Color::White)
    }
    pub fn tool_head_bold() -> Style {
        Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
    }
    pub fn ok() -> Style {
        Style::new().fg(Self::OK()).add_modifier(Modifier::BOLD)
    }
    pub fn err() -> Style {
        Style::new().fg(Self::ERR()).add_modifier(Modifier::BOLD)
    }
    pub fn warn() -> Style {
        Style::new().fg(Self::WARN()).add_modifier(Modifier::BOLD)
    }

    /// table separators / horizontal rules
    pub fn rule_color() -> Color {
        Self::BORDER_DIM()
    }
}
