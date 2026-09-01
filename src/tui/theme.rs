use ratatui::style::{Color, Modifier, Style};
use std::sync::atomic::{AtomicUsize, Ordering};

/// one full color scheme; every role keeps the lightness of the original
/// rose theme, only hues move
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color,
    pub surface: Color,
    pub fg: Color,
    pub dim: Color,
    pub accent: Color,
    pub accent_soft: Color,
    pub border: Color,
    pub border_dim: Color,
    pub ok: Color,
    pub err: Color,
    pub warn: Color,
}

pub struct ThemeDef {
    /// placeholder name until the final naming pass
    pub name: &'static str,
    pub p: Palette,
}

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

/// build a full palette around one base hue, mirroring the original rose
/// saturation/value of every role
const fn pal(h: u32) -> Palette {
    Palette {
        bg: hsv(h, 33, 9),
        surface: hsv(h, 33, 13),
        fg: hsv(h, 5, 93),
        dim: hsv(h, 21, 59),
        accent: hsv(h, 57, 100),
        accent_soft: hsv(h, 44, 84),
        border: hsv(h, 59, 87),
        border_dim: hsv(h, 31, 35),
        ok: hsv(h + 150, 37, 78),
        err: hsv(h + 29, 55, 94),
        warn: hsv(h + 64, 53, 92),
    }
}

pub const THEMES: [ThemeDef; 12] = [
    ThemeDef {
        name: "rose",
        p: pal(335),
    },
    ThemeDef {
        name: "orchid",
        p: pal(305),
    },
    ThemeDef {
        name: "violet",
        p: pal(275),
    },
    ThemeDef {
        name: "indigo",
        p: pal(250),
    },
    ThemeDef {
        name: "cobalt",
        p: pal(220),
    },
    ThemeDef {
        name: "sky",
        p: pal(195),
    },
    ThemeDef {
        name: "aquamarine",
        p: pal(168),
    },
    ThemeDef {
        name: "mint",
        p: pal(145),
    },
    ThemeDef {
        name: "citrus",
        p: pal(105),
    },
    ThemeDef {
        name: "gold",
        p: pal(60),
    },
    ThemeDef {
        name: "amber",
        p: pal(35),
    },
    ThemeDef {
        name: "ember",
        p: pal(10),
    },
];

static CURRENT: AtomicUsize = AtomicUsize::new(0);

fn cur() -> &'static Palette {
    &THEMES[CURRENT.load(Ordering::Relaxed)].p
}

/// select the active theme by index (clamped); returns the effective index
pub fn set_theme(idx: usize) -> usize {
    let i = idx.min(THEMES.len() - 1);
    CURRENT.store(i, Ordering::Relaxed);
    i
}

pub fn theme_index() -> usize {
    CURRENT.load(Ordering::Relaxed)
}

pub struct Theme;

impl Theme {
    #[allow(non_snake_case)]
    pub fn BG() -> Color {
        cur().bg
    }
    #[allow(non_snake_case)]
    pub fn SURFACE() -> Color {
        cur().surface
    }
    #[allow(non_snake_case)]
    pub fn FG() -> Color {
        cur().fg
    }
    #[allow(non_snake_case)]
    pub fn DIM() -> Color {
        cur().dim
    }
    #[allow(non_snake_case)]
    pub fn ACCENT() -> Color {
        cur().accent
    }
    #[allow(non_snake_case)]
    pub fn ACCENT_SOFT() -> Color {
        cur().accent_soft
    }
    #[allow(non_snake_case)]
    pub fn BORDER() -> Color {
        cur().border
    }
    #[allow(non_snake_case)]
    pub fn OK() -> Color {
        cur().ok
    }
    #[allow(non_snake_case)]
    pub fn ERR() -> Color {
        cur().err
    }
    #[allow(non_snake_case)]
    pub fn WARN() -> Color {
        cur().warn
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
    #[allow(dead_code)] // labels removed from UI; kept for popups later
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
    pub fn border_focused() -> Style {
        Style::new().fg(Self::BORDER()).bg(Self::BG())
    }
    #[allow(dead_code)] // reserved: unfocused input border
    pub fn border_dim() -> Style {
        Style::new().fg(Self::DIM()).bg(Self::BG())
    }
    pub fn status_chip() -> Style {
        Style::new()
            .fg(Self::BG())
            .bg(Self::ACCENT())
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

    /// table separators / horizontal rules
    pub fn rule_color() -> Color {
        cur().border_dim
    }
}
