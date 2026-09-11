//! Spinner/animation showcase for the `/test` gallery menu.
//!
//! Test-only eye candy: one entry per animation, each a name plus its frame
//! cycle. The gallery renders `frame(entry, tick)` per row, so every style
//! can be judged live before it is adopted anywhere real. Nothing here is
//! used by production widgets (tool rows keep braille-classic).

/// One showcase animation: a name and its looping frames.
pub struct Spin {
    pub name: &'static str,
    pub frames: &'static [&'static str],
}

macro_rules! spin {
    ($name:literal, [$($f:literal),+ $(,)?]) => {
        Spin { name: $name, frames: &[$($f),+] }
    };
}

/// Frame cycle for `entry` at `tick` (wraps around).
pub fn frame(entry: &Spin, tick: usize) -> &'static str {
    entry.frames[tick % entry.frames.len()]
}

pub const ALL: &[Spin] = &[
    spin!("braille-classic", ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    spin!("braille-bounce", ["⠁", "⠉", "⠙", "⠚", "⠒", "⠂", "⠒", "⠲", "⠴", "⠦", "⠖", "⠒", "⠐", "⠒", "⠓", "⠋"]),
    spin!("braille-snake", ["⠋", "⠙", "⠚", "⠞", "⠖", "⠦", "⠴", "⠲", "⠳", "⠓"]),
    spin!("braille-arc", ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"]),
    spin!("braille-wave", ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷", "⣾", "⣽"]),
    spin!("line", ["-", "\\", "|", "/"]),
    spin!("line-blocks", ["▌", "▀", "▐", "▄"]),
    spin!("arrow", ["←", "↖", "↑", "↗", "→", "↘", "↓", "↙"]),
    spin!("arrow-fat", ["⬆", "↗", "➡", "↘", "⬇", "↙", "⬅", "↖"]),
    spin!("triangle", ["◢", "◣", "◤", "◥"]),
    spin!("square-corners", ["◰", "◳", "◲", "◱"]),
    spin!("circle-quarters", ["◐", "◓", "◑", "◒"]),
    spin!("arc", ["◜", "◠", "◝", "◞", "◡", "◟"]),
    spin!("bars-vertical", ["▁", "▃", "▄", "▅", "▆", "▇", "█", "▇", "▆", "▅", "▄", "▃"]),
    spin!("bars-grow", ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"]),
    spin!("bars-horizontal", ["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█", "▊", "▋", "▌", "▍", "▎"]),
    spin!("bounce-bar", ["(●     )", "( ●    )", "(  ●   )", "(   ●  )", "(    ● )", "(     ●)", "(    ● )", "(   ●  )", "(  ●   )", "( ●    )"]),
    spin!("meter", ["[      ]", "[=     ]", "[==    ]", "[===   ]", "[====  ]", "[===== ]", "[======]", "[===== ]", "[====  ]", "[===   ]", "[==    ]", "[=     ]"]),
    spin!("star", ["✶", "✸", "✹", "✺", "✹", "✷"]),
    spin!("toggle", ["☐", "☑"]),
    spin!("radio", ["◎", "◉"]),
    spin!("suits", ["♥", "♦", "♣", "♠"]),
    spin!("dqpb", ["d", "q", "p", "b"]),
    spin!("pipe", ["┤", "┘", "┴", "└", "├", "┌", "┬", "┼"]),
    spin!("music", ["♩", "♪", "♫", "♬"]),
    spin!("hamburger", ["☱", "☲", "☴"]),
    spin!("ellipsis", ["   ", ".  ", ".. ", "..."]),
    spin!("heartbeat", ["●○○", "○●○", "○○●", "○●○"]),
    spin!("star-shoot", ["*     ", " *    ", "  *   ", "   *  ", "    * ", "     *"]),
    spin!("matrix", ["ﾊﾐﾋｰｳｼ", "ﾐﾋｰｳｼﾅ", "ﾋｰｳｼﾅﾓ", "ｰｳｼﾅﾓﾆ", "ｳｼﾅﾓﾆｻ", "ｼﾅﾓﾆｻﾜ"]),
    spin!("shimmer-live", ["◌"]),
    spin!("clock-wide", ["🕐", "🕑", "🕒", "🕓", "🕔", "🕕", "🕖", "🕗", "🕘", "🕙", "🕚", "🕛"]),
    spin!("moon-wide", ["🌑", "🌒", "🌓", "🌔", "🌕", "🌖", "🌗", "🌘"]),
    spin!("earth-wide", ["🌍", "🌎", "🌏"]),
    spin!("monkey-wide", ["🙈", "🙉", "🙊"]),
    spin!("speaker-wide", ["🔈", "🔉", "🔊", "🔇"]),
    spin!("sun-cloud", ["☀", "☁", "☂", "☃"]),
    spin!("hourglass-wide", ["⌛", "⏳"]),
];

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn every_entry_loops_frames() {
        assert!(!ALL.is_empty());
        for entry in ALL {
            assert!(!entry.frames.is_empty(), "{} has no frames", entry.name);
            assert_eq!(frame(entry, 0), entry.frames[0]);
            assert_eq!(
                frame(entry, entry.frames.len()),
                entry.frames[0],
                "{} must wrap",
                entry.name
            );
        }
    }

    #[test]
    fn single_cell_entries_stay_narrow() {
        // entries without "-wide" in the name must fit one terminal cell per
        // frame, so they can replace the 1-column braille spinner anywhere
        for entry in ALL {
            if entry.name.ends_with("-wide") || entry.name.ends_with("-bar") {
                continue;
            }
            if matches!(
                entry.name,
                "meter" | "heartbeat" | "star-shoot" | "matrix" | "ellipsis" | "shimmer-live"
            ) {
                continue;
            }
            for f in entry.frames {
                assert_eq!(
                    UnicodeWidthStr::width(*f),
                    1,
                    "{} frame {f:?} is not single-cell",
                    entry.name
                );
            }
        }
    }
}
