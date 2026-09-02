use ratatui::style::{Color, Modifier, Style};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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

pub const THEMES: [ThemeDef; 20] = [
    // ordered by hue so the /themes list reads as one continuous rainbow:
    // 335 -> 320 -> 305 ... -> 10, then 352 wraps back toward rose
    ThemeDef { name: "rose",       p: pal(335) },
    ThemeDef { name: "plasma",     p: pal(320) },
    ThemeDef { name: "orchid",     p: pal(305) },
    ThemeDef { name: "grape",      p: pal(290) },
    ThemeDef { name: "violet",     p: pal(275) },
    ThemeDef { name: "indigo",     p: pal(250) },
    ThemeDef { name: "denim",      p: pal(235) },
    ThemeDef { name: "cobalt",     p: pal(220) },
    ThemeDef { name: "sky",        p: pal(195) },
    ThemeDef { name: "lagoon",     p: pal(182) },
    ThemeDef { name: "aquamarine", p: pal(168) },
    ThemeDef { name: "slime",      p: pal(156) },
    ThemeDef { name: "mint",       p: pal(145) },
    ThemeDef { name: "lime",       p: pal(125) },
    ThemeDef { name: "citrus",     p: pal(105) },
    ThemeDef { name: "gold",       p: pal(60) },
    ThemeDef { name: "amber",      p: pal(35) },
    ThemeDef { name: "peach",      p: pal(22) },
    ThemeDef { name: "ember",      p: pal(10) },
    ThemeDef { name: "bubblegum",  p: pal(352) },
];

/// kinds of animated (time-driven) palettes.
#[derive(Clone, Copy)]
pub enum AnimKind {
    /// slow orange<->yellow ping-pong
    Lava,
    /// pink (bubblegum 352) <-> aquamarine (168) ping-pong through purple/blue
    Gum,
}

/// an animated theme: a name, the static text hue, and the animation kind
#[derive(Clone, Copy)]
pub struct AnimThemeDef {
    pub name: &'static str,
    pub base_hue: u32,
    pub kind: AnimKind,
}

/// build a palette for an animated theme at a given animation tick.
/// text roles (fg/dim) stay static at `base` so the conversation stays
/// readable; every decorative role is driven by the kind + tick.
fn anim_palette(kind: AnimKind, base: u32, tick: u64) -> Palette {
    let fg = hsv(base, 5, 93);
    let dim = hsv(base, 21, 59);
    // each arm yields the four decorative hue channels (accent, border, bg,
    // surface); the final Palette is built once below so the s/v layering is
    // identical for every animated theme.
    let (accent_h, border_h, bg_h, surf_h) = match kind {
        AnimKind::Lava => {
            // slow, smooth ping-pong: hue eases orange -> yellow -> back to
            // orange (triangle wave) instead of snapping to orange at the top
            // of the range. 1 hue/step, step every 3 frames (~6.7 fps) ->
            // ~4.5s each way, ~9s full cycle.
            let st = (tick / 3) as u32;
            let half = st % 60; // period 60 steps
            let f = if half <= 30 { half } else { 60 - half }; // 0..30..0
            (10 + f, 20 + f, 12 + f, 18 + f)
        }
        AnimKind::Gum => {
            // pink (bubblegum 352) <-> aquamarine (168) ping-pong, taking the
            // SHORT WAY DOWN through purple/blue (352 -> 300 -> 250 -> 200 ->
            // 168) so green/yellow/red never appear between the two endpoints.
            // 1 hue/step, step every 2 frames (~10fps stepping) -> ~18s full
            // cycle, smooth. all decorative roles share the hue so the UI
            // shifts as one clean pink<->blue gradient.
            let st = (tick / 2) as u32;
            let period = 92u32; // steps per half-swing (hue span 184 / 2)
            let half = st % (period * 2);
            let tri = if half <= period { half } else { period * 2 - half }; // 0..period..0
            let h = 352 - tri * 2; // 352..168 (no wraparound, stays >= 0)
            (h, h, h, h)
        }
    };
    Palette {
        bg: hsv(bg_h, 33, 9),
        surface: hsv(surf_h, 33, 13),
        fg,
        dim,
        accent: hsv(accent_h, 57, 100),
        accent_soft: hsv(accent_h, 44, 84),
        border: hsv(border_h, 59, 87),
        border_dim: hsv(border_h, 31, 35),
        ok: hsv((base + 150) % 360, 37, 78),
        err: hsv((base + 29) % 360, 55, 94),
        warn: hsv((base + 64) % 360, 53, 92),
    }
}

pub const ANIMATED_THEMES: [AnimThemeDef; 2] = [
    AnimThemeDef { name: "lava",  base_hue: 18,  kind: AnimKind::Lava },
    AnimThemeDef { name: "taffy", base_hue: 352, kind: AnimKind::Gum },
];

static CURRENT: AtomicUsize = AtomicUsize::new(0);
/// index into ANIMATED_THEMES when an animated theme is active, else usize::MAX
static ACTIVE_ANIM: AtomicUsize = AtomicUsize::new(usize::MAX);
/// current animation frame counter, bumped once per tick by the TUI loop
static ANIM_TICK: AtomicU64 = AtomicU64::new(0);

/// feed the latest animation frame to the theme engine (call once per redraw)
pub fn set_anim_tick(t: u64) {
    ANIM_TICK.store(t, Ordering::Relaxed);
}

/// activate an animated theme; switches the live palette to time-driven colors
pub fn set_anim_theme(idx: usize) -> usize {
    let i = idx.min(ANIMATED_THEMES.len() - 1);
    ACTIVE_ANIM.store(i, Ordering::Relaxed);
    i
}

/// deactivate any animated theme, falling back to the static palette
pub fn set_anim_theme_off() {
    ACTIVE_ANIM.store(usize::MAX, Ordering::Relaxed);
}

/// live palette for an animated theme at an arbitrary tick (for menu previews)
pub fn anim_palette_at(idx: usize, tick: u64) -> Palette {
    let i = idx.min(ANIMATED_THEMES.len() - 1);
    let t = ANIMATED_THEMES[i];
    anim_palette(t.kind, t.base_hue, tick)
}

/// which animated theme is active, if any
pub fn anim_theme_index() -> Option<usize> {
    let a = ACTIVE_ANIM.load(Ordering::Relaxed);
    if a == usize::MAX {
        None
    } else {
        Some(a)
    }
}

/// the latest animation frame counter (for live menu previews)
pub fn anim_tick() -> u64 {
    ANIM_TICK.load(Ordering::Relaxed)
}

fn cur() -> Palette {
    let a = ACTIVE_ANIM.load(Ordering::Relaxed);
    if a != usize::MAX {
        let t = ANIMATED_THEMES[a];
        return anim_palette(t.kind, t.base_hue, ANIM_TICK.load(Ordering::Relaxed));
    }
    THEMES[CURRENT.load(Ordering::Relaxed)].p
}

/// select the active theme by index (clamped); returns the effective index
pub fn set_theme(idx: usize) -> usize {
    let i = idx.min(THEMES.len() - 1);
    CURRENT.store(i, Ordering::Relaxed);
    ACTIVE_ANIM.store(usize::MAX, Ordering::Relaxed);
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

    /// Slightly quieter border used only around fenced code blocks.
    pub fn code_border() -> Color {
        fn mix(a: u8, b: u8) -> u8 {
            ((a as u16 * 3 + b as u16) / 4) as u8
        }
        match (cur().border_dim, cur().bg) {
            (Color::Rgb(r, g, b), Color::Rgb(br, bg, bb)) => {
                Color::Rgb(mix(r, br), mix(g, bg), mix(b, bb))
            }
            (border, _) => border,
        }
    }

    /// table separators / horizontal rules
    pub fn rule_color() -> Color {
        cur().border_dim
    }
}
