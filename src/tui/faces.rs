//! UwU face system: one registry for static faces and frame animations,
//! with a w/ω style switch over a single source.
//!
//! Faces are stored once, written with `w`. Rendering with
//! [`FaceStyle::Omega`] swaps every `w` for `ω` (same column width, softer
//! look); [`FaceStyle::W`] keeps the source as-is for terminals where the
//! omega renders as tofu. No per-face duplication: both styles — and every
//! future face — come from the same table.
//!
//! Moods are deliberately NOT split: one face everywhere, animation only
//! over time (the sleep cycle). Meaning rides on the neighbouring text.

/// w/ω render style. The default is [`FaceStyle::Omega`] (softer, on-brand);
/// [`FaceStyle::W`] is the fallback for terminals without the glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaceStyle {
    W,
    Omega,
}

/// One gallery face: the single source art. Gallery rows are numbered,
/// the art is self-labeling.
pub struct FaceDef {
    pub art: &'static str,
}

/// Static gallery faces, each exactly 3 columns wide in both styles.
pub const FACES: &[FaceDef] = &[
    FaceDef { art: "UwU" },
    FaceDef { art: "uwu" },
    FaceDef { art: ">w<" },
    FaceDef { art: "~w~" },
    FaceDef { art: "≧w≦" },
    FaceDef { art: "^w^" },
    FaceDef { art: "^w~" },
    FaceDef { art: "~w^" },
];

/// Sleep animation frames, in order. Width grows with the Z's — that is
/// the animation, so (unlike the static faces) uniformity is not required.
pub const SLEEP_FRAMES: &[&str] = &["UwU", "-w- z", "-w- zZ", "-w- zZz"];

/// Wait-loop frames: the sleep cycle without the waking head — loops
/// forever while a tool waits (bash_output, sleep). Padded to
/// [`SLEEP_WIDTH`] by [`wait_frame`] so the row never jitters.
pub const WAIT_FRAMES: &[&str] = &["-w- z", "-w- zZ", "-w- zZz"];

/// Column width of the footer face slot: every face pads to this.
pub const SLEEP_WIDTH: usize = 7;

/// Ticks per sleep frame at the 50ms spinner cadence (half a second).
pub const SLEEP_TICKS_PER_FRAME: usize = 10;

/// Render one art in the given style.
pub fn render(art: &str, style: FaceStyle) -> String {
    match style {
        FaceStyle::W => art.to_string(),
        FaceStyle::Omega => art
            .chars()
            .map(|c| if c == 'w' { 'ω' } else { c })
            .collect(),
    }
}

/// Art by registry index, falling back to the first face when the
/// stored index points past the table (hand-edited config, reordered
/// registry).
pub fn by_index(idx: usize) -> &'static str {
    FACES.get(idx).map(|f| f.art).unwrap_or(FACES[0].art)
}

/// Style from the w/ω switch.
pub fn style(omega: bool) -> FaceStyle {
    if omega {
        FaceStyle::Omega
    } else {
        FaceStyle::W
    }
}

/// Current sleep-animation frame for a spinner tick.
pub fn sleep_frame(tick: usize, style: FaceStyle) -> String {
    let frame = SLEEP_FRAMES[(tick / SLEEP_TICKS_PER_FRAME) % SLEEP_FRAMES.len()];
    render(frame, style)
}

/// Current wait-loop frame, padded to [`SLEEP_WIDTH`] with trailing
/// spaces so the verb next to it never shifts while the Z's grow.
pub fn wait_frame(tick: usize, style: FaceStyle) -> String {
    let frame = WAIT_FRAMES[(tick / SLEEP_TICKS_PER_FRAME) % WAIT_FRAMES.len()];
    let rendered = render(frame, style);
    let pad = SLEEP_WIDTH.saturating_sub(unicode_width::UnicodeWidthStr::width(rendered.as_str()));
    format!("{rendered}{}", " ".repeat(pad))
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    /// every static face occupies the same columns in both styles, or rows
    /// using them inline would jitter
    #[test]
    fn static_faces_share_one_width_in_both_styles() {
        for face in FACES {
            let w = UnicodeWidthStr::width(face.art);
            assert_eq!(w, 3, "face {:?} must be 3 columns", face.art);
            for style in [FaceStyle::W, FaceStyle::Omega] {
                assert_eq!(
                    UnicodeWidthStr::width(render(face.art, style).as_str()),
                    w,
                    "{:?} in {style:?} must keep its width",
                    face.art
                );
            }
        }
    }

    #[test]
    fn omega_swaps_only_the_w() {
        assert_eq!(render("UwU", FaceStyle::Omega), "UωU");
        assert_eq!(render("-w- zZz", FaceStyle::Omega), "-ω- zZz");
        assert_eq!(render("UwU", FaceStyle::W), "UwU");
    }

    #[test]
    fn wait_loop_has_no_waking_head_and_never_jitters() {
        use unicode_width::UnicodeWidthStr;
        // no UwU frame: pure sleep, looped
        assert_eq!(WAIT_FRAMES, &["-w- z", "-w- zZ", "-w- zZz"]);
        for tick in [0, 10, 20, 30, 40] {
            let frame = wait_frame(tick, FaceStyle::W);
            assert_eq!(
                UnicodeWidthStr::width(frame.as_str()),
                SLEEP_WIDTH,
                "padded slot: {frame:?}"
            );
        }
        assert_eq!(wait_frame(0, FaceStyle::W), "-w- z  ");
        assert_eq!(wait_frame(20, FaceStyle::W), "-w- zZz");
        assert_eq!(wait_frame(30, FaceStyle::W), "-w- z  ");
        assert_eq!(wait_frame(10, FaceStyle::Omega), "-ω- zZ ");
    }

    #[test]
    fn sleep_cycles_in_order() {
        assert_eq!(sleep_frame(0, FaceStyle::W), "UwU");
        assert_eq!(sleep_frame(9, FaceStyle::W), "UwU");
        assert_eq!(sleep_frame(10, FaceStyle::W), "-w- z");
        assert_eq!(sleep_frame(20, FaceStyle::W), "-w- zZ");
        assert_eq!(sleep_frame(30, FaceStyle::W), "-w- zZz");
        assert_eq!(sleep_frame(40, FaceStyle::W), "UwU");
        assert_eq!(sleep_frame(10, FaceStyle::Omega), "-ω- z");
    }
}
