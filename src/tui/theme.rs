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
    /// one step above the terminal background — the composer band and user
    /// strip (INPUT_BG) sit one above this, the selection fill two above.
    /// Dim-only separation merges on terminals with a weak DIM — the surface
    /// guarantees depth everywhere.
    #[allow(non_snake_case)]
    pub const fn MENU_BG() -> Color {
        Color::Indexed(233)
    }
    /// Composer band and user message strip: Codex-style ~#1c1c1c, one step
    /// above the menu surface (233). Same family, quieter — your messages and
    /// your input read as one surface, distinct from the menu chrome.
    /// A 256-color gray (not a custom RGB): present on every modern terminal.
    #[allow(non_snake_case)]
    pub const fn INPUT_BG() -> Color {
        Color::Indexed(234)
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
    /// House accent: xterm 168 (`#d75f87`), the cube color nearest the brand
    /// ramp's loud end (`#de2f66`) and the same index the art falls back to
    /// without truecolor. Blue vibrated on black terminals and fought the
    /// brand identity everywhere; links keep their own blue.
    /// Indexed, not RGB — renders on terminals without truecolor.
    #[allow(non_snake_case)]
    pub const fn ACCENT() -> Color {
        Color::Indexed(168)
    }
    #[allow(non_snake_case)]
    pub const fn ACCENT_SOFT() -> Color {
        Color::Indexed(168)
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
    /// One green, one red, one amber for the whole UI. Verdicts (the tool dot,
    /// the finish wave, blockquotes, the ACT chip) and the effort scale's
    /// matching stops all read these three numbers: the app used to carry four
    /// greens and two reds, every one of them claiming to mean "good".
    pub const OK_RGB: (u8, u8, u8) = (135, 200, 145);
    pub const ERR_RGB: (u8, u8, u8) = (220, 150, 145);
    pub const WARN_RGB: (u8, u8, u8) = (222, 180, 124);
    #[allow(non_snake_case)]
    pub const fn OK() -> Color {
        let (r, g, b) = Self::OK_RGB;
        Color::Rgb(r, g, b)
    }
    #[allow(non_snake_case)]
    pub const fn ERR() -> Color {
        let (r, g, b) = Self::ERR_RGB;
        Color::Rgb(r, g, b)
    }
    #[allow(non_snake_case)]
    pub const fn WARN() -> Color {
        let (r, g, b) = Self::WARN_RGB;
        Color::Rgb(r, g, b)
    }
    #[allow(non_snake_case)]
    pub const fn LIGHT_BLUE() -> Color {
        Color::LightBlue
    }
    /// Blockquotes are green — so they are *the* green, not a fourth one.
    #[allow(non_snake_case)]
    pub const fn GREEN() -> Color {
        Self::OK()
    }
    /// Blockquote band: dark translucent-green on dark terminals. The `▐`
    /// rail itself stays on the default background; the band starts right
    /// of it. Text stays the bright green fg.
    #[allow(non_snake_case)]
    pub const fn QUOTE_BG() -> Color {
        Color::Rgb(18, 31, 18)
    }
    /// Effort palette, one source of truth for every effort-colored surface
    /// (slider track, dots, labels, status-bar chip). The slider sweep blends
    /// between these same numbers, so the frame it lands on *is* its endpoint:
    /// moving the level cannot leave the card brighter than it settles to.
    /// Low and Xhigh are the house green and amber, not near-misses of them:
    /// one color cannot mean two things in one glance.
    pub const fn effort_rgb(level: EffortLevel) -> (u8, u8, u8) {
        match level {
            EffortLevel::Off => (128, 128, 128),
            EffortLevel::Low => Self::OK_RGB,
            EffortLevel::Medium => (80, 220, 220),
            EffortLevel::High => (110, 165, 255),
            EffortLevel::Xhigh => Self::WARN_RGB,
            EffortLevel::Max => (220, 130, 220),
        }
    }
    pub const fn effort_color(level: EffortLevel) -> Color {
        let (r, g, b) = Self::effort_rgb(level);
        Color::Rgb(r, g, b)
    }

    /// One hue per level, never bold: a brightening terminal shows bold ANSI
    /// as a lighter rung, so bold would paint the label and the track under it
    /// as two faces of the same level.
    pub fn effort(level: EffortLevel) -> Style {
        Style::new().fg(Self::effort_color(level))
    }

    /// Focused form field: the label reads calm white on the same selection
    /// band list rows get — focus is position, not the cyan accent, which
    /// next to values reads as a link color.
    pub fn field_label_focused() -> Style {
        Style::new().fg(Color::White).patch(Self::selection())
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
    /// Section headers in list menus (pinned / today / dates): bold
    /// bright label, no dash rules — the weight carries the structure.
    pub fn section() -> Style {
        Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
    }
    /// List markers (ordered numbers, option bullets): house accent.
    pub fn marker() -> Style {
        Style::new().fg(Self::ACCENT()).add_modifier(Modifier::BOLD)
    }
    /// Selected menu row: subtle fill + bright text. Fill is a 256-color
    /// gray like INPUT_BG (no custom RGB, no transparency games), so
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
    /// Hint chips: `key` muted white + `desc` dim, joined with a dim `·`.
    /// `(key, desc)` pairs keep one visual language for every footer.
    pub fn hints(pairs: &[(&str, &str)]) -> ratatui::text::Line<'static> {
        use ratatui::text::Span;
        let mut spans = vec![Span::styled(" ", Self::dim())];
        for (i, (key, desc)) in pairs.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ", Self::dim()));
            }
            spans.push(Span::styled(key.to_string(), Self::meta()));
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
    /// ACT mode chip: the effort Low hue, never bold — same soft treatment
    /// as the effort palette it mirrors.
    pub fn mode_chip_act() -> Style {
        Style::new().fg(Self::effort_color(EffortLevel::Low))
    }
    /// PLAN mode chip: the effort High hue, never bold.
    pub fn mode_chip_plan() -> Style {
        Style::new().fg(Self::effort_color(EffortLevel::High))
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
    /// Status carries no bold: on a pastel ground bold is just a brighter
    /// ground, which is the loudness this palette stopped having a use for.
    /// The verdict lives in the marker's shape (✓/✗/spinner), as it always was.
    pub fn ok() -> Style {
        Style::new().fg(Self::OK())
    }
    pub fn err() -> Style {
        Style::new().fg(Self::ERR())
    }
    pub fn warn() -> Style {
        Style::new().fg(Self::WARN())
    }

    /// table separators / horizontal rules
    pub fn rule_color() -> Color {
        Self::BORDER_DIM()
    }

    /// Every color the UI uses, in one list for `/test colors`: name plus
    /// the style as used. New colors land here or they do not land at
    /// all — the gallery is how duplicates get spotted (ACCENT and
    /// ACCENT_SOFT are already the same index; BG/SURFACE/FG all Reset).
    pub fn palette() -> Vec<(&'static str, Style)> {
        let mut out = vec![
            ("FG", Style::new().fg(Self::FG())),
            ("DIM", Self::dim()),
            ("META", Self::meta()),
            ("ACCENT", Self::accent()),
            ("ACCENT_SOFT", Style::new().fg(Self::ACCENT_SOFT())),
            ("OK", Self::ok()),
            ("ERR", Self::err()),
            ("WARN", Self::warn()),
            ("LIGHT_BLUE", Style::new().fg(Self::LIGHT_BLUE())),
            ("GREEN", Style::new().fg(Self::GREEN())),
            ("MENU_BG", Style::new().bg(Self::MENU_BG())),
            ("INPUT_BG", Style::new().bg(Self::INPUT_BG())),
            ("SELECTION_BG", Style::new().bg(Self::SELECTION_BG())),
            ("QUOTE_BG", Style::new().bg(Self::QUOTE_BG())),
            ("RULE", Style::new().fg(Self::rule_color())),
            ("SECTION", Self::section()),
            ("MARKER", Self::marker()),
            ("MODE_ACT", Self::mode_chip_act()),
            ("MODE_PLAN", Self::mode_chip_plan()),
            ("TOOL_HEAD", Self::tool_head()),
            ("FIELD_FOCUSED", Self::field_label_focused()),
            ("SELECTION", Self::selection()),
            (
                "CORAL_START",
                Style::new().fg(Color::Rgb(
                    crate::tui::art::CORAL_START.0,
                    crate::tui::art::CORAL_START.1,
                    crate::tui::art::CORAL_START.2,
                )),
            ),
            (
                "CORAL_END",
                Style::new().fg(Color::Rgb(
                    crate::tui::art::CORAL_END.0,
                    crate::tui::art::CORAL_END.1,
                    crate::tui::art::CORAL_END.2,
                )),
            ),
        ];
        for level in EffortLevel::SELECTABLE {
            out.push((level.as_str(), Self::effort(level)));
        }
        out
    }
}
