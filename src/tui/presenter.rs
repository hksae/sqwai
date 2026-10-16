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
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::sync::{
    Arc, Condvar, Mutex, PoisonError,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui::style::{Color as RColor, Modifier};
use unicode_width::UnicodeWidthStr;

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
    /// Shared so the UI can ring-reuse buffers: the count on the Arc is the
    /// handoff proof — the UI only refills a slot once the presenter has
    /// dropped it (presented or coalesced away).
    pub buf: Arc<Buffer>,
}

/// Per-presented-frame report back to the UI thread (perf log).
#[derive(Debug)]
pub struct FrameReport {
    pub seq: u64,
    pub draw_us: u128,
    /// blocking terminal write only (excludes diff/queue prep)
    pub flush_us: u128,
    pub bytes: u64,
    pub presented_at: Instant,
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
            let mut slot = self
                .shared
                .slot
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            slot.frame = Some(frame);
        }
        self.shared.cv.notify_one();
    }

    /// Signal shutdown after pending frames drain, and wake the presenter.
    pub fn shutdown(&self) {
        {
            let mut slot = self
                .shared
                .slot
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            slot.shutdown = true;
        }
        self.shared.cv.notify_all();
    }
}

impl FrameRx {
    /// Block until the latest pending frame or a drained shutdown.
    /// Pending frames submitted while presenting are coalesced by `submit`.
    fn wait_for_frame(&self) -> Option<FrameData> {
        let mut slot = self
            .shared
            .slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
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

/// Whether a cell is a plain visual space whose foreground color and
/// weight/slant modifiers (`BOLD`, `DIM`, `ITALIC`) produce zero pixels on
/// screen (Notcurses space-elision rule).
#[inline]
fn is_plain_blank(cell: &Cell) -> bool {
    cell.symbol() == " "
        && cell.underline_color == RColor::Reset
        && !cell.modifier.intersects(
            Modifier::UNDERLINED
                | Modifier::CROSSED_OUT
                | Modifier::REVERSED
                | Modifier::RAPID_BLINK
                | Modifier::SLOW_BLINK,
        )
}

/// Visual equivalence of two cells: identical structs, or two plain blanks
/// sharing the same background color.
#[inline]
fn cells_visually_eq(a: &Cell, b: &Cell) -> bool {
    if a == b {
        return true;
    }
    is_plain_blank(a) && is_plain_blank(b) && a.bg == b.bg
}

#[inline]
fn rows_visually_eq(a: &[Cell], b: &[Cell]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(ca, cb)| cells_visually_eq(ca, cb))
}

#[inline]
fn hash_row(row: &[Cell]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for c in row {
        if is_plain_blank(c) {
            0u8.hash(&mut h);
            c.bg.hash(&mut h);
        } else {
            1u8.hash(&mut h);
            c.symbol().hash(&mut h);
            c.fg.hash(&mut h);
            c.bg.hash(&mut h);
            c.modifier.hash(&mut h);
        }
    }
    h.finish()
}

/// Fast decimal ASCII writer for `u16` (0 formatting allocations).
#[inline]
fn push_u16(out: &mut Vec<u8>, mut n: u16) {
    if n < 10 {
        out.push(b'0' + n as u8);
        return;
    }
    if n < 100 {
        out.push(b'0' + (n / 10) as u8);
        out.push(b'0' + (n % 10) as u8);
        return;
    }
    let mut buf = [0u8; 5];
    let mut i = 5;
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out.extend_from_slice(&buf[i..]);
}

/// Detect a full-width vertical scroll band `[top..bot)` that shifted by
/// `delta` rows (`+delta` = content moved up / `SU`, `-delta` = content moved
/// down / `SD`). Requires at least 6 shifted matching rows, at least 4 of
/// which actually changed from `prev`, so static blank regions never trigger.
fn detect_scroll_region(
    prev: &Buffer,
    next: &Buffer,
    width: usize,
    height: usize,
    row_clean: &[bool],
) -> Option<(usize, usize, isize)> {
    if height < 8 || width < 10 {
        return None;
    }
    let prev_hashes: Vec<u64> = (0..height)
        .map(|y| hash_row(&prev.content[y * width..(y + 1) * width]))
        .collect();
    let next_hashes: Vec<u64> = (0..height)
        .map(|y| hash_row(&next.content[y * width..(y + 1) * width]))
        .collect();

    let mut best: Option<(usize, usize, isize, usize)> = None; // (top, bot, delta, score)

    for abs_d in 1..=8usize {
        if abs_d + 6 > height {
            break;
        }
        // 1) Content moved UP by `abs_d` (terminal Scroll Up `\x1b[dS`):
        //    next_row[y] == prev_row[y + abs_d]
        {
            let limit = height - abs_d;
            let mut y = 0usize;
            while y < limit {
                if next_hashes[y] == prev_hashes[y + abs_d]
                    && rows_visually_eq(
                        &next.content[y * width..(y + 1) * width],
                        &prev.content[(y + abs_d) * width..(y + abs_d + 1) * width],
                    )
                {
                    let run_start = y;
                    let mut dirty_in_run = 0usize;
                    while y < limit
                        && next_hashes[y] == prev_hashes[y + abs_d]
                        && rows_visually_eq(
                            &next.content[y * width..(y + 1) * width],
                            &prev.content[(y + abs_d) * width..(y + abs_d + 1) * width],
                        )
                    {
                        if !row_clean[y] {
                            dirty_in_run += 1;
                        }
                        y += 1;
                    }
                    let run_len = y - run_start;
                    if run_len >= 6 && dirty_in_run >= 4 {
                        let top = run_start;
                        let bot = (y + abs_d).min(height);
                        if best.is_none_or(|(_, _, _, s)| dirty_in_run > s) {
                            best = Some((top, bot, abs_d as isize, dirty_in_run));
                        }
                    }
                } else {
                    y += 1;
                }
            }
        }
        // 2) Content moved DOWN by `abs_d` (terminal Scroll Down `\x1b[dT`):
        //    next_row[y] == prev_row[y - abs_d]
        {
            let mut y = abs_d;
            while y < height {
                if next_hashes[y] == prev_hashes[y - abs_d]
                    && rows_visually_eq(
                        &next.content[y * width..(y + 1) * width],
                        &prev.content[(y - abs_d) * width..(y - abs_d + 1) * width],
                    )
                {
                    let run_start = y;
                    let mut dirty_in_run = 0usize;
                    while y < height
                        && next_hashes[y] == prev_hashes[y - abs_d]
                        && rows_visually_eq(
                            &next.content[y * width..(y + 1) * width],
                            &prev.content[(y - abs_d) * width..(y - abs_d + 1) * width],
                        )
                    {
                        if !row_clean[y] {
                            dirty_in_run += 1;
                        }
                        y += 1;
                    }
                    let run_len = y - run_start;
                    if run_len >= 6 && dirty_in_run >= 4 {
                        let top = run_start - abs_d;
                        let bot = y;
                        if best.is_none_or(|(_, _, _, s)| dirty_in_run > s) {
                            best = Some((top, bot, -(abs_d as isize), dirty_in_run));
                        }
                    }
                } else {
                    y += 1;
                }
            }
        }
    }
    best.map(|(top, bot, delta, _)| (top, bot, delta))
}

/// Virtual view of row `y` on the terminal after an optional `DECSTBM`
/// hardware scroll `(top, bot, delta)` has been issued. Avoids cloning or
/// mutating `self.prev: Arc<Buffer>` so the UI buffer ring stays zero-copy.
#[inline]
fn terminal_prev_row<'a>(
    prev: &'a Buffer,
    blank_row: &'a [Cell],
    width: usize,
    y: usize,
    scroll: Option<(usize, usize, isize)>,
) -> &'a [Cell] {
    if let Some((top, bot, delta)) = scroll
        && y >= top
        && y < bot
    {
        if delta > 0 {
            let d = delta as usize;
            if y + d < bot {
                let src = y + d;
                return &prev.content[src * width..(src + 1) * width];
            }
            return blank_row;
        } else if delta < 0 {
            let d = (-delta) as usize;
            if y >= top + d {
                let src = y - d;
                return &prev.content[src * width..(src + 1) * width];
            }
            return blank_row;
        }
    }
    &prev.content[y * width..(y + 1) * width]
}

/// Tracked SGR and cursor state during a single frame's rasterization pass.
struct RasterState {
    cur_x: u16,
    cur_y: u16,
    fg: RColor,
    bg: RColor,
    modifier: Modifier,
    underline_color: RColor,
    style_dirty: bool,
}

impl RasterState {
    fn new() -> Self {
        Self {
            cur_x: u16::MAX,
            cur_y: u16::MAX,
            fg: RColor::Reset,
            bg: RColor::Reset,
            modifier: Modifier::empty(),
            underline_color: RColor::Reset,
            style_dirty: false,
        }
    }

    #[inline]
    fn reset_sgr(&mut self, out: &mut Vec<u8>) {
        if self.fg != RColor::Reset
            || self.bg != RColor::Reset
            || !self.modifier.is_empty()
            || self.underline_color != RColor::Reset
            || self.style_dirty
        {
            out.extend_from_slice(b"\x1b[0m");
            self.fg = RColor::Reset;
            self.bg = RColor::Reset;
            self.modifier = Modifier::empty();
            self.underline_color = RColor::Reset;
            self.style_dirty = false;
        }
    }

    /// Emit the shortest ANSI cursor movement from `(cur_x, cur_y)` to `(x, y)`.
    #[inline]
    fn move_to(&mut self, out: &mut Vec<u8>, x: u16, y: u16, height: u16) {
        if self.cur_x == x && self.cur_y == y {
            return;
        }
        if self.cur_y == y && self.cur_x != u16::MAX {
            if x == 0 {
                out.push(b'\r');
                self.cur_x = 0;
                return;
            }
            if x > self.cur_x {
                let dx = x - self.cur_x;
                if dx == 1 {
                    out.extend_from_slice(b"\x1b[C");
                } else {
                    // CUF (`\x1b[{dx}C`) vs CHA (`\x1b[{x+1}G`): pick shorter
                    out.extend_from_slice(b"\x1b[");
                    push_u16(out, dx);
                    out.push(b'C');
                }
                self.cur_x = x;
                return;
            }
            if self.cur_x - x == 1 {
                out.push(b'\x08');
                self.cur_x = x;
                return;
            }
            // Same row, moving left by >1 column: CHA (`\x1b[{x+1}G`)
            out.extend_from_slice(b"\x1b[");
            push_u16(out, x + 1);
            out.push(b'G');
            self.cur_x = x;
            return;
        }
        if self.cur_y != u16::MAX
            && self.cur_x != u16::MAX
            && y == self.cur_y + 1
            && x == 0
            && y < height
        {
            out.extend_from_slice(b"\r\n");
            self.cur_x = 0;
            self.cur_y = y;
            return;
        }
        if x == 0 && y == 0 {
            out.extend_from_slice(b"\x1b[H");
        } else if x == 0 {
            out.extend_from_slice(b"\x1b[");
            push_u16(out, y + 1);
            out.push(b'H');
        } else {
            out.extend_from_slice(b"\x1b[");
            push_u16(out, y + 1);
            out.push(b';');
            push_u16(out, x + 1);
            out.push(b'H');
        }
        self.cur_x = x;
        self.cur_y = y;
    }

    #[inline]
    fn emit_fg(out: &mut Vec<u8>, c: RColor) {
        match c {
            RColor::Reset => out.extend_from_slice(b"\x1b[39m"),
            RColor::Black => out.extend_from_slice(b"\x1b[30m"),
            RColor::Red => out.extend_from_slice(b"\x1b[31m"),
            RColor::Green => out.extend_from_slice(b"\x1b[32m"),
            RColor::Yellow => out.extend_from_slice(b"\x1b[33m"),
            RColor::Blue => out.extend_from_slice(b"\x1b[34m"),
            RColor::Magenta => out.extend_from_slice(b"\x1b[35m"),
            RColor::Cyan => out.extend_from_slice(b"\x1b[36m"),
            RColor::Gray => out.extend_from_slice(b"\x1b[37m"),
            RColor::DarkGray => out.extend_from_slice(b"\x1b[90m"),
            RColor::LightRed => out.extend_from_slice(b"\x1b[91m"),
            RColor::LightGreen => out.extend_from_slice(b"\x1b[92m"),
            RColor::LightYellow => out.extend_from_slice(b"\x1b[93m"),
            RColor::LightBlue => out.extend_from_slice(b"\x1b[94m"),
            RColor::LightMagenta => out.extend_from_slice(b"\x1b[95m"),
            RColor::LightCyan => out.extend_from_slice(b"\x1b[96m"),
            RColor::White => out.extend_from_slice(b"\x1b[97m"),
            RColor::Indexed(i) => {
                out.extend_from_slice(b"\x1b[38;5;");
                push_u16(out, i as u16);
                out.push(b'm');
            }
            RColor::Rgb(r, g, b) => {
                out.extend_from_slice(b"\x1b[38;2;");
                push_u16(out, r as u16);
                out.push(b';');
                push_u16(out, g as u16);
                out.push(b';');
                push_u16(out, b as u16);
                out.push(b'm');
            }
        }
    }

    #[inline]
    fn emit_bg(out: &mut Vec<u8>, c: RColor) {
        match c {
            RColor::Reset => out.extend_from_slice(b"\x1b[49m"),
            RColor::Black => out.extend_from_slice(b"\x1b[40m"),
            RColor::Red => out.extend_from_slice(b"\x1b[41m"),
            RColor::Green => out.extend_from_slice(b"\x1b[42m"),
            RColor::Yellow => out.extend_from_slice(b"\x1b[43m"),
            RColor::Blue => out.extend_from_slice(b"\x1b[44m"),
            RColor::Magenta => out.extend_from_slice(b"\x1b[45m"),
            RColor::Cyan => out.extend_from_slice(b"\x1b[46m"),
            RColor::Gray => out.extend_from_slice(b"\x1b[47m"),
            RColor::DarkGray => out.extend_from_slice(b"\x1b[100m"),
            RColor::LightRed => out.extend_from_slice(b"\x1b[101m"),
            RColor::LightGreen => out.extend_from_slice(b"\x1b[102m"),
            RColor::LightYellow => out.extend_from_slice(b"\x1b[103m"),
            RColor::LightBlue => out.extend_from_slice(b"\x1b[104m"),
            RColor::LightMagenta => out.extend_from_slice(b"\x1b[105m"),
            RColor::LightCyan => out.extend_from_slice(b"\x1b[106m"),
            RColor::White => out.extend_from_slice(b"\x1b[107m"),
            RColor::Indexed(i) => {
                out.extend_from_slice(b"\x1b[48;5;");
                push_u16(out, i as u16);
                out.push(b'm');
            }
            RColor::Rgb(r, g, b) => {
                out.extend_from_slice(b"\x1b[48;2;");
                push_u16(out, r as u16);
                out.push(b';');
                push_u16(out, g as u16);
                out.push(b';');
                push_u16(out, b as u16);
                out.push(b'm');
            }
        }
    }

    /// Incremental SGR transition for `cell`. Elides foreground color and
    /// weight/slant changes on plain spaces (`is_plain_blank`).
    #[inline]
    fn apply_cell_style(&mut self, out: &mut Vec<u8>, cell: &Cell) {
        if is_plain_blank(cell) {
            let bad_mods = Modifier::UNDERLINED
                | Modifier::CROSSED_OUT
                | Modifier::REVERSED
                | Modifier::RAPID_BLINK
                | Modifier::SLOW_BLINK;
            if self.modifier.intersects(bad_mods) || self.underline_color != RColor::Reset {
                if cell.bg == RColor::Reset {
                    self.reset_sgr(out);
                    return;
                }
                if self.modifier.contains(Modifier::UNDERLINED)
                    || self.underline_color != RColor::Reset
                {
                    out.extend_from_slice(b"\x1b[24m");
                    self.modifier.remove(Modifier::UNDERLINED);
                    self.underline_color = RColor::Reset;
                }
                if self.modifier.contains(Modifier::REVERSED) {
                    out.extend_from_slice(b"\x1b[27m");
                    self.modifier.remove(Modifier::REVERSED);
                }
                if self.modifier.contains(Modifier::CROSSED_OUT) {
                    out.extend_from_slice(b"\x1b[29m");
                    self.modifier.remove(Modifier::CROSSED_OUT);
                }
                if self
                    .modifier
                    .intersects(Modifier::RAPID_BLINK | Modifier::SLOW_BLINK)
                {
                    out.extend_from_slice(b"\x1b[25m");
                    self.modifier
                        .remove(Modifier::RAPID_BLINK | Modifier::SLOW_BLINK);
                }
            }
            if self.bg != cell.bg {
                Self::emit_bg(out, cell.bg);
                self.bg = cell.bg;
                self.style_dirty = true;
            }
            return;
        }

        // Returning to completely default style from a styled run: `\x1b[0m` (4B)
        if cell.fg == RColor::Reset
            && cell.bg == RColor::Reset
            && cell.modifier.is_empty()
            && cell.underline_color == RColor::Reset
        {
            self.reset_sgr(out);
            return;
        }

        let removed = self.modifier - cell.modifier;
        if !removed.is_empty() {
            if removed.intersects(Modifier::BOLD | Modifier::DIM) {
                out.extend_from_slice(b"\x1b[22m");
                self.modifier.remove(Modifier::BOLD | Modifier::DIM);
            }
            if removed.contains(Modifier::ITALIC) {
                out.extend_from_slice(b"\x1b[23m");
                self.modifier.remove(Modifier::ITALIC);
            }
            if removed.contains(Modifier::UNDERLINED) {
                out.extend_from_slice(b"\x1b[24m");
                self.modifier.remove(Modifier::UNDERLINED);
            }
            if removed.intersects(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK) {
                out.extend_from_slice(b"\x1b[25m");
                self.modifier
                    .remove(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK);
            }
            if removed.contains(Modifier::REVERSED) {
                out.extend_from_slice(b"\x1b[27m");
                self.modifier.remove(Modifier::REVERSED);
            }
            if removed.contains(Modifier::HIDDEN) {
                out.extend_from_slice(b"\x1b[28m");
                self.modifier.remove(Modifier::HIDDEN);
            }
            if removed.contains(Modifier::CROSSED_OUT) {
                out.extend_from_slice(b"\x1b[29m");
                self.modifier.remove(Modifier::CROSSED_OUT);
            }
        }

        let added = cell.modifier - self.modifier;
        if !added.is_empty() {
            if added.contains(Modifier::BOLD) {
                out.extend_from_slice(b"\x1b[1m");
            }
            if added.contains(Modifier::DIM) {
                out.extend_from_slice(b"\x1b[2m");
            }
            if added.contains(Modifier::ITALIC) {
                out.extend_from_slice(b"\x1b[3m");
            }
            if added.contains(Modifier::UNDERLINED) {
                out.extend_from_slice(b"\x1b[4m");
            }
            if added.contains(Modifier::SLOW_BLINK) {
                out.extend_from_slice(b"\x1b[5m");
            }
            if added.contains(Modifier::RAPID_BLINK) {
                out.extend_from_slice(b"\x1b[6m");
            }
            if added.contains(Modifier::REVERSED) {
                out.extend_from_slice(b"\x1b[7m");
            }
            if added.contains(Modifier::HIDDEN) {
                out.extend_from_slice(b"\x1b[8m");
            }
            if added.contains(Modifier::CROSSED_OUT) {
                out.extend_from_slice(b"\x1b[9m");
            }
            self.modifier = cell.modifier;
            self.style_dirty = true;
        }

        if self.fg != cell.fg {
            Self::emit_fg(out, cell.fg);
            self.fg = cell.fg;
            self.style_dirty = true;
        }
        if self.bg != cell.bg {
            Self::emit_bg(out, cell.bg);
            self.bg = cell.bg;
            self.style_dirty = true;
        }
        if self.underline_color != cell.underline_color {
            match cell.underline_color {
                RColor::Reset => out.extend_from_slice(b"\x1b[59m"),
                RColor::Indexed(i) => {
                    out.extend_from_slice(b"\x1b[58;5;");
                    push_u16(out, i as u16);
                    out.push(b'm');
                }
                RColor::Rgb(r, g, b) => {
                    out.extend_from_slice(b"\x1b[58;2;");
                    push_u16(out, r as u16);
                    out.push(b';');
                    push_u16(out, g as u16);
                    out.push(b';');
                    push_u16(out, b as u16);
                    out.push(b'm');
                }
                _ => {}
            }
            self.underline_color = cell.underline_color;
            self.style_dirty = true;
        }
    }
}

/// The presenter: sole terminal writer. Owns the backend, the previous
/// buffer for diffing, a reusable ANSI byte buffer, and the byte tap for the
/// perf log.
pub struct Presenter<W: Write> {
    backend: CrosstermBackend<W>,
    tap: Arc<AtomicU64>,
    /// The previous frame, held by Arc so the UI-side buffer ring can prove
    /// a slot is free again (`Arc::get_mut`) without copying any cells.
    prev: Arc<Buffer>,
    area: Rect,
    alive: Arc<AtomicBool>,
    scratch: Vec<u8>,
    blank_row: Vec<Cell>,
    #[cfg(debug_assertions)]
    owner: Option<std::thread::ThreadId>,
}

impl<W: Write> Presenter<W> {
    pub fn new(backend: CrosstermBackend<W>, tap: Arc<AtomicU64>, alive: Arc<AtomicBool>) -> Self {
        Self {
            backend,
            tap,
            prev: Arc::new(Buffer::empty(Rect::default())),
            area: Rect::default(),
            alive,
            scratch: Vec::with_capacity(64 * 1024),
            blank_row: Vec::new(),
            #[cfg(debug_assertions)]
            owner: None,
        }
    }

    /// Encode one frame diff directly into `self.scratch` without heap
    /// allocations: row-level fast skip, optional `DECSTBM` hardware scroll
    /// region shift, inline trailing `EL` (`\x1b[K`), small-gap bridging,
    /// space SGR elision, and shortest-escape cursor movement.
    fn encode_diff(&mut self, next: &Buffer) {
        self.scratch.clear();
        let width = self.area.width as usize;
        let height = self.area.height as usize;
        if width == 0 || height == 0 {
            return;
        }
        if self.blank_row.len() != width {
            self.blank_row.resize(width, Cell::default());
        }

        let mut row_clean = vec![false; height];
        let mut dirty_rows = 0usize;
        for (y, clean) in row_clean.iter_mut().enumerate() {
            let r0 = y * width;
            let r1 = r0 + width;
            let eq = rows_visually_eq(&self.prev.content[r0..r1], &next.content[r0..r1]);
            *clean = eq;
            if !eq {
                dirty_rows += 1;
            }
        }
        if dirty_rows == 0 {
            return;
        }

        // Open DEC Mode 2026 synchronized update once for the whole frame.
        self.scratch.extend_from_slice(b"\x1b[?2026h");
        let mut rstate = RasterState::new();

        // Hardware scroll region optimization (DECSTBM): if a contiguous band
        // of >= 6 rows shifted vertically, scroll it in terminal hardware and
        // remap `self.prev` rows virtually so only newly revealed rows diff.
        let scroll = if dirty_rows >= 6 {
            detect_scroll_region(&self.prev, next, width, height, &row_clean)
        } else {
            None
        };
        if let Some((top, bot, delta)) = scroll {
            rstate.reset_sgr(&mut self.scratch);
            self.scratch.extend_from_slice(b"\x1b[");
            push_u16(&mut self.scratch, (top + 1) as u16);
            self.scratch.push(b';');
            push_u16(&mut self.scratch, bot as u16);
            self.scratch.push(b'r');
            self.scratch.extend_from_slice(b"\x1b[");
            push_u16(&mut self.scratch, delta.unsigned_abs() as u16);
            self.scratch.push(if delta > 0 { b'S' } else { b'T' });
            self.scratch.extend_from_slice(b"\x1b[r");
            rstate.cur_x = u16::MAX;
            rstate.cur_y = u16::MAX;

            for (y, clean) in row_clean.iter_mut().enumerate().take(bot).skip(top) {
                let prev_r = terminal_prev_row(&self.prev, &self.blank_row, width, y, scroll);
                let next_r = &next.content[y * width..(y + 1) * width];
                *clean = rows_visually_eq(prev_r, next_r);
            }
        }

        let next_content = &next.content;

        for (y, &clean) in row_clean.iter().enumerate() {
            if clean {
                continue;
            }
            let prev_row = terminal_prev_row(&self.prev, &self.blank_row, width, y, scroll);
            let next_row = &next_content[y * width..(y + 1) * width];

            // Inline trailing blank coalescing (EL: `\x1b[K`):
            // Check if a contiguous suffix of changed cells reaching the last
            // column is clearable on a single background `el_bg`.
            let mut el_col: Option<(usize, RColor)> = None;
            if width >= MIN_EL_RUN && !cells_visually_eq(&prev_row[width - 1], &next_row[width - 1])
            {
                let bg = next_row[width - 1].bg;
                if cell_clearable(&next_row[width - 1], bg) {
                    let mut run = 0usize;
                    while run < width {
                        let ix = width - 1 - run;
                        if cells_visually_eq(&prev_row[ix], &next_row[ix])
                            || !cell_clearable(&next_row[ix], bg)
                        {
                            break;
                        }
                        run += 1;
                    }
                    if run >= MIN_EL_RUN {
                        el_col = Some((width - run, bg));
                    }
                }
            }
            let scan_limit = el_col.map_or(width, |(col, _)| col);

            let mut x = 0usize;
            while x < scan_limit {
                let sym = next_row[x].symbol();
                let cw = UnicodeWidthStr::width(sym).max(1);
                let mut changed = !cells_visually_eq(&prev_row[x], &next_row[x]);
                if !changed && cw > 1 && x + 1 < width {
                    changed = !cells_visually_eq(&prev_row[x + 1], &next_row[x + 1]);
                }
                // Also repaint if the previous frame had a 2-wide glyph at x-1
                // that used to cover x, or if x had a 2-wide glyph whose
                // right half at x+1 is now being overwritten.
                if !changed && UnicodeWidthStr::width(prev_row[x].symbol()) > 1 {
                    changed = true;
                }
                if !changed {
                    // Gap bridging: if the cursor is ALREADY sitting right at
                    // `(x, y)` from the cell we just emitted, and another
                    // changed cell lies within `1..=3` single-width columns
                    // ahead with compatible style, bridge the gap instead of
                    // paying 5..9 bytes for a cursor-jump escape.
                    let can_bridge = rstate.cur_y == y as u16
                        && rstate.cur_x == x as u16
                        && cw == 1
                        && x + 1 < scan_limit
                        && (1..=3).any(| lookahead | {
                            let tx = x + lookahead;
                            tx < scan_limit
                                && !cells_visually_eq(&prev_row[tx], &next_row[tx])
                                && (x..tx).all(|k| {
                                    UnicodeWidthStr::width(next_row[k].symbol()) == 1
                                        && UnicodeWidthStr::width(prev_row[k].symbol()) <= 1
                                        && (is_plain_blank(&next_row[k])
                                            && next_row[k].bg == rstate.bg
                                            || (next_row[k].fg == rstate.fg
                                                && next_row[k].bg == rstate.bg
                                                && next_row[k].modifier == rstate.modifier))
                                })
                        });
                    if !can_bridge {
                        x += cw;
                        continue;
                    }
                }

                rstate.move_to(&mut self.scratch, x as u16, y as u16, height as u16);
                rstate.apply_cell_style(&mut self.scratch, &next_row[x]);
                if sym.is_empty() {
                    self.scratch.push(b' ');
                } else {
                    self.scratch.extend_from_slice(sym.as_bytes());
                }
                let next_x = x + cw;
                if next_x >= width {
                    // Right-margin auto-wrap (`xenl`) leaves the terminal
                    // cursor in a pending-wrap state; force an explicit move
                    // on the next cell.
                    rstate.cur_x = u16::MAX;
                } else {
                    rstate.cur_x = next_x as u16;
                }
                x = next_x;
            }

            if let Some((col, bg)) = el_col {
                rstate.move_to(&mut self.scratch, col as u16, y as u16, height as u16);
                // EL clears using the current background attribute; ensure no
                // underline/reverse modifier leaks into the erased tail.
                if !rstate.modifier.is_empty() || rstate.underline_color != RColor::Reset {
                    rstate.reset_sgr(&mut self.scratch);
                }
                if rstate.bg != bg {
                    RasterState::emit_bg(&mut self.scratch, bg);
                    rstate.bg = bg;
                    rstate.style_dirty = true;
                }
                self.scratch.extend_from_slice(b"\x1b[K");
            }
        }

        rstate.reset_sgr(&mut self.scratch);
        self.scratch.extend_from_slice(b"\x1b[?2026l");
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
            self.prev = Arc::new(Buffer::empty(frame.area));
            self.area = frame.area;
        }
        self.encode_diff(&frame.buf);
        // 0 when the diff was empty (nothing written at all)
        let mut flush_us: u128 = 0;
        let bytes = if self.scratch.is_empty() {
            0
        } else {
            self.backend.write_all(&self.scratch)?;
            // Everything above is memory-only (diff + encode into the
            // BufWriter). The single blocking point is this flush: if it
            // dominates draw_us, the terminal/ConPTY is backpressuring us.
            let t_flush = Instant::now();
            Backend::flush(&mut self.backend)?;
            flush_us = t_flush.elapsed().as_micros();
            self.tap.swap(0, Ordering::Relaxed)
        };
        self.prev = frame.buf;
        Ok(FrameReport {
            seq: frame.seq,
            draw_us: t0.elapsed().as_micros(),
            flush_us,
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
        // the presenter is the only terminal writer: it wins the CPU over
        // the agent's batch work, or the UI visibly hitches under load
        crate::tui::priority::raise_this_thread();
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
            buf: Arc::new(buf),
        }
    }

    fn test_presenter(cap: &CapWriter) -> (Presenter<TapWriter<CapWriter>>, Arc<AtomicU64>) {
        let tap = Arc::new(AtomicU64::new(0));
        let backend = CrosstermBackend::new(TapWriter::new(cap.clone(), Arc::clone(&tap)));
        (
            Presenter::new(backend, Arc::clone(&tap), Arc::new(AtomicBool::new(true))),
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
        assert!(
            !out.contains("AAA") && !out.contains("BBB"),
            "stale frame leaked: {out:?}"
        );
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
        assert_eq!(
            cap.bytes().len(),
            len_after_first,
            "stream grew on empty diff"
        );
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

    #[test]
    fn decstbm_scroll_up_and_down_emits_hardware_scroll_and_minimal_diff() {
        let cap = CapWriter::default();
        let (mut presenter, _tap) = test_presenter(&cap);
        let lines_f1: Vec<String> = (0..12)
            .map(|i| {
                if i == 11 {
                    "STATUS BAR FIXED".to_string()
                } else {
                    format!("chat line number {:02} with some text", i)
                }
            })
            .collect();
        let refs_f1: Vec<&str> = lines_f1.iter().map(String::as_str).collect();
        presenter
            .present(text_frame(1, 50, 12, &refs_f1))
            .expect("frame 1");

        // Scroll chat (rows 0..11) up by 2 lines while row 11 (status bar) stays fixed.
        let before_scroll = cap.bytes().len();
        let lines_f2: Vec<String> = (0..12)
            .map(|i| {
                if i == 11 {
                    "STATUS BAR FIXED".to_string()
                } else {
                    format!("chat line number {:02} with some text", i + 2)
                }
            })
            .collect();
        let refs_f2: Vec<&str> = lines_f2.iter().map(String::as_str).collect();
        let rep_up = presenter
            .present(text_frame(2, 50, 12, &refs_f2))
            .expect("frame 2");
        let stream_up = String::from_utf8_lossy(&cap.bytes()[before_scroll..]).into_owned();
        assert!(
            stream_up.contains("\x1b[1;11r\x1b[2S\x1b[r"),
            "expected DECSTBM scroll-up sequence: {stream_up:?}"
        );
        // Unchanged scrolled lines (2..11) must NOT be re-emitted
        assert!(
            !stream_up.contains("chat line number 05"),
            "scrolled middle line was repainted instead of shifted: {stream_up:?}"
        );
        // Newly revealed lines (11, 12) MUST be emitted
        assert!(
            stream_up.contains("chat line number 11") && stream_up.contains("chat line number 12"),
            "newly revealed bottom lines missing: {stream_up:?}"
        );
        assert!(rep_up.bytes < 160, "scroll frame too large: {}B", rep_up.bytes);

        // Scroll back down by 2 lines (back to frame 1): expects `\x1b[1;11r\x1b[2T\x1b[r`
        let before_down = cap.bytes().len();
        presenter
            .present(text_frame(3, 50, 12, &refs_f1))
            .expect("frame 3");
        let stream_down = String::from_utf8_lossy(&cap.bytes()[before_down..]).into_owned();
        assert!(
            stream_down.contains("\x1b[1;11r\x1b[2T\x1b[r"),
            "expected DECSTBM scroll-down sequence: {stream_down:?}"
        );
        assert!(
            !stream_down.contains("chat line number 05"),
            "scrolled middle line was repainted on scroll-down: {stream_down:?}"
        );
        assert!(
            stream_down.contains("chat line number 00")
                && stream_down.contains("chat line number 01"),
            "newly revealed top lines missing: {stream_down:?}"
        );
    }

    #[test]
    fn space_elision_keeps_style_across_plain_spaces() {
        let cap = CapWriter::default();
        let (mut presenter, _tap) = test_presenter(&cap);
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let red_bold = Style::default()
            .fg(RColor::Rgb(220, 50, 50))
            .add_modifier(Modifier::BOLD);
        buf.set_string(0, 0, "fn", red_bold);
        // Middle space at col 2 has default (Reset) fg and no BOLD, but same Reset bg
        buf.set_string(2, 0, " ", Style::default());
        buf.set_string(3, 0, "main", red_bold);
        presenter
            .present(FrameData {
                seq: 1,
                area,
                buf: Arc::new(buf),
            })
            .expect("present");
        let out = cap.text();
        // Should emit the RGB foreground only ONCE because the plain space in
        // between elides its fg/bold reset!
        assert_eq!(
            out.matches("\x1b[38;2;220;50;50m").count(),
            1,
            "space between identically styled words caused redundant SGR: {out:?}"
        );
    }
}
