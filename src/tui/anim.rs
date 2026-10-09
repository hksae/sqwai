//! Single-character frame sets for `/test anim`: the glyph gallery the live
//! status row picks its marker from.
//!
//! The list is not invented: every set below is a real terminal spinner family
//! (`cli-spinners`, the npm canon, plus two geometric families of our own),
//! reduced to the ones whose frames are a single character. Sets that need two
//! cells (the emoji families) are left out — a row that changes width mid-frame
//! is not a candidate for the status line.
//!
//! Nothing here animates on its own: the gallery re-reads `spinner_tick`, the
//! same 20 FPS counter the tool rows use.

/// One frame strip. `frames` is the whole cycle as a string, so the gallery can
/// show every glyph of a set next to its name while only one of them spins.
#[derive(Debug, Clone, Copy)]
pub struct Anim {
    pub name: &'static str,
    pub frames: &'static str,
}

/// The gallery: braille families first (densest motion), then shapes, then the
/// literal-character ones, then ours.
pub fn sets() -> &'static [Anim] {
    &[
        Anim {
            name: "dots",
            frames: "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏",
        },
        Anim {
            name: "dots2",
            frames: "⣾⣽⣻⢿⡿⣟⣯⣷",
        },
        Anim {
            name: "dots3",
            frames: "⠋⠙⠚⠞⠖⠦⠴⠲⠳⠓",
        },
        Anim {
            name: "dots4",
            frames: "⠄⠆⠇⠋⠙⠸⠰⠠⠰⠸⠙⠋⠇⠆",
        },
        Anim {
            name: "dots5",
            frames: "⠋⠙⠚⠒⠂⠂⠒⠲⠴⠦⠖⠒⠐⠐⠒⠓⠋",
        },
        Anim {
            name: "dots6",
            frames: "⠁⠉⠙⠚⠒⠂⠂⠒⠲⠴⠤⠄⠄⠤⠴⠲⠒⠂⠂⠒⠚⠙⠉⠁",
        },
        Anim {
            name: "dots7",
            frames: "⠈⠉⠋⠓⠒⠐⠐⠒⠖⠦⠤⠠⠠⠤⠦⠖⠒⠐⠐⠒⠓⠋⠉⠈",
        },
        Anim {
            name: "dots8",
            frames: "⠁⠁⠉⠙⠚⠒⠂⠂⠒⠲⠴⠤⠄⠄⠤⠠⠠⠤⠦⠖⠒⠐⠐⠒⠓⠋⠉⠈⠈",
        },
        Anim {
            name: "dots9",
            frames: "⢹⢺⢼⣸⣇⡧⡗⡏",
        },
        Anim {
            name: "dots10",
            frames: "⢄⢂⢁⡁⡈⡐⡠",
        },
        Anim {
            name: "dots11",
            frames: "⠁⠂⠄⡀⢀⠠⠐⠈",
        },
        Anim {
            name: "dots13",
            frames: "⣼⣹⢻⠿⡟⣏⣧⣶",
        },
        Anim {
            name: "dots8Bit",
            frames: "⠀⠁⠂⠃⠄⠅⠆⠇⡀⡁⡂⡃⡄⡅⡆⡇⠈⠉⠊⠋⠌⠍⠎⠏⡈⡉⡊⡋⡌⡍⡎⡏⠐⠑⠒⠓⠔⠕⠖⠗⡐⡑⡒⡓⡔⡕⡖⡗⠘⠙⠚⠛⠜⠝⠞⠟⡘⡙⡚⡛⡜⡝⡞⡟⠠⠡⠢⠣⠤⠥⠦⠧⡠⡡⡢⡣⡤⡥⡦⡧⠨⠩⠪⠫⠬⠭⠮⠯⡨⡩⡪⡫⡬⡭⡮⡯⠰⠱⠲⠳⠴⠵⠶⠷⡰⡱⡲⡳⡴⡵⡶⡷⠸⠹⠺⠻⠼⠽⠾⠿⡸⡹⡺⡻⡼⡽⡾⡿⢀⢁⢂⢃⢄⢅⢆⢇⣀⣁⣂⣃⣄⣅⣆⣇⢈⢉⢊⢋⢌⢍⢎⢏⣈⣉⣊⣋⣌⣍⣎⣏⢐⢑⢒⢓⢔⢕⢖⢗⣐⣑⣒⣓⣔⣕⣖⣗⢘⢙⢚⢛⢜⢝⢞⢟⣘⣙⣚⣛⣜⣝⣞⣟⢠⢡⢢⢣⢤⢥⢦⢧⣠⣡⣢⣣⣤⣥⣦⣧⢨⢩⢪⢫⢬⢭⢮⢯⣨⣩⣪⣫⣬⣭⣮⣯⢰⢱⢲⢳⢴⢵⢶⢷⣰⣱⣲⣳⣴⣵⣶⣷⢸⢹⢺⢻⢼⢽⢾⢿⣸⣹⣺⣻⣼⣽⣾⣿",
        },
        Anim {
            name: "sand",
            frames: "⠁⠂⠄⡀⡈⡐⡠⣀⣁⣂⣄⣌⣔⣤⣥⣦⣮⣶⣷⣿⡿⠿⢟⠟⡛⠛⠫⢋⠋⠍⡉⠉⠑⠡⢁",
        },
        Anim {
            name: "line",
            frames: "-\\|/",
        },
        Anim {
            name: "line2",
            frames: "⠂-–—–-",
        },
        Anim {
            name: "pipe",
            frames: "┤┘┴└├┌┬┐",
        },
        Anim {
            name: "star",
            frames: "✶✸✹✺✹✷",
        },
        Anim {
            name: "star2",
            frames: "+x*",
        },
        Anim {
            name: "flip",
            frames: "___-``'´-___",
        },
        Anim {
            name: "growVertical",
            frames: "▁▃▄▅▆▇▆▅▄▃",
        },
        Anim {
            name: "growHorizontal",
            frames: "▏▎▍▌▋▊▉▊▋▌▍▎",
        },
        Anim {
            name: "balloon",
            frames: " .oO@* ",
        },
        Anim {
            name: "balloon2",
            frames: ".oO°Oo.",
        },
        Anim {
            name: "noise",
            frames: "▓▒░",
        },
        Anim {
            name: "bounce",
            frames: "⠁⠂⠄⠂",
        },
        Anim {
            name: "boxBounce",
            frames: "▖▘▝▗",
        },
        Anim {
            name: "boxBounce2",
            frames: "▌▀▐▄",
        },
        Anim {
            name: "triangle",
            frames: "◢◣◤◥",
        },
        Anim {
            name: "arc",
            frames: "◜◠◝◞◡◟",
        },
        Anim {
            name: "circle",
            frames: "◡⊙◠",
        },
        Anim {
            name: "squareCorners",
            frames: "◰◳◲◱",
        },
        Anim {
            name: "circleQuarters",
            frames: "◴◷◶◵",
        },
        Anim {
            name: "circleHalves",
            frames: "◐◓◑◒",
        },
        Anim {
            name: "squish",
            frames: "╫╪",
        },
        Anim {
            name: "toggle",
            frames: "⊶⊷",
        },
        Anim {
            name: "toggle2",
            frames: "▫▪",
        },
        Anim {
            name: "toggle3",
            frames: "□■",
        },
        Anim {
            name: "toggle4",
            frames: "■□▪▫",
        },
        Anim {
            name: "toggle5",
            frames: "▮▯",
        },
        Anim {
            name: "toggle6",
            frames: "ဝ၀",
        },
        Anim {
            name: "toggle7",
            frames: "⦾⦿",
        },
        Anim {
            name: "toggle8",
            frames: "◍◌",
        },
        Anim {
            name: "toggle9",
            frames: "◉◎",
        },
        Anim {
            name: "toggle11",
            frames: "⧇⧆",
        },
        Anim {
            name: "toggle12",
            frames: "☗☖",
        },
        Anim {
            name: "toggle13",
            frames: "=*-",
        },
        Anim {
            name: "arrow",
            frames: "←↖↑↗→↘↓↙",
        },
        Anim {
            name: "dqpb",
            frames: "dqpb",
        },
        Anim {
            name: "layer",
            frames: "-=≡",
        },
        Anim {
            name: "diamonds",
            frames: "◆◇◈◇",
        },
        Anim {
            name: "blocks9",
            frames: "▁▂▅▆█▇▆▄▃▂",
        },
    ]
}

/// Frame of one set at a given tick.
pub fn frame_at(set: &Anim, tick: usize) -> char {
    let frames: Vec<char> = set.frames.chars().collect();
    frames[tick % frames.len()]
}

/// The live status marker cycles through the `sand` family of the gallery
/// above. The strip is spelled out instead of looked up by name so the render
/// path carries neither an Option nor a panic risk; the test below keeps it from
/// drifting away from the row `/test anim` actually shows.
pub const WORKING_MARK_FRAMES: &str = "⠁⠂⠄⡀⡈⡐⡠⣀⣁⣂⣄⣌⣔⣤⣥⣦⣮⣶⣷⣿⡿⠿⢟⠟⡛⠛⠫⢋⠋⠍⡉⠉⠑⠡⢁";

/// Frame of the live status marker at a given tick.
pub fn working_mark(tick: usize) -> char {
    static FRAMES: std::sync::OnceLock<Vec<char>> = std::sync::OnceLock::new();
    let frames = FRAMES.get_or_init(|| WORKING_MARK_FRAMES.chars().collect());
    frames[tick % frames.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_frame_is_one_display_cell() {
        for set in sets() {
            for c in set.frames.chars() {
                let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                assert_eq!(w, 1, "{} carries {:?} (width {w})", set.name, c);
            }
            assert!(set.frames.chars().count() > 1, "{} needs a cycle", set.name);
        }
    }

    #[test]
    fn working_mark_is_the_sand_row_of_the_gallery() {
        let sand = sets()
            .iter()
            .find(|s| s.name == "sand")
            .expect("sand is in the gallery");
        assert_eq!(WORKING_MARK_FRAMES, sand.frames);
    }

    #[test]
    fn working_mark_cycles_and_stays_in_range() {
        let n = WORKING_MARK_FRAMES.chars().count();
        assert_eq!(working_mark(0), working_mark(n));
        assert_ne!(working_mark(0), working_mark(1));
    }
    #[test]
    fn names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for set in sets() {
            assert!(seen.insert(set.name), "duplicate set {}", set.name);
        }
    }
}
