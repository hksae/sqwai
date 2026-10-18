#![allow(unused_imports)]
use super::menus::POPUP_MAX_ROWS;
use super::*;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget};
use std::hash::{Hash, Hasher};
use tui_textarea::TextArea;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agent::loop_task::AgentEvent;
use crate::config::{Config, EffortLevel, ModelConfig, WireFormat};
use crate::providers::{self, ChatRequest, Message as PMessage, Role, SharedProvider};
use crate::session::Session;
use crate::tui::markdown::{Highlighter, render, wrap_tagged};
use crate::tui::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SegMeta {
    pub id: u64,
    pub rev: u64,
}

/// One wrapped-cache entry, keyed by segment id (not position): appends and
/// stream updates never invalidate other segments' rows.
pub(super) struct SegCacheEntry {
    pub rev: u64,
    pub width: u16,
    pub key: usize,
    pub rows: Vec<(Line<'static>, Option<usize>)>,
}

/// Chunk identity of one assembled transcript. Segments are pinned by stable
/// id; structural rows (blank spacers, group footers) by ordinal among the
/// structural pushes of one assembly pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AsmTag {
    Seg(u64),
    Struct(u64),
}

/// Assembled rows of one view (main chat or one subagent), parked whole on
/// view switch so coming back never reassembles: `tags`/`lens` describe the
/// chunk map of `lines`/`rowseg`, `fp` is the fingerprint it was built from.
#[derive(Default)]
pub(super) struct StoredView {
    pub lines: Vec<Line<'static>>,
    pub rowseg: Vec<Option<usize>>,
    pub tags: Vec<AsmTag>,
    pub lens: Vec<usize>,
    pub fp: u64,
}

/// One assembly chunk: its tag plus wrapped rows. A `fresh` chunk carries
/// rows built this pass; a reused chunk carries an empty placeholder — the
/// merge step takes its rows from the live buffers instead of cloning them.
pub(super) type RowChunk = (AsmTag, Vec<(Line<'static>, Option<usize>)>);

/// How the last rebuild merged chunks into the live buffers. Recorded for
/// the `/debug` perf log: Skip means the fingerprint gate skipped the
/// rebuild entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum MergeKind {
    #[default]
    Skip,
    Splice,
    Append,
    Concat,
}

impl MergeKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            MergeKind::Skip => "skip",
            MergeKind::Splice => "splice",
            MergeKind::Append => "append",
            MergeKind::Concat => "concat",
        }
    }
}

/// Semantic click target resolved at mouse-down against the shown frame.
/// Re-validated at mouse-up by segment id, so a layout shift between press
/// and release cannot fire a stale row number at the wrong control.
/// Not `Copy`: the Link arm pins an owned URL at press time.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ClickTarget {
    Ask {
        seg: u64,
        row: AskRow,
    },
    Toggle {
        seg: u64,
        screen: u16,
    },
    /// Fenced-block index resolved at press: the fire step only indexes the
    /// segment's source blocks, so a layout shift in between cannot retarget
    /// the copy to a row that moved under the old screen line.
    Code {
        seg: u64,
        block: usize,
    },
    /// Chat link URL resolved at press (see `link_at_row`): firing opens it
    /// in the system browser. The URL is pinned, never re-resolved, so a
    /// streaming shift between press and release cannot retarget the open.
    Link {
        url: String,
    },
}

/// Preview of a tool's shown body, computed once when the output lands —
/// never re-cloned and re-split on every rebuild.
pub(super) fn tool_preview(diff: Option<&str>, output: &str) -> (Vec<String>, usize) {
    const MAX_ROWS: usize = 40;
    let body = diff.unwrap_or(output);
    let mut total = 0usize;
    let mut lines = Vec::new();
    for line in body.lines() {
        if lines.len() < MAX_ROWS {
            lines.push(line.to_string());
        }
        total += 1;
    }
    (lines, total)
}

/// State dot for tool rows: the house green and red, with an ANSI fallback for
/// terminals without truecolor. No bold on either branch — the shape of the
/// marker says the verdict, the color only echoes it.
pub(super) fn tool_dot_style(done_ok: bool) -> Style {
    if crate::tui::shimmer::has_truecolor() {
        if done_ok { Theme::ok() } else { Theme::err() }
    } else if done_ok {
        Style::new().fg(Color::Green)
    } else {
        Style::new().fg(Color::Red)
    }
}

/// Fenced code blocks in assistant source text, in order.
pub(super) fn code_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut in_code = false;
    let mut current = String::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if in_code {
                blocks.push(current.trim_end_matches('\n').to_string());
                current.clear();
            }
            in_code = !in_code;
        } else if in_code {
            current.push_str(line);
            current.push('\n');
        }
    }
    blocks
}

/// Re-assert what `init_presenter` asked the terminal for. On Windows the
/// opener's `cmd.exe` attaches to *our* console and rewrites its input mode,
/// clearing `ENABLE_MOUSE_INPUT` without putting it back on the way out — so
/// without this the first link click silently kills the wheel and every click
/// for the rest of the session, while the keyboard keeps working and the loss
/// looks like a frozen view. Idempotent, so it is safe on every platform.
#[cfg(not(test))]
fn rearm_input_capture() {
    use std::io::Write as _;
    let _ = crossterm::terminal::enable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    let _ = std::io::stdout().write_all(b"\x1b[?1003h");
    let _ = std::io::stdout().flush();
}

/// Open a clicked chat link in the system browser, detached (never blocks
/// the frame). The hit-test only resolves `http(s)` targets, so nothing
/// else can arrive here from a click.
fn open_browser_url(url: &str) -> std::io::Result<()> {
    #[cfg(test)]
    {
        // clicks in tests must never escape to a real browser: resolution
        // is covered by unit tests, firing only needs the graceful path
        let _ = url;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no browser in tests",
        ))
    }
    #[cfg(not(test))]
    {
        use std::process::Stdio;
        // The child gets no handle on our terminal: an error line from `cmd`
        // or `xdg-open` would otherwise land inside a frame and skew the diff.
        #[cfg(target_os = "windows")]
        let spawned = std::process::Command::new("cmd")
            // `start` takes the first quoted arg as the window title: the empty
            // title keeps the URL in the command slot, quoted whole by spawn
            .args(["/C", "start", "", url])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        #[cfg(target_os = "macos")]
        let spawned = std::process::Command::new("open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        #[cfg(all(not(test), not(any(target_os = "windows", target_os = "macos"))))]
        let spawned = std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let result = spawned.map(|_| ());
        rearm_input_capture();
        result
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // chat rows are short-lived render state
pub(super) enum Segment {
    User(String),
    Assistant {
        text: String,
        live: bool,
    },
    /// Model commentary/thoughts emitted before or between tool calls, folded into activity
    Commentary(String),
    /// compact subagent row; click to reveal its latest output
    Subagent {
        id: u64,
        task: String,
        status: String,
        output: String,
        expanded: bool,
    },
    /// one reasoning block; the model may emit several across a turn
    Thinking {
        text: String,
        expanded: bool,
        /// monotonic start of this reasoning block
        started: Option<std::time::Instant>,
        /// frozen elapsed time once the block closes
        duration_ms: u64,
        live: bool,
    },
    /// one tool call: spinner while running, result line when finished,
    /// full output or diff on click
    Tool {
        name: String,
        args: String,
        /// model-side call id pinning notices to this exact row (parallel
        /// same-name calls); `None` for rows predating it (subagent chats)
        call_id: Option<String>,
        /// `None` while the tool is still running
        ok: Option<bool>,
        output: String,
        diff: Option<String>,
        /// first rows of the shown body (diff preferred), computed once when
        /// the output lands; rendering truncates per width from these
        preview: Vec<String>,
        /// total source lines of the shown body, for the "… N more" row
        preview_total: usize,
        expanded: bool,
        /// finish-wave start for the one-shot green/red sweep; None settles
        /// to the static ✓/✗ row. Set when the ToolNotice lands.
        flash: Option<std::time::Instant>,
        /// the live `ask_user` questionnaire, and nothing else. While it is
        /// `Some` the row is a tool call waiting on the user: its head is the
        /// ordinary tool head, its body is the question. The answer clears the
        /// slot, which is why an answered ask and a reloaded one render
        /// through exactly the same code.
        ask: Option<Box<AskState>>,
    },
    Status {
        text: String,
        kind: StatusKind,
        /// error rows arrive collapsed (a provider dump must not flood the
        /// chat) and unfold on click; other kinds always show fully
        expanded: bool,
        /// transient rows (retry notices) are retracted when the turn they
        /// belong to completes successfully; terminal failures stay
        transient: bool,
    },
}

/// A finished (or still-streaming) agent turn's working content — the
/// contiguous run of `Thinking`/`Tool` segments that precedes the turn's final
/// answer. Data only (session summaries re-attach through it); the flat
/// transcript never folds rows behind it.
///
/// `seg_start..seg_end` indexes into `App.segments` and covers the run up to
/// (but excluding) the final `Assistant` answer of that turn.
#[derive(Clone)]
pub(super) struct ActivityGroup {
    pub seg_start: usize,
    pub seg_end: usize,
    pub calls: usize,
    pub thinking: usize,
    pub duration_ms: u64,
    pub errors: usize,
    pub rejected: usize,
    /// Index of the user message that started this group's turn. Saved into
    /// `ActivitySummary::user_index` so a restore can re-attach summaries to
    /// the right group even after stopped/failed turns.
    pub turn_user: Option<usize>,
}

/// The questionnaire inside a live `ask_user` tool row: the answers the user
/// is editing, not a rendered record. Dropped the moment the call is answered
/// or abandoned, so no frozen copy of it has to be rebuilt on session reload.
#[derive(Debug, Clone)]
pub(super) struct AskState {
    /// loop-local id the answer travels back on (`ControlMsg::AskAnswer`)
    pub id: u64,
    pub questions: Vec<crate::agent::loop_task::AskQuestion>,
    /// per-question picked flags, edited in place
    pub picked: Vec<Vec<bool>>,
    /// per-question free text
    pub custom: Vec<String>,
    /// which question the keyboard is on
    pub focus: usize,
}

impl AskState {
    pub(super) fn is_answered(&self, q: usize) -> bool {
        let has_pick = self
            .picked
            .get(q)
            .is_some_and(|v| v.iter().any(|p| *p));
        let has_custom = self
            .custom
            .get(q)
            .is_some_and(|s| !s.trim().is_empty());
        has_pick || has_custom
    }

    pub(super) fn can_reach(&self, q: usize) -> bool {
        q < self.questions.len() && (0..q).all(|i| self.is_answered(i))
    }
}

/// One interactive row inside a live `ask_user` row, used for mouse
/// hover/click mapping. Questions and separators are not targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum AskRow {
    Option { q: usize, opt: usize },
    Custom { q: usize },
    Confirm,
}

/// A tool row whose call is an unanswered `ask_user`: the one row in the chat
/// that carries an interactive body.
fn tool_has_ask(seg: &Segment) -> bool {
    matches!(seg, Segment::Tool { ask: Some(_), .. })
}

pub(super) fn edit_change_counts(diff: &str) -> (usize, usize) {
    diff.lines().fold((0, 0), |(added, removed), line| {
        if line.starts_with("+++") || line.starts_with("---") {
            (added, removed)
        } else if line.starts_with('+') {
            (added + 1, removed)
        } else if line.starts_with('-') {
            (added, removed + 1)
        } else {
            (added, removed)
        }
    })
}

/// Full-width user strip. The quiet surface is the primary distinction; `›`
/// keeps it legible in terminals that flatten colors and in light themes.
/// Compact like the composer: no blank padding rows, so a one-line message is
/// one row; a multi-line message keeps all of its lines.
pub(super) fn user_box(text: &str, w: u16, hl: &Highlighter) -> Vec<Line<'static>> {
    let inner_w = w.saturating_sub(2).max(1);
    let inner = wrap_tagged(
        render(text, inner_w, hl)
            .into_iter()
            .map(|line| (line, None))
            .collect(),
        inner_w,
    )
    .0;
    let surface = Style::new().fg(Theme::FG()).bg(Theme::INPUT_BG());
    // Codex-style marker: bold dim `›` on the band, plain text after it.
    let marker = surface.add_modifier(Modifier::BOLD | Modifier::DIM);
    inner
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let text = line_text(&line);
            let used = UnicodeWidthStr::width(text.as_str());
            let pad = " ".repeat(usize::from(inner_w).saturating_sub(used));
            let prefix = if index == 0 { "› " } else { "  " };
            Line::from(vec![
                Span::styled(prefix.to_string(), marker),
                Span::styled(text, surface),
                Span::styled(pad, surface),
            ])
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct CellPos {
    pub(super) row: usize, // absolute row in cache_lines
    pub(super) col: usize,
}

#[derive(Clone, Copy)]
pub(super) struct Selection {
    pub(super) a: CellPos,
    pub(super) b: CellPos,
}

impl Selection {
    /// Endpoints in document order. For a multi-line selection dragging
    /// bottom-up the start keeps its own column and the end keeps its own —
    /// columns must never be sorted independently of their rows.
    pub(super) fn ordered(&self) -> (CellPos, CellPos) {
        if (self.a.row, self.a.col) <= (self.b.row, self.b.col) {
            (self.a, self.b)
        } else {
            (self.b, self.a)
        }
    }
}

/// Strip UI chrome prefixes/suffixes from a copied row. Only exact known
/// decorations are removed ("  │ " tool rail, "│ " code rail, "› " user
/// marker, one trailing " │" frame cap); every other leading space — real
/// code indent included — survives, unlike a blind trim of lookalike chars.
pub(super) fn strip_row_chrome(line: &str) -> String {
    let mut s = line;
    for prefix in ["  │ ", "│ ", "› "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest;
            break;
        }
    }
    if let Some(rest) = s.strip_suffix(" │") {
        rest.trim_end().to_string()
    } else {
        s.trim_end().to_string()
    }
}

impl App {
    // ---------- mouse ----------

    /// screen row -> absolute row in cache_lines
    pub(super) fn abs_row(&self, screen_row: u16) -> usize {
        self.chat_top(self.last_chat.height.max(1))
            + (screen_row.saturating_sub(self.last_chat.y)) as usize
    }

    /// Maps a screen cell column to a character index within `self.cache_lines[abs_row]`.
    pub(super) fn screen_col_to_char(&self, abs_row: usize, screen_col: u16) -> usize {
        let chat_x = if self.last_chat.width > 0 {
            self.last_chat.x
        } else {
            1
        };
        let target_col = screen_col.saturating_sub(chat_x) as usize;
        let Some(line) = self.cache_lines.get(abs_row) else {
            return target_col;
        };
        let mut cur_col = 0usize;
        let mut char_idx = 0usize;
        for span in &line.spans {
            for ch in span.content.chars() {
                let w = UnicodeWidthChar::width(ch).unwrap_or(0);
                if cur_col + w > target_col {
                    return char_idx;
                }
                cur_col += w;
                char_idx += 1;
            }
        }
        char_idx
    }

    /// Row is inside the chat rectangle at all? Clicks above it (tabs,
    /// header) must not map into row 0 via saturating arithmetic.
    fn in_chat_rect(&self, row: u16) -> bool {
        let h = self.last_chat.height.max(1);
        row >= self.last_chat.y && row < self.last_chat.y.saturating_add(h)
    }

    pub(super) fn mouse_down(&mut self, row: u16, col: u16) {
        if !self.in_chat_rect(row) {
            self.press = None;
            self.press_anchor = None;
            self.press_target = None;
            self.dragging = false;
            return;
        }
        let abs_r = self.abs_row(row);
        let char_col = self.screen_col_to_char(abs_r, col);
        self.press = Some(CellPos {
            row: abs_r,
            col: char_col,
        });
        // semantic drag anchor alongside the raw row: if rows shift before
        // the drag continues (streaming), the segment id + intra-run offset
        // still names the pressed content. Structural blanks keep the raw
        // row (today's behavior).
        let tag = self.cache_rowseg.get(abs_r).copied().flatten();
        self.press_anchor = tag.and_then(|idx| {
            let (_, meta) = self.view_transcript();
            let id = meta.get(idx).map(|m| m.id)?;
            let mut start = abs_r;
            while start > 0 && self.cache_rowseg.get(start - 1) == Some(&Some(idx)) {
                start -= 1;
            }
            Some((self.active_subagent, id, abs_r - start))
        });
        self.press_target = self.resolve_target(row, abs_r, char_col);
        self.dragging = false;
    }

    /// Press point re-resolved against the current layout. Falls back to the
    /// raw row when the anchor's view/segment is gone (deleted mid-press).
    fn reanchored_press(&self, pressed: CellPos) -> CellPos {
        let Some((view, id, off)) = self.press_anchor else {
            return pressed;
        };
        if view != self.active_subagent {
            return pressed;
        }
        let (_, meta) = self.view_transcript();
        let Some(idx) = meta.iter().position(|m| m.id == id) else {
            return pressed;
        };
        let Some((start, end)) = self.seg_row_range(idx) else {
            return pressed;
        };
        if end <= start {
            return pressed;
        }
        CellPos {
            row: start + off.min(end - start - 1),
            col: pressed.col,
        }
    }

    /// Semantic target under a chat row, resolved against the shown frame.
    /// `screen` is the press row for anchoring; `abs` indexes the layout
    /// this frame was painted from; `char_col` is the press column mapped
    /// into the row's characters (for link hit-testing).
    fn resolve_target(&self, screen: u16, abs: usize, char_col: usize) -> Option<ClickTarget> {
        // Ask rows only exist in the main view; in a subagent view
        // the same absolute rows belong to a different transcript.
        if self.active_subagent.is_none()
            && self.menu_stack.is_empty()
            && let Some((seg_idx, row)) = self.ask_row_at(abs)
        {
            let id = self.seg_meta.get(seg_idx).map(|m| m.id)?;
            return Some(ClickTarget::Ask { seg: id, row });
        }
        let tag = self.cache_rowseg.get(abs).copied()??;
        let (segs, meta) = self.view_transcript();
        let id = meta.get(tag).map(|m| m.id)?;
        // code lives in assistant text only: probe the wrapped rows solely
        // for those segments (and never for tool/thinking/status rows)
        if matches!(segs.get(tag), Some(Segment::Assistant { .. }))
            && let Some(block) = self.code_block_index(id, abs)
        {
            return Some(ClickTarget::Code { seg: id, block });
        }
        // links the same: assistant rows only, URL pinned at press time
        if matches!(segs.get(tag), Some(Segment::Assistant { .. }))
            && let Some(url) = self.link_at_row(tag, abs, char_col)
        {
            return Some(ClickTarget::Link { url });
        }
        match segs.get(tag) {
            Some(
                Segment::Thinking { .. }
                | Segment::Subagent { .. }
                | Segment::Tool { .. }
                | Segment::Status { .. },
            ) => Some(ClickTarget::Toggle { seg: id, screen }),
            _ => None,
        }
    }

    /// Current view's transcript + identity, without cloning: main chat or
    /// the open subagent's rows.
    fn view_transcript(&self) -> (&[Segment], &[SegMeta]) {
        if let Some(id) = self.active_subagent
            && let (Some(chat), Some(meta)) =
                (self.subagent_chats.get(&id), self.subagent_meta.get(&id))
        {
            return (chat, meta);
        }
        (&self.segments, &self.seg_meta)
    }

    pub(super) fn mouse_drag(&mut self, row: u16, col: u16) {
        let Some(pressed) = self.press else { return };
        let p0 = self.reanchored_press(pressed);
        let abs_r = self.abs_row(row);
        let char_col = self.screen_col_to_char(abs_r, col);
        let cur = CellPos {
            row: abs_r,
            col: char_col,
        };
        if !self.dragging && cur == p0 {
            return;
        }
        self.dragging = true;
        self.sel = Some(Selection { a: p0, b: cur });
        self.dirty = true;
    }

    /// A click on the status bar's model name or effort chip opens the same
    /// menu the key opens (Ctrl+G and the chip's own binding). Both ranges were
    /// measured while the bar was built, so the hit test follows the layout
    /// instead of guessing at it.
    fn status_chip_click(&mut self, row: u16, col: u16) -> bool {
        if row != self.status_y || !self.menu_stack.is_empty() {
            return false;
        }
        let inside =
            |range: Option<(u16, u16)>| range.is_some_and(|(x0, x1)| col >= x0 && col <= x1);
        let context = inside(self.ctx_click);
        let model = inside(self.md_click);
        if !context && !model && !inside(self.ef_click) {
            return false;
        }
        self.press = None;
        self.press_anchor = None;
        self.dragging = false;
        self.sel = None;
        if context {
            self.open_menu(Menu::Context);
        } else if model {
            let provider = self.model_cfg.provider.clone();
            self.open_menu(Menu::Models { provider });
        } else {
            self.open_menu(Menu::Effort { model: None });
        }
        true
    }

    pub(super) fn mouse_up(&mut self, row: u16, col: u16) {
        if self.status_chip_click(row, col) {
            return;
        }
        let _pressed = self.press.take();
        self.press_anchor = None;
        let target = self.press_target.take();
        let was_drag = std::mem::take(&mut self.dragging);
        if was_drag {
            if let Some(sel) = self.sel {
                self.copy_selection(&sel);
            }
            return; // keep the selection visible until the next action
        }
        self.sel = None;
        // command popup item?
        if let Some((_, item)) = self.popup_rows.iter().find(|(y, _)| *y == row) {
            let item = item.clone();
            if let Some(cmd) = self.popup_level2_cmd()
                && let Some(sub) = item.strip_prefix(&format!("{cmd} "))
            {
                self.apply_subcommand_insert(cmd, sub);
                return;
            }
            self.apply_command_insert(&item);
            return;
        }
        if let Some(target) = target {
            self.fire_target(target);
        }
    }

    pub(super) fn mouse_move(&mut self, row: u16) {
        if self.popup_visible() {
            // the mouse moves the same cursor the arrows do: one selection, two
            // hands, and Enter/click mean the same thing. The command lands on
            // the copy of itself that is inside the current window, so hovering
            // never drags the window to another rotation.
            let hit = self
                .popup_rows
                .iter()
                .position(|(y, _)| *y == row)
                .map(|p| self.popup_rows[p].1.clone());
            let walk = hit.as_ref().and_then(|name| {
                let items = self.popup_items();
                let n = items.len() as i64;
                items
                    .iter()
                    .position(|i| i == name)
                    .map(|idx| self.popup_top + (idx as i64 - self.popup_top).rem_euclid(n.max(1)))
            });
            if walk != self.popup_walk {
                self.popup_walk = walk;
                self.hover = hit;
                self.dirty = true;
            }
            return;
        }
        // hover highlight for the inline ask (chat rows, not an overlay).
        // Outside the chat rectangle the row must not saturate onto row 0:
        // a header/tab hover would otherwise light up whatever sits on top.
        if !self.in_chat_rect(row) {
            if self.ask_hover.is_some() {
                self.ask_hover = None;
                self.dirty = true;
            }
            return;
        }
        if self.menu_stack.is_empty() && self.active_ask_seg().is_some() {
            let abs = self.abs_row(row);
            let ask_hover = self.ask_row_at(abs).map(|(_, r)| r);
            if ask_hover != self.ask_hover {
                self.ask_hover = ask_hover;
                self.dirty = true;
            }
        } else if self.ask_hover.is_some() {
            self.ask_hover = None;
            self.dirty = true;
        }
    }

    /// Contiguous absolute-row range `(start, end)` of segment `seg_idx` in
    /// the current view's assembled buffers. Uses the chunk map (`asm_tags` /
    /// `asm_lens`) in O(chunks) instead of scanning all wrapped lines in
    /// `cache_rowseg`, falling back to `cache_rowseg` when a test populates
    /// `cache_rowseg` directly without `asm_tags`.
    pub(super) fn seg_row_range(&self, seg_idx: usize) -> Option<(usize, usize)> {
        let (_, meta) = self.view_transcript();
        if self.asm_tags.len() == self.asm_lens.len() && !self.asm_tags.is_empty() {
            if let Some(m) = meta.get(seg_idx) {
                let target = AsmTag::Seg(m.id);
                let mut offset = 0usize;
                for (tag, &len) in self.asm_tags.iter().zip(self.asm_lens.iter()) {
                    if *tag == target {
                        return (len > 0).then_some((offset, offset + len));
                    }
                    offset += len;
                }
            }
        }
        let start = self.cache_rowseg.iter().position(|t| *t == Some(seg_idx))?;
        let mut end = start;
        while end < self.cache_rowseg.len() && self.cache_rowseg[end] == Some(seg_idx) {
            end += 1;
        }
        Some((start, end))
    }

    /// contiguous absolute-row range of one segment in the wrapped cache
    fn ask_block_range(&self, seg_idx: usize) -> Option<(usize, usize)> {
        self.seg_row_range(seg_idx)
    }

    /// map an absolute cache row onto an interactive AskUser target, if any
    pub(super) fn ask_row_at(&self, abs_row: usize) -> Option<(usize, AskRow)> {
        let seg_idx = self.cache_rowseg.get(abs_row).copied()??;
        if Some(seg_idx) != self.active_ask_seg() {
            return None;
        }
        if !self.segments.get(seg_idx).is_some_and(tool_has_ask) {
            return None;
        }
        let (start, _) = self.ask_block_range(seg_idx)?;
        let offset = abs_row.saturating_sub(start);
        self.ask_decode(seg_idx, offset).map(|r| (seg_idx, r))
    }

    /// offset inside the segment's rendered block -> interactive row.
    /// Must stay in lockstep with `render_segment`'s tool branch: every
    /// logical line there is exactly one visual row (truncated to width).
    /// Offset 0 is the tool head row, which toggles like any other tool row
    /// and is never an answer target.
    fn ask_decode(&self, seg_idx: usize, offset: usize) -> Option<AskRow> {
        let Segment::Tool {
            ask: Some(ask),
            expanded: true,
            ..
        } = self.segments.get(seg_idx)?
        else {
            return None;
        };
        // the head row owns offset 0
        let mut line = 1usize;
        for (q_idx, q) in ask.questions.iter().enumerate() {
            if !q.header.is_empty() {
                if offset == line {
                    return None;
                }
                line += 1;
            }
            // question text itself is not clickable
            if offset == line {
                return None;
            }
            line += 1;
            for o_idx in 0..q.options.len() {
                if offset == line {
                    return Some(AskRow::Option {
                        q: q_idx,
                        opt: o_idx,
                    });
                }
                line += 1;
            }
            if q.allow_free {
                if offset == line {
                    return Some(AskRow::Custom { q: q_idx });
                }
                line += 1;
            }
            if q_idx + 1 < ask.questions.len() {
                // separator
                if offset == line {
                    return None;
                }
                line += 1;
            }
        }
        if offset == line {
            return Some(AskRow::Confirm);
        }
        None
    }

    /// Atomic helper for tests: resolve and fire in one frame, where no
    /// layout shift can intervene.
    #[cfg(test)]
    pub(super) fn click(&mut self, abs_row: usize) {
        // out of range: existing fold/copy targets never depend on the
        // column, and links must not resolve without a real press column
        self.click_at(abs_row, usize::MAX);
    }

    /// Same, with a character column (for link hit-testing).
    #[cfg(test)]
    pub(super) fn click_at(&mut self, abs_row: usize, char_col: usize) {
        let screen = self.last_chat.y.saturating_add(
            (abs_row.saturating_sub(self.chat_top(self.last_chat.height.max(1)))) as u16,
        );
        if let Some(target) = self.resolve_target(screen, abs_row, char_col) {
            self.fire_target(target);
        }
    }

    /// Execute a mouse-down-resolved target against the CURRENT layout.
    /// Every arm re-validates by segment id: a target whose segment vanished
    /// (or changed kind) between press and release is ignored, never fired
    /// at a stale row number.
    fn fire_target(&mut self, target: ClickTarget) {
        match target {
            ClickTarget::Ask { seg, row } => {
                let Some(idx) = self.index_of_seg(seg) else {
                    return;
                };
                if Some(idx) != self.active_ask_seg() {
                    return;
                }
                if !self.segments.get(idx).is_some_and(tool_has_ask) {
                    return;
                }
                // a click on an option must select it, never dismiss the
                // whole question (the old overlay did exactly that via an
                // outside-rect Esc path)
                match row {
                    AskRow::Option { q, opt } => {
                        let (allowed, multiple) = match self.segments.get(idx) {
                            Some(Segment::Tool {
                                ask: Some(ask), ..
                            }) => (
                                q <= ask.focus && ask.can_reach(q),
                                ask.questions.get(q).is_some_and(|qq| qq.multiple),
                            ),
                            _ => (false, false),
                        };
                        if !allowed {
                            return;
                        }
                        if multiple {
                            self.inline_ask_toggle(q, opt);
                        } else {
                            self.inline_ask_select(q, opt);
                        }
                    }
                    AskRow::Custom { q } => {
                        let allowed = matches!(
                            self.segments.get(idx),
                            Some(Segment::Tool { ask: Some(ask), .. })
                                if q <= ask.focus && ask.can_reach(q)
                        );
                        if allowed {
                            self.inline_ask_open_custom(q);
                        }
                    }
                    AskRow::Confirm => self.inline_ask_confirm(),
                }
            }
            ClickTarget::Toggle { seg, screen } => {
                if self.active_subagent.is_some() {
                    if let Some(id) = self.active_subagent {
                        self.click_subagent_seg(id, seg, screen);
                    }
                    return;
                }
                let Some(idx) = self.index_of_seg(seg) else {
                    return;
                };
                // clicking an error line folds/unfolds its full text (it
                // arrives collapsed); full text stays selectable by drag
                // like any row. A finished tool row reveals its output; a
                // live ask row folds its questionnaire away.
                let toggle = match self.segments.get(idx) {
                    Some(Segment::Status {
                        kind: StatusKind::Err,
                        expanded,
                        ..
                    }) => Some(!*expanded),
                    Some(Segment::Thinking { expanded, .. }) => Some(!*expanded),
                    Some(Segment::Subagent { expanded, .. }) => Some(!*expanded),
                    Some(Segment::Tool {
                        ok: Some(_),
                        expanded,
                        ..
                    }) => Some(!*expanded),
                    Some(Segment::Tool {
                        ask: Some(_),
                        expanded,
                        ..
                    }) => Some(!*expanded),
                    _ => None,
                };
                if let Some(v) = toggle {
                    // Anchor the header row BEFORE flipping so the block keeps
                    // its screen line — EXCEPT when toggling the bottom-most
                    // segment while following the tail, where anchoring would
                    // turn off `follow`, hide the composer, and bury the
                    // unfolded output below the viewport.
                    if !(self.follow && idx + 1 == self.segments.len()) {
                        self.capture_anchor_for_view(None, idx, screen);
                    }
                    // unfolding an error also copies its full text: the
                    // collapsed row is truncated, the clipboard gets all
                    if v && let Some(Segment::Status {
                        kind: StatusKind::Err,
                        text,
                        ..
                    }) = self.segments.get(idx)
                    {
                        let text = text.clone();
                        if super::clip_set(&text).is_ok() {
                            self.status("error copied to clipboard", StatusKind::Info);
                        }
                    }
                    match self.segments.get_mut(idx) {
                        Some(Segment::Thinking { expanded, .. }) => *expanded = v,
                        Some(Segment::Subagent { expanded, .. }) => *expanded = v,
                        Some(Segment::Tool { expanded, .. }) => *expanded = v,
                        Some(Segment::Status { expanded, .. }) => *expanded = v,
                        _ => {}
                    }
                    self.touch_segment(idx);
                    self.dirty = true;
                }
            }
            ClickTarget::Code { seg, block } => {
                let (segs, meta) = self.view_transcript();
                let Some(idx) = meta.iter().position(|m| m.id == seg) else {
                    return;
                };
                let Some(Segment::Assistant { text, .. }) = segs.get(idx) else {
                    return;
                };
                // block was pinned at press time; if the source changed since
                // (or the block is gone) the click is ignored, never retargeted
                if let Some(text) = code_blocks(text).into_iter().nth(block) {
                    match super::clip_set(&text) {
                        Ok(()) => self.status("code copied to clipboard", StatusKind::Info),
                        Err(e) => self.status(&format!("copy failed: {e}"), StatusKind::Err),
                    }
                }
            }
            ClickTarget::Link { url } => {
                // the URL was pinned (and scheme-gated) at press time; the
                // opener is the last step, never a resolver
                match open_browser_url(&url) {
                    Ok(()) => self.status("opened in browser", StatusKind::Info),
                    Err(e) => self.status(&format!("open failed: {e}"), StatusKind::Err),
                }
            }
        }
    }

    /// Toggle one subagent-chat row by stable id. No cache is cleared: the
    /// id-keyed entries stay valid and the fingerprint gate decides the rest.
    fn click_subagent_seg(&mut self, id: u64, seg: u64, screen: u16) {
        let Some(meta) = self.subagent_meta.get(&id) else {
            return;
        };
        let Some(idx) = meta.iter().position(|m| m.id == seg) else {
            return;
        };
        let Some(chat) = self.subagent_chats.get(&id) else {
            return;
        };
        let toggle = match chat.get(idx) {
            Some(Segment::Thinking { expanded, .. }) => Some(!*expanded),
            Some(Segment::Tool {
                ok: Some(_),
                expanded,
                ..
            }) => Some(!*expanded),
            _ => None,
        };
        let Some(v) = toggle else { return };
        // anchor BEFORE mutating so the row keeps its screen line. The
        // anchor wants the row's TAG (segment index), never its position:
        // passing the position re-anchored to whatever row later sat
        // there and yanked the viewport to the bottom on expand.
        let is_last = idx + 1 == chat.len();
        if !(self.follow && is_last) {
            if self.cache_rowseg.contains(&Some(idx)) {
                self.capture_anchor_for_view(Some(id), idx, screen);
            } else {
                self.pause_follow_for_inspection();
            }
        }
        if let Some(chat) = self.subagent_chats.get_mut(&id) {
            match chat.get_mut(idx) {
                Some(Segment::Thinking {
                    expanded: state, ..
                })
                | Some(Segment::Tool {
                    expanded: state, ..
                }) => *state = v,
                _ => {}
            }
        }
        self.sub_touch(id, idx);
        self.dirty = true;
    }

    /// Anchor a header in an explicit view (main passes `None`).
    fn capture_anchor_for_view(&mut self, view: Option<u64>, tag: usize, screen: u16) {
        let h = self.last_chat.height.max(1);
        let top = self.chat_top(h);
        self.view_top = top;
        self.follow = false;
        let y = self.last_chat.y;
        let offset = (screen.saturating_sub(y)) as usize;
        let found = self.seg_row_range(tag).is_some();
        self.pending_anchor = found.then_some((view, tag, offset));
    }

    /// Which fenced code block of a segment contains an absolute row, by
    /// walking that segment's cached wrapped rows and tracking code-frame
    /// borders (`╭` opens, `╰` closes). Tables use `┌`-corners, so a `│`
    /// heuristic can no longer mistake a table row for code.
    /// View-aware: resolves the id in the currently shown transcript.
    /// URL under a cursor inside one assistant row, if the cursor sits on a
    /// link. Geometry only: the row must belong to this segment (a wrapped
    /// label may spill parens onto the next row, but never into the next
    /// segment), the rest is the renderer's adjacency rule — see
    /// `markdown::link_url_at`.
    pub(super) fn link_at_row(
        &self,
        seg_idx: usize,
        abs_row: usize,
        char_col: usize,
    ) -> Option<String> {
        if self.cache_rowseg.get(abs_row) != Some(&Some(seg_idx)) {
            return None;
        }
        let row = self.cache_lines.get(abs_row)?;
        let same_seg = self.cache_rowseg.get(abs_row + 1) == Some(&Some(seg_idx));
        let next = same_seg
            .then(|| self.cache_lines.get(abs_row + 1))
            .flatten();
        crate::tui::markdown::link_url_at(&row.spans, next.map(|l| l.spans.as_slice()), char_col)
    }

    fn code_block_index(&self, seg: u64, abs_row: usize) -> Option<usize> {
        let (_, meta) = self.view_transcript();
        let idx = meta.iter().position(|m| m.id == seg)?;
        let (start, _) = self.ask_block_range(idx)?;
        let mut block = 0usize;
        let mut in_code = false;
        for row in start..=abs_row.min(self.cache_lines.len().saturating_sub(1)) {
            // only rows of this segment participate; anything else ends scan
            if self.cache_rowseg.get(row) != Some(&Some(idx)) {
                break;
            }
            // never index directly: hit-testing must survive transiently
            // desynced buffers (a missing row ends the scan, not the app)
            let Some(line) = self.cache_lines.get(row) else {
                break;
            };
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let trimmed = text.trim_start();
            if trimmed.starts_with('╭') {
                in_code = true;
                block += 1;
            } else if in_code && trimmed.starts_with('╰') {
                in_code = false;
            }
        }
        in_code.then_some(block.saturating_sub(1))
    }

    /// Strip UI chrome from a copied row WITHOUT eating real indentation:
    /// only exact known decorations go (tool rail, code rail, user marker,
    /// one trailing frame cap). Everything else — including code indent and
    /// the 2-column group nesting indent — is preserved byte-for-byte, so a
    /// copied snippet stays faithful to the source text.
    pub(super) fn copy_selection(&mut self, sel: &Selection) {
        // Order whole points first: for a multi-line selection dragging
        // bottom-up, start keeps its own column and end keeps its own.
        // Sorting columns independently would splice the wrong edges.
        let (first, second) = if (sel.a.row, sel.a.col) <= (sel.b.row, sel.b.col) {
            (sel.a, sel.b)
        } else {
            (sel.b, sel.a)
        };
        let (r0, r1) = (first.row, second.row);
        if self.cache_lines.is_empty() || r0 >= self.cache_lines.len() {
            return;
        }
        let max_row = self.cache_lines.len() - 1;
        let mut out = String::new();
        for r in r0..=r1.min(max_row) {
            let chars: Vec<char> = line_text(&self.cache_lines[r]).chars().collect();
            let start = if r == r0 {
                first.col.min(chars.len())
            } else {
                0
            };
            let end = if r == r1 {
                second.col.min(chars.len())
            } else {
                chars.len()
            };
            let mut line: String = chars[start..end.min(start.max(end))].iter().collect();
            line = strip_row_chrome(&line);
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&line);
        }
        match super::clip_set(&out) {
            Ok(()) => self.status("copied", StatusKind::Ok),
            Err(e) => self.status(&format!("copy failed: {e}"), StatusKind::Err),
        }
        self.dirty = true;
    }

    /// True while a resize drag is (probably) still in flight: a resize
    /// event landed less than the settle window ago. Width-driven rebuilds
    /// defer until this clears; content-driven rebuilds proceed regardless.
    fn resize_settling(&self) -> bool {
        self.last_resize
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(150))
    }

    pub(super) fn chat_top(&self, height: u16) -> usize {
        let h = height.max(1) as usize;
        // the scrolling card is part of the document, so the view can reach
        // past the last history row into its rows
        let max = (self.cache_lines.len() + self.surf_rows).saturating_sub(h);
        if self.follow {
            max
        } else {
            self.view_top.min(max)
        }
    }

    /// Freeze the current viewport and stop following: the next layout change
    /// (toggle, stream append) no longer yanks the screen. Call before any
    /// inspection-driven mutation.
    pub(super) fn pause_follow_for_inspection(&mut self) {
        let top = self.chat_top(self.last_chat.height.max(1));
        self.view_top = top;
        self.follow = false;
        self.pending_anchor = None;
    }

    /// Restore a captured anchor against the freshly rebuilt rows. Drops it
    /// when the tagged row is gone (segment removed concurrently).
    fn apply_pending_anchor(&mut self, view: Option<u64>) {
        let Some((anchor_view, tag, screen)) = self.pending_anchor.take() else {
            return;
        };
        if anchor_view != view {
            return;
        }
        let h = self.last_chat.height.max(1) as usize;
        let max = self.cache_lines.len().saturating_sub(h);
        if let Some((abs, _)) = self.seg_row_range(tag) {
            self.view_top = abs.saturating_sub(screen).min(max);
            self.follow = false;
        }
    }

    /// Transcript header at the top of the session history:
    /// ` sqwai v<version>` (with brand shimmer on `sqwai` and dim version)
    /// followed by ` <cwd>` in dim gray. Fits even the narrowest terminal.
    pub(super) fn empty_mark_lines(&self) -> Vec<Line<'static>> {
        let mut spans = vec![Span::styled(" ".to_string(), Theme::base())];
        if self.segments.is_empty() {
            spans.extend(crate::tui::shimmer::shimmer_brand_spans(
                "sqwai",
                self.spinner_tick,
            ));
        } else {
            spans.push(Span::styled(
                "sqwai".to_string(),
                Theme::brand_ink().add_modifier(Modifier::BOLD),
            ));
        }
        spans.push(Span::styled(
            format!(" v{}", env!("CARGO_PKG_VERSION")),
            Theme::dim(),
        ));
        let w = self.last_chat.width.max(self.cache_w).max(20) as usize;
        let cwd = truncate_display_width(&self.cwd_label, w.saturating_sub(1));
        vec![
            Line::from(spans),
            Line::from(Span::styled(format!(" {cwd}"), Theme::dim())),
        ]
    }

    /// Test/compat entry: production renders via [`Self::render_into`]
    /// for the presenter thread.
    #[cfg(test)]
    pub(super) fn draw(&mut self, f: &mut ratatui::Frame) {
        let area = f.area();
        self.render_into(f.buffer_mut(), area);
    }

    /// Terminal-independent render: paints the whole UI into `buf` without
    /// touching a `Terminal`, so the frame can be produced on the UI thread
    /// and presented by a dedicated presenter thread. `draw` stays as a thin
    /// wrapper for tests.
    pub(super) fn render_into(&mut self, buf: &mut Buffer, area: Rect) {
        // cleared every frame: only a path that actually paints the surface
        // sets it back, so the subagent view and a too-small screen cannot
        // leave a stale one behind for the scroll routing
        self.surface_open = false;
        self.surf_rows = 0;
        // cleared every frame: only a deferred width rebuild sets it below,
        // and the loop keeps `dirty` while it stays set
        self.defer_rebuild = false;
        if area.width < 20 || area.height < 6 {
            return;
        }
        if let Some(id) = self.active_subagent {
            self.draw_subagent_chat(buf, area, id);
            return;
        }
        let input_rows = self.input.lines().len().clamp(1, 6) as u16;
        // The borderless composer takes exactly its content height: one row
        // until the user enters a newline, then it grows up to six rows.
        let input_h = input_rows;
        // A surface, not a popup: it owns the composer's slot at any terminal
        // size, so there is one form to learn and the transcript above is never
        // dimmed or cleared for it. Effort is the fixed seven-row picker;
        // /context is its card — the rule plus the rows the menu already builds.
        // One system for every surface (effort today, /context, and whatever
        // moves here next): it is a block at the end of the history, not
        // furniture over it. It joins the document, sits pinned to the bottom
        // of the screen at the end of it, and scrolls off from there — through
        // its own rows when it is taller than the terminal — so nothing is ever
        // cut short and the transcript above is never dimmed or cleared.
        const EFFORT_SURFACE_H: u16 = 7;
        // Chosen once, before the layout: the height and the painter that fills
        // it must not disagree about which menu is open.
        let surf_menu = self.cur_menu().cloned();
        // The `/` command list rides the same rail: it is a block at the end of
        // the history, not a box over it, and the arrows are the only thing
        // that moves inside it. One row of the block is the blank separator
        // above the first command.
        let list_rows = if !matches!(surf_menu, Some(Menu::Effort { .. }) | Some(Menu::Context))
            && self.menu_stack.is_empty()
            && self.popup_visible()
        {
            self.popup_items().len().min(POPUP_MAX_ROWS) as u16 + 1
        } else {
            0
        };
        let list_block = list_rows > 0;
        let surf_full = match surf_menu {
            Some(Menu::Effort { .. }) => EFFORT_SURFACE_H,
            // rule + title + air + card rows + the close hint under them
            Some(Menu::Context) => self.menu_rows.len() as u16 + 4,
            // rule + title + air + the leading rows + the window + the "+N"
            // caption when there is one + the row under them. The hint column
            // rides rows the list already has, but the block never gets shorter
            // than the column: a one-model list still has to show its keys
            // under its own rows rather than across the rule.
            Some(Menu::Settings)
            | Some(Menu::Debug)
            | Some(Menu::Plan)
            | Some(Menu::Help)
            | Some(Menu::ContextView)
            | Some(Menu::TestAnim)
            | Some(Menu::TestArt)
            | Some(Menu::TestColors)
            | Some(Menu::TestMd)
            | Some(Menu::UndoShadow)
            | Some(Menu::UndoList)
            | Some(Menu::Providers)
            | Some(Menu::EditProvider { .. })
            | Some(Menu::ProviderFormat { .. })
            | Some(Menu::Models { .. })
            | Some(Menu::EditModel { .. })
            | Some(Menu::Sessions)
            | Some(Menu::ConfirmDelete { .. })
            | Some(Menu::Approval { .. }) => {
                let (head, win, _) = self.list_shape(area.height);
                let body = self.menu_rows.len().saturating_sub(head);
                let shown = body.min(win).max(1).max(self.list_hints().len());
                (3 + head + shown + usize::from(body > shown) + 1) as u16
            }
            Some(
                Menu::EditModelField { .. }
                | Menu::EditProviderField { .. }
                | Menu::EditSessionTitle { .. }
                | Menu::EditScalar(..),
            ) => (3 + self.list_hints().len().max(2)) as u16,
            _ => list_rows,
        };
        let surface = matches!(
            surf_menu,
            Some(Menu::Effort { .. })
                | Some(Menu::Context)
                | Some(Menu::ContextView)
                | Some(Menu::Help)
                | Some(Menu::TestAnim)
                | Some(Menu::TestArt)
                | Some(Menu::TestColors)
                | Some(Menu::TestMd)
                | Some(Menu::Settings)
                | Some(Menu::Debug)
                | Some(Menu::Plan)
                | Some(Menu::EditScalar(..))
                | Some(Menu::UndoShadow)
                | Some(Menu::UndoList)
                | Some(Menu::Providers)
                | Some(Menu::EditProvider { .. })
                | Some(Menu::EditProviderField { .. })
                | Some(Menu::ProviderFormat { .. })
                | Some(Menu::Models { .. })
                | Some(Menu::EditModel { .. })
                | Some(Menu::EditModelField { .. })
                | Some(Menu::Sessions)
                | Some(Menu::EditSessionTitle { .. })
                | Some(Menu::ConfirmDelete { .. })
                | Some(Menu::Approval { .. })
        );
        // the block joins the document either way; only a menu surface also
        // claims the composer's own rows
        self.surf_rows = surf_full as usize;
        self.surface_open = surface || list_block;
        // the slot the surface would claim in the layout; the painter uses the
        // scrolled position instead, so this only keeps the sums honest
        let surf_h = surf_full.min(area.height);
        // One slot sits above the composer. A notice owns it; when there is
        // nothing to say, the queue moves down into it instead of leaving the
        // row empty under its own line. The queue keeps a row of its own only
        // while a notice holds the slot — the two never both go unused.
        let notice = self.notice_line(area.width);
        let queued = !self.pending_queue.is_empty();
        // The bottom band is the tail of the history, not furniture hung over
        // it: at the end of the transcript it paints in its own slots, scrolled
        // up those same slots belong to the history and nothing floats below it.
        let scrolled_up = !self.follow;
        // The notice row and the air above the composer step aside for the
        // command list too: the list is the block that owns that space, so it
        // also drops one row into it.
        let band_hidden = surface || list_block;
        let queue_h = u16::from(!band_hidden && notice.is_some() && queued);
        // the row directly above the composer: the slot when something owns
        // it, plain air otherwise. A surface and the command list claim the
        // whole bottom of the screen for themselves — no notice, no queue, no
        // air above them.
        let air_h = u16::from(!band_hidden);
        let slot_h = if surface { surf_h } else { input_h };
        // the status line hides with the surface too: the rule across the top
        // of the surface is the only separator it needs
        let status_h = u16::from(!surface);
        let band_h = queue_h + air_h + slot_h + status_h;
        let mut constraints = vec![
            Constraint::Min(3),
            Constraint::Length(queue_h),
            Constraint::Length(air_h),
        ];
        let live_r = (!band_hidden && !scrolled_up && (notice.is_some() || queued))
            .then_some(constraints.len() - 1);
        constraints.push(Constraint::Length(slot_h));
        let input_r = constraints.len() - 1;
        constraints.push(Constraint::Length(status_h));
        let status_r = constraints.len() - 1;
        let layout = Layout::vertical(constraints).split(area);
        let chat = Rect {
            x: area.x + 1,
            y: layout[0].y,
            width: area.width.saturating_sub(2),
            height: if surface {
                // the surface is inside the document, so the window is the screen
                area.height
            } else {
                layout[0]
                    .height
                    .saturating_add(if scrolled_up { band_h } else { 0 })
            },
        };

        // The transcript is rendered inside `chat`, not the outer frame. Use
        // that exact width for cache invalidation and frame construction so a
        // resize cannot leave rows wider than the rectangle that displays them.
        // Reassembly is gated by the transcript fingerprint: paint-only state
        // (typing, scroll, selection, hover) never rebuilds the transcript.
        // Width-driven rebuilds additionally wait out an active resize drag
        // (see resize_settling): repainting the old rows for ~150ms beats a
        // full transcript re-render on every intermediate drag event.
        if self.cache_w != chat.width {
            if self.resize_settling() {
                self.defer_rebuild = true;
            } else {
                self.rebuild_cache(chat.width);
                self.last_fp = self.transcript_fp(chat.width, &self.segments, &self.seg_meta);
            }
        } else {
            let fp = self.transcript_fp(chat.width, &self.segments, &self.seg_meta);
            if fp != self.last_fp {
                self.rebuild_cache(chat.width);
                self.last_fp = fp;
            } else {
                self.update_live_chrome_rows(chat.width);
            }
        }
        self.last_chat = chat;
        self.last_input = layout[input_r];
        self.apply_pending_anchor(None);
        // Where the block sits this frame: the document is the history rows plus
        // its own, so at the end of it the block is pinned to the bottom of the
        // transcript window, scrolling up carries it down and off, and a block
        // taller than the window is scrolled through rather than cut short. For
        // a menu surface that window is the whole screen (it claims the
        // composer's rows too); for the command list it stops above them.
        let block_geom = (surface || list_block).then(|| {
            let top = self.chat_top(chat.height) as isize;
            let start = (chat.y as isize + self.cache_lines.len() as isize - top)
                .max(chat.bottom() as isize - surf_full as isize);
            let skip = (chat.y as isize - start).max(0) as u16;
            let y = start.max(chat.y as isize) as u16;
            let height = (surf_full - skip).min(area.bottom().saturating_sub(y));
            (
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height,
                },
                skip,
            )
        });

        Block::new().style(Theme::base()).render(area, buf);

        // No startup screen: empty sessions render the plain transcript.
        // Render visible lines directly into `buf` by reference (zero clones
        // of Line/Span/String on the hot path); only rows under an active
        // drag selection allocate a temporary reversed Line.
        {
            let top = self.chat_top(chat.height);
            let sel_range = self.sel.map(|s| s.ordered());
            let first_mark = if self.active_subagent.is_none() && sel_range.is_none() && top <= 1 {
                self.empty_mark_lines().into_iter().next()
            } else {
                None
            };
            let user_bg = Style::new().bg(Theme::INPUT_BG());
            for (screen_row, abs_row) in (top..top + chat.height as usize).enumerate() {
                let y = chat.y + screen_row as u16;
                let is_user = self
                    .cache_rowseg
                    .get(abs_row)
                    .and_then(|tag| *tag)
                    .is_some_and(|idx| matches!(self.segments.get(idx), Some(Segment::User(_))));
                if is_user {
                    buf.set_style(
                        Rect {
                            x: area.x,
                            y,
                            width: area.width,
                            height: 1,
                        },
                        user_bg,
                    );
                }
                if abs_row == 1
                    && let Some(ref mark) = first_mark
                {
                    buf.set_line(chat.x, y, mark, chat.width);
                } else if let Some(l) = self.cache_lines.get(abs_row) {
                    if let Some((first, second)) = sel_range
                        && abs_row >= first.row
                        && abs_row <= second.row
                    {
                        let chars = line_text(l).chars().count();
                        let sel_line = if chars == 0 {
                            // empty row inside the selection: full-width highlight
                            Line::from(vec![Span::styled(
                                " ".repeat(chat.width as usize),
                                Style::new().add_modifier(Modifier::REVERSED),
                            )])
                        } else {
                            let cs = if abs_row == first.row {
                                first.col.min(chars)
                            } else {
                                0
                            };
                            let ce = if abs_row == second.row {
                                second.col.min(chars)
                            } else {
                                chars
                            };
                            apply_sel(l, cs, ce.max(cs))
                        };
                        buf.set_line(chat.x, y, &sel_line, chat.width);
                    } else {
                        buf.set_line(chat.x, y, l, chat.width);
                    }
                }
            }
        }

        // The queue renders where it has room: its own row above the slot when
        // a notice holds that row, the slot itself when nothing is waiting to
        // say anything. The composer stays a filled band; there are no
        // separator lines.
        if !scrolled_up
            && !band_hidden
            && queued
            && let Some(r) = if notice.is_some() { Some(1) } else { live_r }
        {
            Paragraph::new(self.queue_line(area.width)).render(layout[r], buf);
        }
        // one status row above the input, for transient notices only. The
        // "what the agent is doing" row is not here: it is the last row of the
        // transcript, so it scrolls with the work it describes.
        if let Some(r) = live_r
            && let Some(line) = notice
        {
            Paragraph::new(line).render(layout[r], buf);
        }
        // The effort surface owns this slot while open: no composer, no
        // band fill and no caret — a block cursor left in the slot would
        // sit on the surface title, and the gray band is exactly what the
        // redesign removes.
        if !surface && !scrolled_up {
            self.input.set_block(Self::input_block());
            // the cursor is rendered by tui-textarea; the input has no frame.
            // `› ` marks the top input row (like the user strip); the textarea
            // is shifted right so text aligns under it on every row.
            let marker_w = self.input_marker_w();
            let input_rect = Rect {
                x: layout[input_r].x + marker_w,
                y: layout[input_r].y,
                width: layout[input_r].width.saturating_sub(marker_w),
                height: layout[input_r].height,
            };
            if marker_w > 0 {
                Paragraph::new(Line::from(Span::styled(
                    "› ",
                    Style::new().add_modifier(Modifier::BOLD | Modifier::DIM),
                )))
                .render(
                    Rect {
                        x: layout[input_r].x,
                        y: layout[input_r].y,
                        width: marker_w,
                        height: 1,
                    },
                    buf,
                );
            }
            self.input.render(input_rect, buf);
            // composer band fill: gutter, marker and short rows share the
            // input background (the textarea paints its own rect already).
            // Both None and explicit Reset count as unpainted; real fills
            // (block cursor, text selection) must survive.
            for y in layout[input_r].y..layout[input_r].bottom() {
                for x in layout[input_r].x..layout[input_r].right() {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        let mut style = cell.style();
                        if matches!(style.bg, None | Some(ratatui::style::Color::Reset)) {
                            style.bg = Some(Theme::INPUT_BG());
                            cell.set_style(style);
                        }
                    }
                }
            }
        }
        // the chips are only clickable while the bar is on screen: scrolled up,
        // that row shows history and no chip lives there
        self.status_y = if scrolled_up {
            u16::MAX
        } else {
            layout[status_r].y
        };
        if !surface && !scrolled_up {
            let sb = self.status_bar(area.width);
            sb.render(layout[status_r], buf);
        }

        match block_geom {
            // off screen means not painted, never cut short
            Some((rect, skip)) if rect.height > 0 => {
                if !surface {
                    self.draw_popup(buf, rect, skip, surf_full);
                } else {
                    match surf_menu {
                        Some(Menu::Context) => {
                            self.draw_context_surface(buf, rect, skip, surf_full)
                        }
                        Some(Menu::Settings)
                        | Some(Menu::Debug)
                        | Some(Menu::Plan)
                        | Some(Menu::Help)
                        | Some(Menu::ContextView)
                        | Some(Menu::TestAnim)
                        | Some(Menu::TestArt)
                        | Some(Menu::TestColors)
                        | Some(Menu::TestMd)
                        | Some(Menu::EditScalar(..))
                        | Some(Menu::UndoShadow)
                        | Some(Menu::UndoList)
                        | Some(Menu::Providers)
                        | Some(Menu::EditProvider { .. })
                        | Some(Menu::EditProviderField { .. })
                        | Some(Menu::ProviderFormat { .. })
                        | Some(Menu::Models { .. })
                        | Some(Menu::EditModel { .. })
                        | Some(Menu::EditModelField { .. })
                        | Some(Menu::Sessions)
                        | Some(Menu::EditSessionTitle { .. })
                        | Some(Menu::ConfirmDelete { .. })
                        | Some(Menu::Approval { .. }) => {
                            self.draw_list_surface(buf, rect, skip, surf_full)
                        }
                        Some(Menu::Effort { .. }) => {
                            self.draw_effort_slider(buf, rect, skip, surf_full)
                        }
                        _ => {}
                    }
                }
            }
            Some(_) => {
                self.effort_hits.clear();
                self.surf_hits.clear();
                self.menu_rect = Rect::default();
                self.popup_rows.clear();
            }
            None => self.popup_rows.clear(),
        }
        self.draw_menu(buf, area);
    }

    /// Open a subagent transcript, restoring its saved scroll position.
    /// The main view's scroll is stashed aside (not destroyed); closing the
    /// view restores it, so visiting a subagent no longer yanks the main chat.
    /// Park the active view's assembled rows into a `StoredView`, leaving
    /// empty live buffers behind. `last_fp` is poisoned so a draw without a
    /// loaded store rebuilds instead of trusting empty rows.
    fn take_active_store(&mut self) -> StoredView {
        let store = StoredView {
            lines: std::mem::take(&mut self.cache_lines),
            rowseg: std::mem::take(&mut self.cache_rowseg),
            tags: std::mem::take(&mut self.asm_tags),
            lens: std::mem::take(&mut self.asm_lens),
            fp: self.last_fp,
        };
        self.last_fp = u64::MAX;
        store
    }

    /// Make a parked view live: its rows, chunk map and fingerprint move
    /// back into the active buffers, so the next draw's fingerprint gate
    /// hits and skips reassembly entirely.
    fn load_active_store(&mut self, store: StoredView) {
        self.cache_lines = store.lines;
        self.cache_rowseg = store.rowseg;
        self.asm_tags = store.tags;
        self.asm_lens = store.lens;
        self.last_fp = store.fp;
    }

    #[cfg(test)]
    pub(super) fn open_subagent_view(&mut self, id: u64) {
        if let Some(cur) = self.active_subagent {
            if cur == id {
                return;
            }
            // A -> B: park A's rows and scroll before loading B
            self.sub_views.insert(cur, (self.view_top, self.follow));
            let parked = self.take_active_store();
            self.sub_stores.insert(cur, parked);
        } else {
            // Stash only when coming from the main view: opening B while
            // viewing A must not overwrite the stashed main rows with A's.
            self.stashed_main_scroll = Some((self.view_top, self.follow));
            self.main_store = self.take_active_store();
        }
        let (top, follow) = self.sub_views.get(&id).copied().unwrap_or((0, true));
        self.view_top = top;
        self.follow = follow;
        if let Some(store) = self.sub_stores.remove(&id) {
            self.load_active_store(store);
        }
        // else: buffers are empty with a poisoned fp, so the next draw
        // assembles this transcript from the id-keyed segment cache
        self.active_subagent = Some(id);
        self.press_target = None;
        self.dirty = true;
    }

    /// Close the subagent view, parking its rows + scroll first so a revisit
    /// restores them whole, then restoring the stashed main rows + scroll.
    /// The main transcript may have grown meanwhile (background streaming):
    /// its stored fingerprint is stale then, and the next draw splices only
    /// the new tail instead of reassembling.
    pub(super) fn close_subagent_view(&mut self) {
        if let Some(id) = self.active_subagent.take() {
            self.sub_views.insert(id, (self.view_top, self.follow));
            let parked = self.take_active_store();
            self.sub_stores.insert(id, parked);
            let main = std::mem::take(&mut self.main_store);
            self.load_active_store(main);
        }
        if let Some((top, follow)) = self.stashed_main_scroll.take() {
            self.view_top = top;
            self.follow = follow;
        } else {
            self.follow = true;
        }
        self.press_target = None;
        self.dirty = true;
    }

    fn draw_subagent_chat(&mut self, buf: &mut Buffer, area: Rect, id: u64) {
        let layout = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
        let chat = Rect {
            x: area.x + 1,
            y: layout[1].y,
            width: area.width.saturating_sub(2),
            height: layout[1].height,
        };
        Block::new().style(Theme::base()).render(area, buf);
        Paragraph::new(Line::from(Span::styled(
            format!(" subagent-{id}"),
            Theme::accent_bold(),
        )))
        .style(Theme::base())
        .render(layout[0], buf);
        // Render the subagent transcript through the SAME id-keyed cache and
        // fingerprint gate as the main view — no transcript cloning, no
        // dataset swap, no cache wipe. Entries for both transcripts coexist
        // because segment ids are globally unique. The live buffers already
        // hold this view's rows (loaded by open, parked by close), so a
        // repeat draw with matching fingerprint reassembles nothing.
        let Some(meta) = self.subagent_meta.get(&id).cloned() else {
            self.last_chat = chat;
            self.last_input = Rect::default();
            return;
        };
        let fp = {
            let sub_segs = self
                .subagent_chats
                .get(&id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            self.transcript_fp(chat.width, sub_segs, &meta)
        };
        if self.cache_w != chat.width && self.resize_settling() {
            self.defer_rebuild = true;
        } else if self.cache_w != chat.width || fp != self.last_fp {
            self.rebuild_sub_cache(chat.width, id);
            self.last_fp = fp;
        }
        self.last_chat = chat;
        self.last_input = Rect::default();
        self.apply_pending_anchor(Some(id));
        let top = self.chat_top(chat.height);
        for (screen_row, l) in self
            .cache_lines
            .iter()
            .skip(top)
            .take(chat.height as usize)
            .enumerate()
        {
            buf.set_line(chat.x, chat.y + screen_row as u16, l, chat.width);
        }
        // scroll is parked by close/switch, not here: persisting mid-view is
        // what the old code did for the fingerprint, and it is unnecessary
        // now that the live buffers (and last_fp) ARE this view's state
        self.last_chat = chat;
        Paragraph::new(Line::from(Span::styled(" esc close", Theme::dim())))
            .style(Theme::base())
            .render(layout[2], buf);
    }

    /// Assemble one subagent transcript into the live buffers.
    /// Same pipeline as the main view (render → wrap → id-keyed cache),
    /// with the same activity folding: finished groups come from the store,
    /// the running turn recomputes live. Borrows are scoped per index so
    /// the cache insert never aliases the transcript slice it was rendered
    /// from.
    fn rebuild_sub_cache(&mut self, width: u16, id: u64) {
        // timed for the `/debug` perf log, like the main rebuild
        let t0 = std::time::Instant::now();
        // No cache wipe on width change: entries carry their own width.
        let w = width.saturating_sub(2).max(10);
        let count = self
            .subagent_chats
            .get(&id)
            .map(|chat| chat.len())
            .unwrap_or(0);
        let mut chunks: Vec<RowChunk> = Vec::new();
        let mut fresh: Vec<bool> = Vec::new();
        // flat transcript, same as the main one: every segment renders its
        // own rows, no collapsing. Finished turns are frozen in `sub_groups`
        // as data (kept for symmetry with the main transcript), but the
        // view never hides rows behind footers anymore.
        for idx in 0..count {
            let (mid, mrev, key) = match (self.subagent_chats.get(&id), self.subagent_meta.get(&id))
            {
                (Some(chat), Some(meta)) => match (chat.get(idx), meta.get(idx)) {
                    (Some(seg), Some(m)) => (m.id, m.rev, self.seg_key(seg)),
                    _ => continue,
                },
                _ => continue,
            };
            let render_w = w;
            let hit = matches!(self.seg_cache.get(&mid), Some(entry)
                if entry.rev == mrev && entry.width == render_w && entry.key == key);
            if hit {
                // live rows are reused by the merge step: no clone here
                chunks.push((AsmTag::Seg(mid), Vec::new()));
                fresh.push(false);
                continue;
            }
            self.test_renders += 1;
            let live = matches!(
                self.subagent_chats.get(&id).and_then(|chat| chat.get(idx)),
                Some(Segment::Assistant { live: true, .. })
            );
            let rows: Vec<(Line<'static>, Option<usize>)> = if live {
                // disjoint field borrows, same shape as the main transcript
                // path: the child answer text is read while the prefix store
                // is updated
                let Self {
                    subagent_chats,
                    live_md,
                    hl,
                    ..
                } = self;
                let chat = subagent_chats.get(&id).unwrap();
                match &chat[idx] {
                    Segment::Assistant { text, .. } => {
                        live_md.wrapped_lines(mid, idx, text, render_w, hl)
                    }
                    _ => Vec::new(), // the live check above pins this match
                }
            } else {
                // immutable borrow ends before the cache insert below
                let chat = self.subagent_chats.get(&id).unwrap();
                let chunk = self.render_segment(chat, idx, render_w, false);
                let (rows, tags) = wrap_tagged(chunk, w);
                rows.into_iter().zip(tags).collect()
            };
            self.seg_cache.insert(
                mid,
                SegCacheEntry {
                    rev: mrev,
                    width: render_w,
                    key,
                    rows: rows.clone(),
                },
            );
            chunks.push((AsmTag::Seg(mid), rows));
            fresh.push(true);
        }
        self.prune_seg_cache();
        self.merge_chunks(chunks, fresh);
        self.cache_w = width;
        self.last_rebuild_us = t0.elapsed().as_micros();
    }
}

#[cfg(test)]
mod tests {
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn modal_dim_steps_painted_backgrounds_down() {
        use super::dim_bg;
        use ratatui::style::Color;
        // transparent and already-dark stay put
        assert_eq!(dim_bg(Color::Reset), Color::Reset);
        assert_eq!(dim_bg(Color::Black), Color::Black);
        assert_eq!(dim_bg(Color::DarkGray), Color::DarkGray);
        // user band (grayscale ramp) scales down the ramp, stays distinct
        assert_eq!(dim_bg(Color::Indexed(235)), Color::Indexed(233));
        assert_eq!(dim_bg(Color::Indexed(232)), Color::Indexed(232));
        // block cursor / selections collapse to dark gray
        assert_eq!(dim_bg(Color::White), Color::DarkGray);
        assert_eq!(dim_bg(Color::Cyan), Color::DarkGray);
        // truecolor scales toward black
        assert_eq!(dim_bg(Color::Rgb(100, 150, 200)), Color::Rgb(60, 90, 120));
    }

    /// The status bar budgets its right-hand group in terminal columns. These
    /// two helpers are that measurement, so they must not fall back to
    /// counting characters: a directory such as `~/仕事/proj` is 9 characters
    /// but 11 columns, and undercounting it pushes the whole group off screen
    /// and shifts every click target.
    #[test]
    fn status_bar_measures_columns_not_characters() {
        use super::{cols, truncate_display_width};

        assert_eq!(cols(" model "), 7);
        assert_eq!("~/仕事/proj".chars().count(), 9, "9 characters");
        assert_eq!(cols("~/仕事/proj"), 11, "but 11 columns");
        assert_eq!(cols("~/Проекты"), 9, "cyrillic is one column per char");

        // truncation stays inside the column budget for wide text
        for budget in [4usize, 8, 20] {
            let wide = truncate_display_width("日本語のプロジェクト", budget);
            assert!(
                UnicodeWidthStr::width(wide.as_str()) <= budget,
                "budget {budget}: {wide:?} is {} columns",
                UnicodeWidthStr::width(wide.as_str())
            );
        }
    }
}

#[allow(dead_code)]
fn pad_display(s: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(used)))
}

/// Background step-down for the modal-menu dim pass. DIM alone only dulls
/// foreground intensity, so this maps every painted background one step
/// toward black while foreground hues (kept, +DIM) still carry the meaning:
/// - transparent / already-dark stays put;
/// - grayscale-ramp grays (our user band) scale down the ramp;
/// - bright/white/plenty-chromatic backgrounds collapse to dark gray.
///
/// Other 256-palette entries are left alone: we never emit them as
/// backgrounds, and guessing their hue without a terminal query is worse
/// than leaving one bright cell behind.
pub(super) fn dim_bg(bg: ratatui::style::Color) -> ratatui::style::Color {
    use ratatui::style::Color;
    match bg {
        Color::Reset | Color::Black | Color::DarkGray => bg,
        Color::Indexed(i) if (232..=255).contains(&i) => Color::Indexed(232 + (i - 232) * 3 / 5),
        Color::White
        | Color::Gray
        | Color::Red
        | Color::Green
        | Color::Yellow
        | Color::Blue
        | Color::Magenta
        | Color::Cyan
        | Color::LightRed
        | Color::LightGreen
        | Color::LightYellow
        | Color::LightBlue
        | Color::LightMagenta
        | Color::LightCyan => Color::DarkGray,
        Color::Indexed(_) => bg,
        Color::Rgb(r, g, b) => {
            let down = |c: u8| (c as u16 * 3 / 5) as u8;
            Color::Rgb(down(r), down(g), down(b))
        }
    }
}

/// Terminal columns a status-bar label occupies.
pub(super) fn cols(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

pub(super) fn truncate_display_width(s: &str, width: usize) -> String {
    let clean: std::borrow::Cow<'_, str> = if s.contains(['\n', '\r', '\t']) {
        std::borrow::Cow::Owned(
            s.replace('\t', " ")
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
        )
    } else {
        std::borrow::Cow::Borrowed(s)
    };
    let s = clean.as_ref();
    if UnicodeWidthStr::width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    if used < width {
        out.push('…');
    }
    out
}
#[cfg(test)]
mod frame_tests {
    use super::{pad_display, truncate_display_width};
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn padding_uses_terminal_width_not_character_count() {
        let padded = pad_display("界", 6);
        assert_eq!(UnicodeWidthStr::width(padded.as_str()), 6);
    }

    #[test]
    fn truncation_never_exceeds_requested_width() {
        for value in [
            "a".repeat(100),
            "界界界界".to_string(),
            "a界b界c".to_string(),
        ] {
            for width in 0..=12 {
                assert!(
                    UnicodeWidthStr::width(truncate_display_width(&value, width).as_str()) <= width,
                    "value exceeded width {width}: {value:?}"
                );
            }
        }
    }
}

/// Push one structural row wrapped immediately: separators and group
/// headers are final on their own, so they never wait for a whole-list
/// wrap pass — segment chunks are wrapped at cache time instead.
/// Wrap one structural row (blank spacer, group header) as its own assembly
/// chunk, so the merge step can splice it independently of segment rows.
pub(super) fn struct_chunk(ord: u64, line: Line<'static>, tag: Option<usize>, width: u16) -> RowChunk {
    let (rows, tags) = wrap_tagged(vec![(line, tag)], width);
    (AsmTag::Struct(ord), rows.into_iter().zip(tags).collect())
}

/// Blank structural spacer chunk: already a single empty styled row at any
/// width, so it bypasses `wrap_tagged` and `sanitize_line`.
pub(super) fn blank_chunk(ord: u64) -> RowChunk {
    (AsmTag::Struct(ord), vec![(blank(), None)])
}

pub(super) fn blank() -> Line<'static> {
    Line::from(vec![Span::styled(String::new(), Theme::base())])
}

/// The live tail row of the transcript: what the agent is doing right now,
/// how long this turn has run, and how to stop it. The marker cycles through
/// the `sand` family (see `/test anim`); the paint stays static — the word
/// already changes when the phase changes, so a wave over it would be
/// decoration.
pub(super) fn working_line(thinking: bool, elapsed_s: Option<u64>, tick: usize) -> Line<'static> {
    let brand = |c: (u8, u8, u8)| Style::new().fg(Color::Rgb(c.0, c.1, c.2));
    let mut spans = vec![
        Span::styled(
            format!("{} ", crate::tui::anim::working_mark(tick)),
            brand(crate::tui::art::CORAL_START),
        ),
        Span::styled(
            if thinking { "thinking" } else { "working" },
            brand(crate::tui::art::CORAL_MUTED),
        ),
    ];
    if let Some(secs) = elapsed_s {
        spans.push(Span::styled(format!(" · {secs}s"), Theme::dim()));
    }
    spans.push(Span::styled(" · esc to interrupt", Theme::dim()));
    Line::from(spans)
}

pub(super) fn dim_all(l: Line<'static>) -> Line<'static> {
    Line::from(
        l.spans
            .into_iter()
            .map(|s| Span::styled(s.content, s.style.patch(Style::new().fg(Theme::DIM()))))
            .collect::<Vec<_>>(),
    )
}

pub(super) fn line_text(l: &Line<'_>) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Apply reverse-video to the char range [cs, ce) of a logical line.
fn apply_sel(l: &Line<'_>, cs: usize, ce: usize) -> Line<'static> {
    let owned = |l: &Line<'_>| {
        Line::from(
            l.spans
                .iter()
                .map(|s| Span::styled(s.content.to_string(), s.style))
                .collect::<Vec<_>>(),
        )
    };
    if ce <= cs {
        return owned(l);
    }
    let mut out: Vec<Span> = Vec::new();
    let mut pos = 0usize;
    for span in &l.spans {
        let len = span.content.chars().count();
        let s0 = pos;
        let s1 = pos + len;
        pos = s1;
        if s1 <= cs || s0 >= ce {
            out.push(Span::styled(span.content.to_string(), span.style));
            continue;
        }
        // split this span into before/inside/after
        let chars: Vec<char> = span.content.chars().collect();
        let inside_start = cs.saturating_sub(s0).min(len);
        let inside_end = ce.saturating_sub(s0).min(len);
        let before: String = chars[..inside_start].iter().collect();
        let inside: String = chars[inside_start..inside_end].iter().collect();
        let after: String = chars[inside_end..].iter().collect();
        if !before.is_empty() {
            out.push(Span::styled(before, span.style));
        }
        out.push(Span::styled(
            inside,
            span.style.add_modifier(Modifier::REVERSED),
        ));
        if !after.is_empty() {
            out.push(Span::styled(after, span.style));
        }
    }
    Line::from(out)
}
