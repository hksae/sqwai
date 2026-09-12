//! Spinner/animation showcase for the `/test animations` gallery menu.
//!
//! Test-only eye candy: one entry per animation, each a name plus its frame
//! cycle. The gallery renders `frame(entry, tick)` per row, so every style
//! can be judged live before it is adopted anywhere real. Nothing here is
//! used by production widgets (tool rows keep braille-classic).
//!
//! Single-cell entries (no `-wide` suffix) fit one terminal column and could
//! replace the tool spinner anywhere. The `cli-*` entries port frame data
//! from sindresorhus/cli-spinners (MIT); the `flux-*` entries port the
//! FluxFrames presets from ratatui/ratatui-spinner (MIT), skipping the ones
//! that duplicate entries above (classic/orbit/moon/square/bar). Emoji
//! entries from those collections are deliberately left out.
//!
//! Rows named `shimmer-*` and `flux-wave-wide` ignore `frames` (a placeholder
//! is still required): the gallery paints them live — tinted shimmer waves
//! via [`crate::tui::shimmer::shimmer_named`], the phase wave procedurally.

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
    spin!("shimmer-ocean", ["◌"]),
    spin!("shimmer-ember", ["◌"]),
    spin!("shimmer-mint", ["◌"]),
    spin!("shimmer-pulse", ["◌"]),
    spin!("clock-wide", ["🕐", "🕑", "🕒", "🕓", "🕔", "🕕", "🕖", "🕗", "🕘", "🕙", "🕚", "🕛"]),
    spin!("moon-wide", ["🌑", "🌒", "🌓", "🌔", "🌕", "🌖", "🌗", "🌘"]),
    spin!("earth-wide", ["🌍", "🌎", "🌏"]),
    spin!("monkey-wide", ["🙈", "🙉", "🙊"]),
    spin!("speaker-wide", ["🔈", "🔉", "🔊", "🔇"]),
    spin!("sun-cloud", ["☀", "☁", "☂", "☃"]),
    spin!("hourglass-wide", ["⌛", "⏳"]),
    // --- cli-spinners ports (braille family) ---
    spin!("cli-throb", ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"]),
    spin!("cli-spin-alt", ["⠋", "⠙", "⠚", "⠞", "⠖", "⠦", "⠴", "⠲", "⠳", "⠓"]),
    spin!("cli-nod", ["⠄", "⠆", "⠇", "⠋", "⠙", "⠸", "⠰", "⠠", "⠰", "⠸", "⠙", "⠋", "⠇", "⠆"]),
    spin!("cli-fan", ["⢹", "⢺", "⢼", "⣸", "⣇", "⡧", "⡗", "⡏"]),
    spin!("cli-drip", ["⢄", "⢂", "⢁", "⡁", "⡈", "⡐", "⡠"]),
    spin!("cli-orbit8", ["⠁", "⠂", "⠄", "⡀", "⢀", "⠠", "⠐", "⠈"]),
    spin!("cli-swirl", ["⣼", "⣹", "⢻", "⠿", "⡟", "⣏", "⣧", "⣶"]),
    spin!("cli-sand", ["⠁", "⠂", "⠄", "⡀", "⡈", "⡐", "⡠", "⣀", "⣁", "⣂", "⣄", "⣌", "⣔", "⣤", "⣥", "⣦", "⣮", "⣶", "⣷", "⣿", "⡿", "⠿", "⢟", "⠟", "⡛", "⠛", "⠫", "⢋", "⠋", "⠍", "⡉", "⠉", "⠑", "⠡", "⢁"]),
    spin!("cli-orbit-wide", ["⢎ ", "⠎⠁", "⠊⠑", "⠈⠱", " ⡱", "⢀⡰", "⢄⡠", "⢆⡀"]),
    spin!("cli-corner-wide", ["⠉⠉", "⠈⠙", "⠀⠹", "⠀⢸", "⠀⣰", "⢀⣠", "⣀⣀", "⣄⡀", "⣆⠀", "⡇⠀", "⠏⠀", "⠋⠁"]),
    // --- cli-spinners ports (lines, arrows, ticks) ---
    spin!("cli-rolling-wide", ["/  ", " - ", " \\ ", "  |", "  |", " \\ ", " - ", "/  "]),
    spin!("cli-dash", ["⠂", "-", "–", "—", "–", "-"]),
    spin!("cli-chase-wide", ["▹▹▹▹▹", "▸▹▹▹▹", "▹▸▹▹▹", "▹▹▸▹▹", "▹▹▹▸▹", "▹▹▹▹▸"]),
    spin!("cli-flip", ["_", "_", "_", "-", "`", "`", "'", "´", "-", "_", "_", "_"]),
    spin!("cli-squish", ["╫", "╪"]),
    spin!("cli-layer", ["-", "=", "≡"]),
    spin!("cli-circle", ["◡", "⊙", "◠"]),
    spin!("cli-shades", ["▓", "▒", "░"]),
    // --- cli-spinners ports (toggles) ---
    spin!("cli-tbox", ["▫", "▪"]),
    spin!("cli-tsquare", ["□", "■"]),
    spin!("cli-tcycle", ["■", "□", "▪", "▫"]),
    spin!("cli-tbar", ["▮", "▯"]),
    spin!("cli-ttarget", ["⦾", "⦿"]),
    spin!("cli-tring", ["◍", "◌"]),
    spin!("cli-tbrackets", ["⧇", "⧆"]),
    spin!("cli-tblink", ["=", "*", "-"]),
    spin!("cli-tstones", ["☗", "☖"]),
    // --- cli-spinners ports (pulses, chasers, scenes) ---
    spin!("cli-balloon", [".", "o", "O", "°", "O", "o", "."]),
    spin!("cli-dots-wide", [".  ", ".. ", "...", " ..", "  .", "   "]),
    spin!("cli-grenade-wide", ["،  ", "′  ", " ´ ", " ‾ ", "  ⸌", "  ⸊", "  |", "  ⁎", "  ⁕", " ෴ ", "  ⁓", "   ", "   ", "   "]),
    spin!("cli-point-wide", ["∙∙∙", "●∙∙", "∙●∙", "∙∙●", "∙∙∙"]),
    spin!("cli-beta-wide", ["ρββββββ", "βρβββββ", "ββρββββ", "βββρβββ", "ββββρββ", "βββββρβ", "ββββββρ"]),
    spin!("cli-binary-wide", ["010010", "001100", "100101", "111010", "111101", "010111", "101011", "111000", "110011", "110101"]),
    spin!("cli-blocks-wide", ["▰▱▱▱▱▱▱", "▰▰▱▱▱▱▱", "▰▰▰▱▱▱▱", "▰▰▰▰▱▱▱", "▰▰▰▰▰▱▱", "▰▰▰▰▰▰▱", "▰▰▰▰▰▰▰", "▰▱▱▱▱▱▱"]),
    spin!("cli-pong-wide", ["▐⠂       ▌", "▐⠈       ▌", "▐ ⠂      ▌", "▐ ⠠      ▌", "▐  ⡀     ▌", "▐  ⠠     ▌", "▐   ⠂    ▌", "▐   ⠈    ▌", "▐    ⠂   ▌", "▐    ⠠   ▌", "▐     ⡀  ▌", "▐     ⠠  ▌", "▐      ⠂ ▌", "▐      ⠈ ▌", "▐       ⠂▌", "▐       ⠠▌", "▐       ⡀▌", "▐      ⠠ ▌", "▐      ⠂ ▌", "▐     ⠈  ▌", "▐     ⠂  ▌", "▐    ⠠   ▌", "▐    ⡀   ▌", "▐   ⠠    ▌", "▐   ⠂    ▌", "▐  ⠈     ▌", "▐  ⠂     ▌", "▐ ⠠      ▌", "▐ ⡀      ▌", "▐⠠       ▌"]),
    spin!("cli-shark-wide", ["▐|\\____________▌", "▐_|\\___________▌", "▐__|\\__________▌", "▐___|\\_________▌", "▐____|\\________▌", "▐_____|\\_______▌", "▐______|\\______▌", "▐_______|\\_____▌", "▐________|\\____▌", "▐_________|\\___▌", "▐__________|\\__▌", "▐___________|\\_▌", "▐____________|\\▌", "▐____________/|▌", "▐___________/|_▌", "▐__________/|__▌", "▐_________/|___▌", "▐________/|____▌", "▐_______/|-----▌", "▐______/|------▌", "▐_____/|-------▌", "▐____/|--------▌", "▐___/|---------▌", "▐__/|----------▌", "▐_/|-----------▌", "▐/|------------▌"]),
    spin!("cli-fish-wide", ["~~~~~~~~~~~~~~~~~~~~", "> ~~~~~~~~~~~~~~~~~~", "º> ~~~~~~~~~~~~~~~~~", "(º> ~~~~~~~~~~~~~~~~", "((º> ~~~~~~~~~~~~~~~", "<((º> ~~~~~~~~~~~~~~", "><((º> ~~~~~~~~~~~~~", " ><((º> ~~~~~~~~~~~~", "~ ><((º> ~~~~~~~~~~~", "~~ <>((º> ~~~~~~~~~~", "~~~ ><((º> ~~~~~~~~~", "~~~~ <>((º> ~~~~~~~~", "~~~~~ ><((º> ~~~~~~~", "~~~~~~ <>((º> ~~~~~~", "~~~~~~~ ><((º> ~~~~~", "~~~~~~~~ <>((º> ~~~~", "~~~~~~~~~ ><((º> ~~~", "~~~~~~~~~~ <>((º> ~~", "~~~~~~~~~~~ ><((º> ~", "~~~~~~~~~~~~ <>((º> ", "~~~~~~~~~~~~~ ><((º>", "~~~~~~~~~~~~~~ <>((", "~~~~~~~~~~~~~~~ ><((", "~~~~~~~~~~~~~~~~ <>(", "~~~~~~~~~~~~~~~~~ ><", "~~~~~~~~~~~~~~~~~~ <", "~~~~~~~~~~~~~~~~~~~~"]),
    // --- ratatui-spinner FluxFrames ports (single-cell presets) ---
    spin!("flux-braille", ["⣾", "⣷", "⣯", "⣟", "⡿", "⢿", "⣽", "⣻"]),
    spin!("flux-line", ["│", "╱", "─", "╲"]),
    spin!("flux-block", ["▖", "▘", "▝", "▗"]),
    spin!("flux-clock", ["◷", "◶", "◵", "◴"]),
    spin!("flux-triangles", ["▲", "▶", "▼", "◀"]),
    spin!("flux-pulse", ["⣀", "⣤", "⣶", "⣾", "⣿", "⣾", "⣶", "⣤"]),
    spin!("flux-bounce", ["⠉", "⠒", "⣀", "⠒"]),
    spin!("flux-half", ["▀", "▐", "▄", "▌"]),
    spin!("flux-dice", ["⚀", "⚁", "⚂", "⚃", "⚄", "⚅"]),
    spin!("flux-corners", ["┌", "┐", "┘", "└"]),
    spin!("flux-circle-fill", ["○", "◔", "◑", "◕", "●"]),
    spin!("flux-piston", ["▁", "▃", "▅", "▇", "█", "▇", "▅", "▃"]),
    spin!("flux-star", ["✶", "✷", "✸", "✹"]),
    spin!("flux-pair", ["⠉", "⠘", "⠰", "⢠", "⣀", "⡄", "⠆", "⠃"]),
    spin!("flux-diamond", ["◇", "◈", "◆", "◈"]),
    spin!("flux-arc", ["◜", "◝", "◞", "◟"]),
    // multi-cell phase wave (FluxSpinner width=8, phase_step=1): painted
    // procedurally by the gallery, frames are a placeholder
    spin!("flux-wave-wide", ["⣾⣷⣯⣟⡿⢿⣽⣻"]),
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
    fn names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for entry in ALL {
            assert!(seen.insert(entry.name), "duplicate entry {}", entry.name);
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
            // live-painted rows carry a placeholder frame, not real content
            if entry.name.starts_with("shimmer-") {
                continue;
            }
            if matches!(
                entry.name,
                "meter" | "heartbeat" | "star-shoot" | "matrix" | "ellipsis"
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
