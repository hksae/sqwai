//! Dedicated terminal presenter thread (see `notes/render-thread-plan.md`).
//!
//! The UI thread renders widgets into an owned [`Buffer`] and hands the
//! latest one to the presenter through a latest-wins mailbox. The presenter
//! is the ONLY writer to the terminal: it diffs against its previous buffer,
//! coalesces trailing blanks into EL, wraps the frame in DEC 2026
//! synchronized update, and flushes. While a present blocks on a slow
//! terminal, newer frames overwrite the mailbox slot instead of queueing —
//! stale frames drop structurally, input never blocks on terminal I/O.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{
    Arc, Condvar, Mutex, PoisonError,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossterm::cursor::MoveTo;
use crossterm::style::{
    Attribute as CAttribute, Color as CColor, SetAttribute, SetBackgroundColor,
    SetForegroundColor,
};
use crossterm::terminal::{Clear as ClearCmd, ClearType as CClearType};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui::style::{Color as RColor, Modifier};

/// Minimum interval between presents (~120 FPS max). No EMA is needed:
/// a slow present naturally coalesces the mailbox behind it.
pub const MIN_PRESENT_INTERVAL: Duration = Duration::from_nanos(8_333_334);

/// Minimum trailing blank run (cells) worth replacing with a single EL.
pub(crate) const MIN_EL_RUN: usize = 8;

/// One frame built by the UI thread. (Build time travels separately in
/// the UI-side pending queue keyed by `seq`; the presenter only needs the
/// pixels.)
#[derive(Debug)]
pub struct FrameData {
    pub seq: u64,
    pub area: Rect,
    pub buf: Buffer,
}

/// Per-presented-frame report back to the UI thread (perf log).
#[derive(Debug)]
pub struct FrameReport {
    pub seq: u64,
    pub draw_us: u128,
    pub bytes: u64,
    pub presented_at: Instant,
}

pub(crate) fn map_crossterm_color(c: RColor) -> CColor {
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
fn cell_clearable(cell: &Cell, bg: RColor) -> bool {
    cell.symbol() == " "
        && cell.modifier == Modifier::empty()
        && cell.bg == bg
        && cell.underline_color == RColor::Reset
}

/// Split diff cells into a skip mask plus EL commands. Pure: no I/O.
///
/// A trailing run of clearable blanks that reaches the last terminal column
/// exactly contiguously is replaced by one erase-to-end-of-line. Only cells
/// the diff explicitly wants blank are ever replaced, so EL cannot erase
/// real content; wide glyphs cannot straddle the EL start because the
/// ratatui buffer never stores overlapping glyphs. Runs shorter than
/// [`MIN_EL_RUN`] stay plain spaces (the EL preamble would cost more).
pub(crate) fn coalesce_trailing_blanks(
    cells: &[(u16, u16, Cell)],
    width: u16,
) -> (Vec<bool>, Vec<(u16, u16, RColor)>) {
    let mut skip = vec![false; cells.len()];
    let mut els: Vec<(u16, u16, RColor)> = Vec::new();
    if cells.is_empty() {
        return (skip, els);
    }
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
            if cells[i].0 != expect_x || !cell_clearable(&cells[i].2, bg) {
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
    els.sort_unstable_by_key(|&(x, y, _)| (x, y));
    (skip, els)
}

/// Write tap: counts wire bytes. Sits INSIDE any buffering so `take` after
/// flush equals frame wire volume.
#[derive(Debug)]
pub(crate) struct TapWriter<W: Write> {
    inner: W,
    tap: Arc<AtomicU64>,
}

impl<W: Write> TapWriter<W> {
    pub(crate) fn new(inner: W, tap: Arc<AtomicU64>) -> Self {
        Self { inner, tap }
    }
}

impl<W: Write> Write for TapWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.tap.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Debug)]
struct Slot {
    frame: Option<FrameData>,
    shutdown: bool,
}

#[derive(Debug)]
struct Shared {
    slot: Mutex<Slot>,
    cv: Condvar,
}

/// UI-side mailbox handle: submit overwrites any pending frame (latest wins).
#[derive(Debug, Clone)]
pub struct FrameTx {
    shared: Arc<Shared>,
}

/// Presenter-side mailbox handle: blocking latest-frame wait.
#[derive(Debug, Clone)]
pub struct FrameRx {
    shared: Arc<Shared>,
}

/// Create a linked UI/presenter mailbox pair.
pub fn mailbox() -> (FrameTx, FrameRx) {
    let shared = Arc::new(Shared {
        slot: Mutex::new(Slot {
            frame: None,
            shutdown: false,
        }),
        cv: Condvar::new(),
    });
    (
        FrameTx {
            shared: Arc::clone(&shared),
        },
        FrameRx { shared },
    )
}

impl FrameTx {
    /// Publish a frame, dropping whatever was still pending.
    pub fn submit(&self, frame: FrameData) {
        {
            let mut slot = self.shared.slot.lock().unwrap_or_else(PoisonError::into_inner);
            slot.frame = Some(frame);
        }
        self.shared.cv.notify_one();
    }

    /// Signal shutdown after pending frames drain, and wake the presenter.
    pub fn shutdown(&self) {
        {
            let mut slot = self.shared.slot.lock().unwrap_or_else(PoisonError::into_inner);
            slot.shutdown = true;
        }
        self.shared.cv.notify_all();
    }
}

impl FrameRx {
    /// Block until the latest pending frame or a drained shutdown.
    /// Pending frames submitted while presenting are coalesced by `submit`.
    fn wait_for_frame(&self) -> Option<FrameData> {
        let mut slot = self.shared.slot.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(frame) = slot.frame.take() {
                return Some(frame);
            }
            if slot.shutdown {
                return None;
            }
            slot = self
                .shared
                .cv
                .wait(slot)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Sets `alive` false when the presenter thread exits for any reason
/// (return or panic unwind), so the UI tick can report a dead renderer
/// instead of freezing silently.
struct AliveGuard {
    alive: Arc<AtomicBool>,
}

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}

/// The presenter: sole terminal writer. Owns the backend, the previous
/// buffer for diffing, and the byte tap for the perf log.
pub struct Presenter<W: Write> {
    backend: CrosstermBackend<W>,
    tap: Arc<AtomicU64>,
    prev: Buffer,
    area: Rect,
    alive: Arc<AtomicBool>,
    #[cfg(debug_assertions)]
    owner: Option<std::thread::ThreadId>,
}

impl<W: Write> Presenter<W> {
    pub fn new(backend: CrosstermBackend<W>, tap: Arc<AtomicU64>, alive: Arc<AtomicBool>) -> Self {
        Self {
            backend,
            tap,
            prev: Buffer::empty(Rect::default()),
            area: Rect::default(),
            alive,
            #[cfg(debug_assertions)]
            owner: None,
        }
    }

    /// Diff one frame against the previous buffer and write it.
    /// An empty diff writes nothing at all (not even sync brackets).
    pub fn present(&mut self, frame: FrameData) -> io::Result<FrameReport> {
        // Single-writer invariant: exactly one thread may ever present.
        #[cfg(debug_assertions)]
        {
            let here = std::thread::current().id();
            if let Some(owner) = self.owner {
                debug_assert_eq!(owner, here, "terminal written from more than one thread");
            } else {
                self.owner = Some(here);
            }
        }
        let t0 = Instant::now();
        if frame.area != self.area {
            self.backend.clear()?;
            self.prev = Buffer::empty(frame.area);
            self.area = frame.area;
        }
        let cells: Vec<(u16, u16, Cell)> = self.prev.diff(&frame.buf).into_iter()
            .map(|(x, y, c)| (x, y, c.clone()))
            .collect();
        let bytes = if cells.is_empty() {
            0
        } else {
            let (skip, els) = coalesce_trailing_blanks(&cells, self.area.width);
            // DEC Mode 2026: the terminal buffers the whole frame and
            // presents it in one refresh instead of painting rows piecemeal.
            crossterm::queue!(
                self.backend,
                crossterm::terminal::BeginSynchronizedUpdate
            )?;
            for &(x, y, bg) in &els {
                // EL clears with the *current* background: reset attributes
                // first, then set it explicitly.
                crossterm::queue!(self.backend, MoveTo(x, y))?;
                crossterm::queue!(self.backend, SetAttribute(CAttribute::Reset))?;
                crossterm::queue!(
                    self.backend,
                    SetBackgroundColor(map_crossterm_color(bg))
                )?;
                crossterm::queue!(self.backend, ClearCmd(CClearType::UntilNewLine))?;
            }
            if !els.is_empty() {
                // The stock cell path below assumes reset colors/modifiers
                // on entry (same assumption it makes after its own trailer).
                crossterm::queue!(self.backend, SetForegroundColor(CColor::Reset))?;
                crossterm::queue!(self.backend, SetBackgroundColor(CColor::Reset))?;
                crossterm::queue!(self.backend, SetAttribute(CAttribute::Reset))?;
            }
            let rest = cells
                .iter()
                .enumerate()
                .filter(|(i, _)| !skip[*i])
                .map(|(_, (x, y, c))| (*x, *y, c));
            self.backend.draw(rest)?;
            crossterm::queue!(
                self.backend,
                crossterm::terminal::EndSynchronizedUpdate
            )?;
            Backend::flush(&mut self.backend)?;
            self.tap.swap(0, Ordering::Relaxed)
        };
        self.prev = frame.buf;
        Ok(FrameReport {
            seq: frame.seq,
            draw_us: t0.elapsed().as_micros(),
            bytes,
            presented_at: Instant::now(),
        })
    }
}

/// Run the present loop until drained shutdown, stats-channel disconnect
/// (UI gone), or a terminal error. Slow presents coalesce the mailbox
/// behind them; the loop always presents the latest frame.
pub fn spawn<W: Write + Send + 'static>(
    mut presenter: Presenter<W>,
    rx: FrameRx,
    stats: mpsc::Sender<FrameReport>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let _alive = AliveGuard {
            alive: Arc::clone(&presenter.alive),
        };
        let mut last_present = Instant::now() - MIN_PRESENT_INTERVAL;
        loop {
            let Some(frame) = rx.wait_for_frame() else {
                break;
            };
            // Fixed pacing gate between presents (never inside a frame).
            let wait =
                (last_present + MIN_PRESENT_INTERVAL).saturating_duration_since(Instant::now());
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
            match presenter.present(frame) {
                Ok(report) => {
                    last_present = report.presented_at;
                    if stats.send(report).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    /// Capturing writer for byte-level assertions.
    #[derive(Clone, Default)]
    struct CapWriter {
        out: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for CapWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.out
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl CapWriter {
        fn bytes(&self) -> Vec<u8> {
            self.out
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.bytes()).into_owned()
        }
    }

    fn text_frame(seq: u64, width: u16, height: u16, lines: &[&str]) -> FrameData {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        for (y, line) in lines.iter().enumerate() {
            buf.set_string(0, y as u16, line, Style::default());
        }
        FrameData {
            seq,
            area,
            buf,
        }
    }

    fn test_presenter(
        cap: &CapWriter,
    ) -> (
        Presenter<TapWriter<CapWriter>>,
        Arc<AtomicU64>,
    ) {
        let tap = Arc::new(AtomicU64::new(0));
        let backend = CrosstermBackend::new(TapWriter::new(cap.clone(), Arc::clone(&tap)));
        (
            Presenter::new(
                backend,
                Arc::clone(&tap),
                Arc::new(AtomicBool::new(true)),
            ),
            tap,
        )
    }

    #[test]
    fn latest_wins_and_stale_frames_drop() {
        let cap = CapWriter::default();
        let (tx, rx) = mailbox();
        let (stats_tx, stats_rx) = mpsc::channel();
        // Submit three frames before the presenter runs: only the last
        // may be presented, the first two must drop.
        tx.submit(text_frame(1, 20, 2, &["AAA", "aaa"]));
        tx.submit(text_frame(2, 20, 2, &["BBB", "bbb"]));
        tx.submit(text_frame(3, 20, 2, &["CCC", "ccc"]));
        tx.shutdown();
        let (presenter, _tap) = test_presenter(&cap);
        spawn(presenter, rx, stats_tx).join().expect("presenter");
        let out = cap.text();
        assert!(out.contains("CCC"), "latest frame missing: {out:?}");
        assert!(!out.contains("AAA") && !out.contains("BBB"), "stale frame leaked: {out:?}");
        let reports: Vec<FrameReport> = stats_rx.try_iter().collect();
        assert_eq!(reports.len(), 1, "expected one report: {reports:?}");
        assert_eq!(reports[0].seq, 3);
    }

    #[test]
    fn resize_forces_full_redraw() {
        let cap = CapWriter::default();
        let (mut presenter, _tap) = test_presenter(&cap);
        // Direct calls: the mailbox would coalesce these, hiding the
        // area-change branch. This tests it deterministically.
        presenter
            .present(text_frame(1, 20, 2, &["hello", "world"]))
            .expect("present");
        presenter
            .present(text_frame(2, 30, 3, &["hello", "world", "again"]))
            .expect("present");
        let out = cap.text();
        // Full clear between sizes, then the new content in full.
        assert!(out.contains("\x1b[2J"), "expected full clear: {out:?}");
        assert!(out.contains("again"), "resized content missing: {out:?}");
    }

    #[test]
    fn empty_diff_writes_nothing() {
        let cap = CapWriter::default();
        let (mut presenter, _tap) = test_presenter(&cap);
        let rep1 = presenter
            .present(text_frame(1, 20, 2, &["hi", "yo"]))
            .expect("present");
        assert!(rep1.bytes > 0);
        let len_after_first = cap.bytes().len();
        let rep2 = presenter
            .present(text_frame(2, 20, 2, &["hi", "yo"]))
            .expect("present");
        assert_eq!(rep2.bytes, 0, "empty diff must write nothing");
        assert_eq!(cap.bytes().len(), len_after_first, "stream grew on empty diff");
    }

    #[test]
    fn frame_stream_has_no_cursor_show() {
        let cap = CapWriter::default();
        let (mut presenter, _tap) = test_presenter(&cap);
        presenter
            .present(text_frame(1, 20, 2, &["hi", "yo"]))
            .expect("present");
        let out = cap.text();
        // Only DECTCEM show is forbidden: MoveTo (ESC[H / ESC[r;cH) is
        // legitimate cell positioning and must keep working.
        assert!(!out.contains("\x1b[?25h"), "cursor show leaked: {out:?}");
        assert!(out.contains("\x1b["), "expected escape output: {out:?}");
    }

    #[test]
    fn long_tail_coalesces_to_el() {
        let cells: Vec<(u16, u16, Cell)> = (0..20)
            .map(|x| {
                let mut c = Cell::default();
                if x < 2 {
                    c.set_symbol(if x == 0 { "h" } else { "i" });
                }
                (x, 0, c)
            })
            .collect();
        let (skip, els) = coalesce_trailing_blanks(&cells, 20);
        assert_eq!(els.len(), 1);
        assert_eq!(els[0], (2, 0, RColor::Reset));
        assert_eq!(skip.iter().filter(|&&s| s).count(), 18);
        assert!(!skip[0] && !skip[1]);
    }

    #[test]
    fn short_or_detached_tail_passes_through() {
        // 3 blanks: below the EL threshold.
        let cells: Vec<(u16, u16, Cell)> = (0..20)
            .map(|x| {
                let mut c = Cell::default();
                if x == 0 {
                    c.set_symbol("h");
                }
                if x >= 17 {
                    c.set_symbol(" ");
                } else if x > 0 {
                    c.set_symbol("x");
                }
                (x, 0, c)
            })
            .collect();
        let (skip, els) = coalesce_trailing_blanks(&cells, 20);
        assert!(els.is_empty(), "short run must not use EL: {els:?}");
        assert!(skip.iter().all(|&s| !s));
        // Long run not reaching the last column.
        let mut cells2 = cells;
        cells2[19].2.set_symbol("z");
        let (_, els2) = coalesce_trailing_blanks(&cells2, 20);
        assert!(els2.is_empty(), "detached run must not use EL: {els2:?}");
    }
}
