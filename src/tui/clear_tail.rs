//! [`ClearTailBackend`]: a `CrosstermBackend` decorator that coalesces trailing
//! blank runs into a single erase-to-end-of-line (`\x1b[K`, EL).
//!
//! Stock `ratatui-0.29` `Backend::draw` prints every changed cell, so a mostly
//! empty 150-column row costs ~100+ `Print(" ")` commands plus cursor moves.
//! This wrapper detects a trailing run of blank, unstyled, same-`bg` cells that
//! reaches the last terminal column and replaces it with
//! `MoveTo + SetAttribute(Reset) + SetBackgroundColor(bg) + EL`, mirroring the
//! `ClearToEnd` optimization from Codex `custom_terminal.rs` (`diff_buffers`).
//!
//! Soundness (why EL never erases real content):
//! - Only cells the stock diff explicitly wants set to blank are replaced; the
//!   run must be exactly contiguous (`x == width - run .. width`) and reach the
//!   last column, so there is no unchanged non-blank tail beyond it.
//! - Run cells must be `" "` with empty modifiers, default underline color and
//!   identical `bg` (the EL background). `fg` is irrelevant for a space.
//! - Wide glyphs cannot straddle the EL start: the ratatui buffer never stores
//!   overlapping glyphs, so any previous-frame glyph half inside the cleared
//!   region belongs to a cell the diff already marks blank.
//!
//! Assumptions (match `sqwai` fullscreen usage):
//! - The buffer covers the whole terminal (alternate screen, `area.x == 0`), so
//!   "last buffer column" == "last terminal column" and EL cannot spill into
//!   foreign content.
//! - After emitting ELs the wrapper restores reset colors/attributes, because
//!   the inner stock `draw` assumes that entry state (same assumption it makes
//!   after its own reset trailer).
//!
//! Runs shorter than [`MIN_EL_RUN`] stay plain spaces: below that the EL
//! preamble costs more than the spaces it replaces.

use std::collections::BTreeMap;
use std::io::{self, Write};

use crossterm::cursor::MoveTo;
use crossterm::style::{
    Attribute as CAttribute, Color as CColor, SetAttribute, SetBackgroundColor,
    SetForegroundColor,
};
use crossterm::terminal::{Clear as ClearCmd, ClearType as CClearType};
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color as RColor, Modifier};

/// Minimum trailing blank run (cells) worth replacing with a single EL.
const MIN_EL_RUN: usize = 8;

/// Shared, byte-counting terminal writer.
///
/// `CrosstermBackend` owns its writer, so per-frame byte totals cannot be
/// read back through it. This handle owns the real writer behind a mutex and
/// can be cloned: one clone feeds the backend, another lets the run loop
/// drain the counter after each frame for the `/debug` perf log.
/// Counts bytes *produced* per frame (pre-kernel); with the 256K `BufWriter`
/// below it a frame rarely fills the buffer mid-frame, so the count matches
/// wire bytes closely enough for throughput diagnosis.
#[derive(Debug)]
pub struct SharedWriter<W: Write> {
    state: std::sync::Arc<std::sync::Mutex<SharedState<W>>>,
}

impl<W: Write> Clone for SharedWriter<W> {
    fn clone(&self) -> Self {
        Self {
            state: std::sync::Arc::clone(&self.state),
        }
    }
}

#[derive(Debug)]
struct SharedState<W: Write> {
    writer: W,
    bytes: u64,
}

/// Concrete writer stack used by the production terminal.
pub type TerminalWriter =
    SharedWriter<std::io::BufWriter<std::io::Stdout>>;

impl<W: Write> SharedWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            state: std::sync::Arc::new(std::sync::Mutex::new(SharedState {
                writer,
                bytes: 0,
            })),
        }
    }

    /// Bytes produced since the last call (resets the counter).
    pub fn take_bytes(&self) -> u64 {
        std::mem::take(
            &mut self
                .state
                .lock()
                .expect("terminal writer mutex")
                .bytes,
        )
    }
}

impl<W: Write> Write for SharedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.state.lock().expect("terminal writer mutex");
        let n = state.writer.write(buf)?;
        state.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.state
            .lock()
            .expect("terminal writer mutex")
            .writer
            .flush()
    }
}

/// `CrosstermBackend` wrapper; see the module docs for the optimization.
#[derive(Debug)]
pub struct ClearTailBackend<W: Write> {
    inner: CrosstermBackend<W>,
    /// Test-only terminal width override (avoids a real `size()` syscall).
    #[cfg(test)]
    test_width: Option<u16>,
}

impl<W: Write> ClearTailBackend<W> {
    /// Wrap `writer` exactly like `CrosstermBackend::new` would.
    pub fn new(writer: W) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
            #[cfg(test)]
            test_width: None,
        }
    }

    fn width(&self) -> u16 {
        #[cfg(test)]
        if let Some(w) = self.test_width {
            return w;
        }
        self.inner.size().map(|s| s.width).unwrap_or(u16::MAX)
    }
}

fn map_color(c: RColor) -> CColor {
    match c {
        RColor::Reset => CColor::Reset,
        RColor::Black => CColor::Black,
        RColor::Red => CColor::DarkRed,
        RColor::Green => CColor::DarkGreen,
        RColor::Yellow => CColor::DarkYellow,
        RColor::Blue => CColor::DarkBlue,
        RColor::Magenta => CColor::DarkMagenta,
        RColor::Cyan => CColor::DarkCyan,
        RColor::Gray => CColor::Grey,
        RColor::DarkGray => CColor::DarkGrey,
        RColor::LightRed => CColor::Red,
        RColor::LightGreen => CColor::Green,
        RColor::LightBlue => CColor::Blue,
        RColor::LightYellow => CColor::Yellow,
        RColor::LightMagenta => CColor::Magenta,
        RColor::LightCyan => CColor::Cyan,
        RColor::White => CColor::White,
        RColor::Indexed(i) => CColor::AnsiValue(i),
        RColor::Rgb(r, g, b) => CColor::Rgb { r, g, b },
    }
}

/// Whether setting this cell to blank is visually identical to clearing it
/// with EL on background `bg`.
fn clearable(cell: &Cell, bg: RColor) -> bool {
    cell.symbol() == " "
        && cell.modifier == Modifier::empty()
        && cell.bg == bg
        && cell.underline_color == RColor::Reset
}

impl<W: Write> Backend for ClearTailBackend<W> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let cells: Vec<(u16, u16, Cell)> =
            content.map(|(x, y, c)| (x, y, c.clone())).collect();
        // Indexes into `cells` replaced by an EL, plus the ELs to emit.
        let mut skip = vec![false; cells.len()];
        let mut els: Vec<(u16, u16, RColor)> = Vec::new();
        if !cells.is_empty() {
            let width = self.width();
            let mut rows: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
            for (i, &(_, y, _)) in cells.iter().enumerate() {
                rows.entry(y).or_default().push(i);
            }
            for (&y, idxs) in &rows {
                let mut ord = idxs.clone();
                ord.sort_by_key(|&i| cells[i].0);
                let &last = ord.last().expect("row index list is never empty");
                // The run must reach the last terminal column ...
                if cells[last].0.checked_add(1) != Some(width) {
                    continue;
                }
                let bg = cells[last].2.bg;
                // ... and be exactly contiguous backwards from it.
                let mut run: usize = 0;
                for &i in ord.iter().rev() {
                    let expect_x = width - 1 - run as u16;
                    if cells[i].0 != expect_x || !clearable(&cells[i].2, bg) {
                        break;
                    }
                    run += 1;
                }
                if run >= MIN_EL_RUN {
                    for &i in &ord[ord.len() - run..] {
                        skip[i] = true;
                    }
                    els.push((width - run as u16, y, bg));
                }
            }
        }
        els.sort_unstable_by_key(|&(x, y, _)| (x, y));
        for &(x, y, bg) in &els {
            // EL clears with the *current* background, so reset attributes
            // first (same order as the stock backend's own sequences).
            crossterm::queue!(self.inner, MoveTo(x, y))?;
            crossterm::queue!(self.inner, SetAttribute(CAttribute::Reset))?;
            crossterm::queue!(self.inner, SetBackgroundColor(map_color(bg)))?;
            crossterm::queue!(self.inner, ClearCmd(CClearType::UntilNewLine))?;
        }
        if !els.is_empty() {
            crossterm::queue!(self.inner, SetForegroundColor(CColor::Reset))?;
            crossterm::queue!(self.inner, SetBackgroundColor(CColor::Reset))?;
            crossterm::queue!(self.inner, SetAttribute(CAttribute::Reset))?;
        }
        let rest = cells
            .iter()
            .enumerate()
            .filter(|(i, _)| !skip[*i])
            .map(|(_, (x, y, c))| (*x, *y, c));
        self.inner.draw(rest)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

/// `Write` passthrough so callers can `queue!` raw sequences (e.g. DEC 2026
/// synchronized-update brackets) around a frame, exactly like with the stock
/// backend.
impl<W: Write> Write for ClearTailBackend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct VecWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("writer mutex").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn text_cell(symbol: &str) -> Cell {
        let mut c = Cell::default();
        c.set_symbol(symbol);
        c
    }

    fn draw_to_string(width: u16, updates: &[(u16, u16, Cell)]) -> String {
        let shared = VecWriter::default();
        let mut backend = ClearTailBackend::new(shared.clone());
        backend.test_width = Some(width);
        let refs = updates.iter().map(|(x, y, c)| (*x, *y, c));
        backend.draw(refs).expect("draw");
        Backend::flush(&mut backend).expect("flush");
        let bytes = shared.0.lock().expect("writer mutex").clone();
        String::from_utf8(bytes).expect("utf8 output")
    }

    fn blank_row(width: u16, y: u16, from_x: u16) -> Vec<(u16, u16, Cell)> {
        (from_x..width).map(|x| (x, y, Cell::default())).collect()
    }

    #[test]
    fn long_trailing_blanks_become_single_el() {
        let width = 20;
        let mut updates = vec![(0, 0, text_cell("h")), (1, 0, text_cell("i"))];
        updates.extend(blank_row(width, 0, 2));
        let out = draw_to_string(width, &updates);
        assert!(out.contains('h') && out.contains('i'), "text kept: {out:?}");
        assert!(out.contains("\x1b[K"), "expected EL, got: {out:?}");
        // 18 spaces must not be emitted literally.
        assert!(
            out.matches(' ').count() < 8,
            "spaces should be coalesced: {out:?}"
        );
    }

    #[test]
    fn short_run_passes_through_as_spaces() {
        let width = 20;
        let mut updates = vec![(0, 0, text_cell("h"))];
        updates.extend(blank_row(width, 0, 17)); // 3 blanks < MIN_EL_RUN
        let out = draw_to_string(width, &updates);
        assert!(!out.contains("\x1b[K"), "no EL expected: {out:?}");
    }

    #[test]
    fn run_not_reaching_last_column_passes_through() {
        let width = 20;
        let mut updates = vec![(0, 0, text_cell("h"))];
        updates.extend(blank_row(width, 0, 1));
        updates.pop(); // drop x=19 ...
        updates.push((19, 0, text_cell("x"))); // ... replaced by content
        let out = draw_to_string(width, &updates);
        assert!(!out.contains("\x1b[K"), "no EL expected: {out:?}");
        assert!(out.contains('x'), "tail content kept: {out:?}");
    }

    #[test]
    fn styled_blanks_pass_through() {
        let width = 20;
        let mut updates = vec![(0, 0, text_cell("h")), (1, 0, text_cell("i"))];
        for x in 2..width {
            let mut c = Cell::default();
            c.modifier = Modifier::BOLD;
            updates.push((x, 0, c));
        }
        let out = draw_to_string(width, &updates);
        assert!(!out.contains("\x1b[K"), "no EL expected: {out:?}");
    }

    #[test]
    fn short_same_bg_tail_after_bg_change_passes_through() {
        let width = 20;
        let mut updates = vec![(0, 0, text_cell("h")), (1, 0, text_cell("i"))];
        for x in 2..14 {
            let mut c = Cell::default();
            c.set_bg(RColor::Red);
            updates.push((x, 0, c));
        }
        updates.extend(blank_row(width, 0, 14)); // Reset run is only 6 < MIN
        let out = draw_to_string(width, &updates);
        assert!(!out.contains("\x1b[K"), "no EL expected: {out:?}");
    }

    #[test]
    fn full_blank_row_with_bg_uses_el() {
        let width = 20;
        let updates: Vec<(u16, u16, Cell)> = (0..width)
            .map(|x| {
                let mut c = Cell::default();
                c.set_bg(RColor::Blue);
                (x, 0, c)
            })
            .collect();
        let out = draw_to_string(width, &updates);
        assert!(out.contains("\x1b[K"), "expected EL, got: {out:?}");
    }

    #[test]
    fn shared_writer_counts_and_drains_bytes() {
        let shared = SharedWriter::new(Vec::<u8>::new());
        {
            let mut probe = shared.clone();
            probe.write_all(b"hello").expect("write");
            probe.flush().expect("flush");
        }
        assert_eq!(shared.take_bytes(), 5);
        assert_eq!(shared.take_bytes(), 0);
    }

    #[test]
    fn map_color_covers_named_and_indexed_colors() {
        assert_eq!(map_color(RColor::Reset), CColor::Reset);
        assert_eq!(map_color(RColor::Red), CColor::DarkRed);
        assert_eq!(map_color(RColor::LightBlue), CColor::Blue);
        assert_eq!(map_color(RColor::Indexed(7)), CColor::AnsiValue(7));
        assert_eq!(
            map_color(RColor::Rgb(1, 2, 3)),
            CColor::Rgb { r: 1, g: 2, b: 3 }
        );
    }
}
