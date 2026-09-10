use ratatui::style::{Color, Modifier, Style};

/// HSV -> RGB as a const fn (s and v are percentages 0..=100)
const fn hsv(h: u32, s: u32, v: u32) -> Color {
    let hh = h % 360;
    let sv = (s * 255) / 100;
    let vv = (v * 255) / 100;
    let c = vv * sv / 255;
    let k = hh / 60;
    let f = (hh % 60) * 255 / 60;
    let p = vv - c;
    let t = p + (c * f) / 255;
    let q = vv - (c * f) / 255;
    let (r, g, b) = match k {
        0 => (vv, t, p),
        1 => (q, vv, p),
        2 => (p, vv, t),
        3 => (p, q, vv),
        4 => (t, p, vv),
        _ => (vv, p, q),
    };
    Color::Rgb(r as u8, g as u8, b as u8)
}

/// Single fixed palette for the TUI (currently using neutral dark "white" values).
pub struct Theme;

impl Theme {
    #[allow(non_snake_case)]
    pub const fn BG() -> Color {
        hsv(220, 10, 8)
    }
    #[allow(non_snake_case)]
    pub const fn SURFACE() -> Color {
        hsv(220, 10, 12)
    }
    #[allow(non_snake_case)]
    pub const fn USER_SURFACE() -> Color {
        hsv(220, 12, 17)
    }
    #[allow(non_snake_case)]
    pub const fn FG() -> Color {
        hsv(0, 0, 95)
    }
    #[allow(non_snake_case)]
    pub const fn DIM() -> Color {
        hsv(0, 0, 62)
    }
    #[allow(non_snake_case)]
    pub const fn ACCENT() -> Color {
        hsv(0, 0, 100)
    }
    #[allow(non_snake_case)]
    pub const fn ACCENT_SOFT() -> Color {
        hsv(0, 0, 82)
    }
    #[allow(non_snake_case)]
    #[allow(dead_code)]
    pub const fn BORDER() -> Color {
        hsv(0, 0, 84)
    }
    #[allow(non_snake_case)]
    pub const fn BORDER_DIM() -> Color {
        hsv(0, 0, 40)
    }
    #[allow(non_snake_case)]
    pub const fn OK() -> Color {
        hsv(145, 45, 72)
    }
    #[allow(non_snake_case)]
    pub const fn ERR() -> Color {
        hsv(0, 60, 88)
    }
    #[allow(non_snake_case)]
    pub const fn WARN() -> Color {
        hsv(45, 60, 88)
    }

    pub fn base() -> Style {
        Style::new().fg(Self::FG()).bg(Self::BG())
    }
    pub fn dim() -> Style {
        Style::new().fg(Self::DIM()).bg(Self::BG())
    }
    pub fn accent() -> Style {
        Style::new().fg(Self::ACCENT()).bg(Self::BG())
    }
    pub fn accent_bold() -> Style {
        Style::new()
            .fg(Self::ACCENT())
            .bg(Self::BG())
            .add_modifier(Modifier::BOLD)
    }
    #[allow(dead_code)]
    pub fn label_user() -> Style {
        Self::accent_bold()
    }
    #[allow(dead_code)]
    pub fn label_agent() -> Style {
        Style::new()
            .fg(Self::ACCENT_SOFT())
            .bg(Self::BG())
            .add_modifier(Modifier::BOLD)
    }
    #[allow(dead_code)]
    pub fn border_focused() -> Style {
        Style::new().fg(Self::BORDER()).bg(Self::BG())
    }
    #[allow(dead_code)]
    pub fn border_dim() -> Style {
        Style::new().fg(Self::BORDER_DIM()).bg(Self::BG())
    }
    pub fn status_chip() -> Style {
        Style::new()
            .fg(Self::BG())
            .bg(Self::ACCENT_SOFT())
            .add_modifier(Modifier::BOLD)
    }
    pub fn ok() -> Style {
        Style::new().fg(Self::OK()).bg(Self::BG())
    }
    pub fn err() -> Style {
        Style::new().fg(Self::ERR()).bg(Self::BG())
    }
    pub fn warn() -> Style {
        Style::new().fg(Self::WARN()).bg(Self::BG())
    }

    /// Slightly quieter border used only around fenced code blocks.
    pub fn code_border() -> Color {
        const fn mix(a: u8, b: u8) -> u8 {
            ((a as u16 * 3 + b as u16) / 4) as u8
        }
        match (Self::BORDER_DIM(), Self::BG()) {
            (Color::Rgb(r, g, b), Color::Rgb(br, bg, bb)) => {
                Color::Rgb(mix(r, br), mix(g, bg), mix(b, bb))
            }
            (border, _) => border,
        }
    }

    /// table separators / horizontal rules
    pub fn rule_color() -> Color {
        Self::BORDER_DIM()
    }
}
