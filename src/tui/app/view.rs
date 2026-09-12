#![allow(unused_imports)]
use super::menus::POPUP_MAX_ROWS;
use super::*;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget};
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
/// id; structural rows (blank spacers, group headers) by ordinal among the
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
type RowChunk = (AsmTag, Vec<(Line<'static>, Option<usize>)>);

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
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum ClickTarget {
    Proposal {
        seg: u64,
        row: ProposalRow,
    },
    Ask {
        seg: u64,
        row: AskRow,
    },
    Toggle {
        seg: u64,
        screen: u16,
    },
    /// `start` is the group's `seg_start`, not its position in
    /// `activity_groups`: a turn can finalize between press and release,
    /// shifting indices — toggling by position could fold the wrong turn.
    Group {
        start: usize,
        screen: u16,
    },
    OpenSubagent {
        seg: u64,
    },
    /// Fenced-block index resolved at press: the fire step only indexes the
    /// segment's source blocks, so a layout shift in between cannot retarget
    /// the copy to a row that moved under the old screen line.
    Code {
        seg: u64,
        block: usize,
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
    /// ask_user with up to 4 questions, inline in chat.
    /// This is the primary (and only) interactive surface: no overlay, no
    /// modal menu. While `answered` is `None` the agent is blocked waiting
    /// for the user; after the answer it freezes into a plain Q&A record.
    AskUser {
        #[allow(dead_code)]
        id: u64,
        questions: Vec<crate::agent::loop_task::AskQuestion>,
        // per-question picked and custom answers, edited in place
        picked: Vec<Vec<bool>>,
        custom: Vec<String>,
        focus: usize,
        /// frozen answer text once the user confirmed/skipped; None = active
        answered: Option<String>,
    },
    /// propose_plan draft awaiting accept/decline, inline in chat and folded
    /// into the turn's activity like a tool call. `decided` freezes it.
    PlanProposal {
        #[allow(dead_code)]
        id: u64,
        draft: crate::plan::Plan,
        decided: Option<bool>,
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
    },
    Status {
        text: String,
        kind: StatusKind,
        /// error rows arrive collapsed (a provider dump must not flood the
        /// chat) and unfold on click; other kinds always show fully
        expanded: bool,
    },
}

/// A finished (or still-streaming) agent turn's working content — the
/// contiguous run of `Thinking`/`Tool` segments that precedes the turn's final
/// answer. Collapsed behind one summary line; expanded on click/Enter.
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
    pub expanded: bool,
    /// Index of the user message that started this group's turn. Saved into
    /// `ActivitySummary::user_index` so a restore can re-attach summaries to
    /// the right group even after stopped/failed turns.
    pub turn_user: Option<usize>,
}

/// Click-tag offset for an activity-group header line inside `cache_rowseg`.
/// Real segment indices never reach this range, so it cleanly separates a
/// group header (toggle the whole block) from a normal segment (toggle itself).
pub(super) const GROUP_BASE: usize = 1 << 40;

/// One interactive row inside an inline AskUser segment, used for mouse
/// hover/click mapping. Headers, questions and separators are not targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum AskRow {
    Option { q: usize, opt: usize },
    Custom { q: usize },
    Confirm,
}

/// One interactive row inside an inline plan-proposal segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum ProposalRow {
    View,
    Accept,
    Decline,
}

/// Indent applied to segments nested inside an activity group. Tool rows are
/// already inset by two, so this reads as one more level under the header.
const GROUP_INDENT: u16 = 2;

fn edit_change_counts(diff: &str) -> (usize, usize) {
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

#[derive(Clone, Copy, PartialEq)]
pub(super) enum BlockKind {
    None,
    ThoughtCollapsed,
    ThoughtExpanded,
    Answer,
    /// the header line of an activity group
    Activity,
}

/// Full-width user strip. The quiet surface is the primary distinction; `›`
/// keeps it legible in terminals that flatten colors and in light themes.
fn user_box(text: &str, w: u16, hl: &Highlighter) -> Vec<Line<'static>> {
    let inner_w = w.saturating_sub(2).max(1);
    let inner = wrap_tagged(
        render(text, inner_w, hl)
            .into_iter()
            .map(|line| (line, None))
            .collect(),
        inner_w,
    )
    .0;
    let surface = Style::new().fg(Theme::FG()).bg(Theme::USER_SURFACE());
    // Codex-style marker: bold dim `›` on the band, plain text after it.
    let marker = surface.add_modifier(Modifier::BOLD | Modifier::DIM);
    let mut out = Vec::with_capacity(inner.len() + 2);
    // One blank surface line above and below keeps the message from adhering to
    // the strip's edge. `›` marks only the first line; continuation lines
    // align under it with blank space of the same width.
    out.push(Line::from(Span::styled(
        " ".repeat(usize::from(w)),
        surface,
    )));
    for (index, line) in inner.into_iter().enumerate() {
        let text = line_text(&line);
        let used = UnicodeWidthStr::width(text.as_str());
        let pad = " ".repeat(usize::from(inner_w).saturating_sub(used));
        let prefix = if index == 0 { "› " } else { "  " };
        out.push(Line::from(vec![
            Span::styled(prefix.to_string(), marker),
            Span::styled(text, surface),
            Span::styled(pad, surface),
        ]));
    }
    out.push(Line::from(Span::styled(
        " ".repeat(usize::from(w)),
        surface,
    )));
    out
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
/// decorations are removed ("    │ " tool rail, "│ " code rail, "› " user
/// marker, one trailing " │" frame cap); every other leading space — real
/// code indent included — survives, unlike a blind trim of lookalike chars.
pub(super) fn strip_row_chrome(line: &str) -> String {
    let mut s = line;
    for prefix in ["    │ ", "│ ", "› "] {
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
        // still names the pressed content. Group headers and structural
        // blanks keep the raw row (today's behavior).
        let tag = self.cache_rowseg.get(abs_r).copied().flatten();
        self.press_anchor = tag.filter(|t| *t < GROUP_BASE).and_then(|idx| {
            let (_, meta) = self.view_transcript();
            let id = meta.get(idx).map(|m| m.id)?;
            let mut start = abs_r;
            while start > 0 && self.cache_rowseg.get(start - 1) == Some(&Some(idx)) {
                start -= 1;
            }
            Some((self.active_subagent, id, abs_r - start))
        });
        self.press_target = self.resolve_target(row, abs_r);
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
        let Some(start) = self.cache_rowseg.iter().position(|t| *t == Some(idx)) else {
            return pressed;
        };
        let mut end = start;
        while end < self.cache_rowseg.len() && self.cache_rowseg[end] == Some(idx) {
            end += 1;
        }
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
    /// this frame was painted from.
    fn resolve_target(&self, screen: u16, abs: usize) -> Option<ClickTarget> {
        // Proposal/ask rows only exist in the main view; in a subagent view
        // the same absolute rows belong to a different transcript.
        if self.active_subagent.is_none() && self.menu_stack.is_empty() {
            if let Some((seg_idx, row)) = self.proposal_row_at(abs) {
                let id = self.seg_meta.get(seg_idx).map(|m| m.id)?;
                return Some(ClickTarget::Proposal { seg: id, row });
            }
            if let Some((seg_idx, row)) = self.ask_row_at(abs) {
                let id = self.seg_meta.get(seg_idx).map(|m| m.id)?;
                return Some(ClickTarget::Ask { seg: id, row });
            }
        }
        let tag = self.cache_rowseg.get(abs).copied()??;
        if tag >= GROUP_BASE {
            let gi = tag - GROUP_BASE;
            // resolve the positional index to a stable group identity now:
            // the live turn's start, or the frozen group's seg_start
            let start = if gi < self.activity_groups.len() {
                self.activity_groups[gi].seg_start
            } else {
                self.trailing_work_run().map(|run| run.0)?
            };
            return Some(ClickTarget::Group { start, screen });
        }
        let (segs, meta) = self.view_transcript();
        let id = meta.get(tag).map(|m| m.id)?;
        // code lives in assistant text only: probe the wrapped rows solely
        // for those segments (and never for tool/thinking/status rows)
        if matches!(segs.get(tag), Some(Segment::Assistant { .. }))
            && let Some(block) = self.code_block_index(id, abs)
        {
            return Some(ClickTarget::Code { seg: id, block });
        }
        match segs.get(tag) {
            Some(Segment::Subagent { .. }) => Some(ClickTarget::OpenSubagent { seg: id }),
            Some(Segment::Thinking { .. } | Segment::Tool { .. } | Segment::Status { .. }) => {
                Some(ClickTarget::Toggle { seg: id, screen })
            }
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

    pub(super) fn mouse_up(&mut self, row: u16, col: u16) {
        // subagent overview and thinking selector in the status bar
        if let Some((x0, x1)) = self.agents_click
            && row == self.status_y
            && col >= x0
            && col <= x1
            && self.menu_stack.is_empty()
        {
            self.press = None;
            self.press_anchor = None;
            self.dragging = false;
            self.sel = None;
            self.open_menu(Menu::Subagents);
            return;
        }
        if let Some((x0, x1)) = self.ef_click
            && row == self.status_y
            && col >= x0
            && col <= x1
            && self.menu_stack.is_empty()
        {
            self.press = None;
            self.press_anchor = None;
            self.dragging = false;
            self.sel = None;
            self.open_menu(Menu::Effort);
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
            let hov = self
                .popup_rows
                .iter()
                .find(|(y, _)| *y == row)
                .map(|(_, s)| s.clone());
            if hov != self.hover {
                self.hover = hov;
                self.dirty = true;
            }
            return;
        }
        // hover highlight for the inline ask (chat rows, not an overlay).
        // Outside the chat rectangle the row must not saturate onto row 0:
        // a header/tab hover would otherwise light up whatever sits on top.
        if !self.in_chat_rect(row) {
            if self.ask_hover.is_some() || self.proposal_hover.is_some() {
                self.ask_hover = None;
                self.proposal_hover = None;
                self.dirty = true;
            }
            return;
        }
        if self.menu_stack.is_empty()
            && (self.active_ask_seg().is_some() || self.active_proposal_seg().is_some())
        {
            let abs = self.abs_row(row);
            let ask_hover = self.ask_row_at(abs).map(|(_, r)| r);
            let proposal_hover = self.proposal_row_at(abs).map(|(_, r)| r);
            if ask_hover != self.ask_hover || proposal_hover != self.proposal_hover {
                self.ask_hover = ask_hover;
                self.proposal_hover = proposal_hover;
                self.dirty = true;
            }
        } else if self.ask_hover.is_some() || self.proposal_hover.is_some() {
            self.ask_hover = None;
            self.proposal_hover = None;
            self.dirty = true;
        }
    }

    /// contiguous absolute-row range of one segment in the wrapped cache
    fn ask_block_range(&self, seg_idx: usize) -> Option<(usize, usize)> {
        let start = self.cache_rowseg.iter().position(|t| *t == Some(seg_idx))?;
        let mut end = start;
        while end < self.cache_rowseg.len() && self.cache_rowseg[end] == Some(seg_idx) {
            end += 1;
        }
        Some((start, end))
    }

    /// map an absolute cache row onto an interactive AskUser target, if any
    pub(super) fn ask_row_at(&self, abs_row: usize) -> Option<(usize, AskRow)> {
        let seg_idx = self.cache_rowseg.get(abs_row).copied()??;
        if Some(seg_idx) != self.active_ask_seg() {
            return None;
        }
        let Segment::AskUser { answered: None, .. } = self.segments.get(seg_idx)? else {
            return None;
        };
        let (start, _) = self.ask_block_range(seg_idx)?;
        let offset = abs_row.saturating_sub(start);
        self.ask_decode(seg_idx, offset).map(|r| (seg_idx, r))
    }

    /// offset inside the segment's rendered block -> interactive row.
    /// Must stay in lockstep with `render_segment`'s AskUser branch: every
    /// logical line there is exactly one visual row (truncated to width).
    fn ask_decode(&self, seg_idx: usize, offset: usize) -> Option<AskRow> {
        let Segment::AskUser { questions, .. } = self.segments.get(seg_idx)? else {
            return None;
        };
        let mut line = 0usize;
        for (q_idx, q) in questions.iter().enumerate() {
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
            if q_idx + 1 < questions.len() {
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

    /// map an absolute cache row onto an interactive proposal target, if any
    pub(super) fn proposal_row_at(&self, abs_row: usize) -> Option<(usize, ProposalRow)> {
        let seg_idx = self.cache_rowseg.get(abs_row).copied()??;
        if Some(seg_idx) != self.active_proposal_seg() {
            return None;
        }
        let Segment::PlanProposal { decided: None, .. } = self.segments.get(seg_idx)? else {
            return None;
        };
        let (start, _) = self.ask_block_range(seg_idx)?;
        // layout from render_segment's PlanProposal branch: header, goal,
        // counts, view, accept, decline — one visual row each
        match abs_row.saturating_sub(start) {
            3 => Some((seg_idx, ProposalRow::View)),
            4 => Some((seg_idx, ProposalRow::Accept)),
            5 => Some((seg_idx, ProposalRow::Decline)),
            _ => None,
        }
    }

    /// Atomic helper for tests: resolve and fire in one frame, where no
    /// layout shift can intervene.
    #[cfg(test)]
    pub(super) fn click(&mut self, abs_row: usize) {
        let screen = self.last_chat.y.saturating_add(
            (abs_row.saturating_sub(self.chat_top(self.last_chat.height.max(1)))) as u16,
        );
        if let Some(target) = self.resolve_target(screen, abs_row) {
            self.fire_target(target);
        }
    }

    /// Execute a mouse-down-resolved target against the CURRENT layout.
    /// Every arm re-validates by segment id: a target whose segment vanished
    /// (or changed kind) between press and release is ignored, never fired
    /// at a stale row number.
    fn fire_target(&mut self, target: ClickTarget) {
        match target {
            ClickTarget::Proposal { seg, row } => {
                let Some(idx) = self.index_of_seg(seg) else {
                    return;
                };
                if Some(idx) != self.active_proposal_seg() {
                    return;
                }
                if !matches!(
                    self.segments.get(idx),
                    Some(Segment::PlanProposal { decided: None, .. })
                ) {
                    return;
                }
                // inline plan proposal: view opens the draft popup,
                // accept/decline answer the tool — a click must never
                // dismiss the question
                match row {
                    ProposalRow::View => self.open_proposal_preview(),
                    ProposalRow::Accept => self.proposal_answer(true),
                    ProposalRow::Decline => self.proposal_answer(false),
                }
            }
            ClickTarget::Ask { seg, row } => {
                let Some(idx) = self.index_of_seg(seg) else {
                    return;
                };
                if Some(idx) != self.active_ask_seg() {
                    return;
                }
                if !matches!(
                    self.segments.get(idx),
                    Some(Segment::AskUser { answered: None, .. })
                ) {
                    return;
                }
                // a click on an option must select it, never dismiss the
                // whole question (the old overlay did exactly that via an
                // outside-rect Esc path)
                match row {
                    AskRow::Option { q, opt } => {
                        let multiple = matches!(
                            self.segments.get(idx),
                            Some(Segment::AskUser { questions, .. })
                                if questions.get(q).is_some_and(|qq| qq.multiple)
                        );
                        if multiple {
                            self.inline_ask_toggle(q, opt);
                        } else {
                            self.inline_ask_select(q, opt);
                        }
                    }
                    AskRow::Custom { q } => {
                        self.inline_ask_focus(q);
                        self.ask_custom_focus = Some(q);
                        self.dirty = true;
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
                // like any row. A finished tool row reveals its output.
                let toggle = match self.segments.get(idx) {
                    Some(Segment::Status {
                        kind: StatusKind::Err,
                        expanded,
                        ..
                    }) => Some(!*expanded),
                    Some(Segment::Thinking { expanded, .. }) => Some(!*expanded),
                    Some(Segment::Tool {
                        ok: Some(_),
                        expanded,
                        ..
                    }) => Some(!*expanded),
                    _ => None,
                };
                if let Some(v) = toggle {
                    // anchor the header row BEFORE flipping so the block
                    // keeps its screen line instead of jumping with follow
                    self.capture_anchor_for_view(None, idx, screen);
                    // unfolding an error also copies its full text: the
                    // collapsed row is truncated, the clipboard gets all
                    if v && let Some(Segment::Status {
                        kind: StatusKind::Err,
                        text,
                        ..
                    }) = self.segments.get(idx)
                    {
                        let text = text.clone();
                        if let Ok(mut cb) = arboard::Clipboard::new()
                            && cb.set_text(text).is_ok()
                        {
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
            ClickTarget::Group { start, screen } => {
                // re-resolve by seg_start against the CURRENT groups: a stale
                // index is ignored instead of folding the wrong turn
                let gi = self
                    .activity_groups
                    .iter()
                    .position(|g| g.seg_start == start)
                    .or_else(|| {
                        (self.streaming
                            && self.trailing_work_run().is_some_and(|run| run.0 == start))
                        .then_some(self.activity_groups.len())
                    });
                let Some(gi) = gi else { return };
                if self.toggle_activity_group(gi) {
                    self.capture_anchor_for_view(None, GROUP_BASE + gi, screen);
                }
            }
            ClickTarget::OpenSubagent { seg } => {
                let Some(idx) = self.index_of_seg(seg) else {
                    return;
                };
                if let Some(Segment::Subagent { id, .. }) = self.segments.get(idx) {
                    let id = *id;
                    self.open_subagent_view(id);
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
                    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
                        Ok(()) => self.status("code copied to clipboard", StatusKind::Info),
                        Err(e) => self.status(&format!("copy failed: {e}"), StatusKind::Err),
                    }
                }
            }
        }
    }

    /// Fold or unfold one activity group. Index `activity_groups.len()` is the
    /// running turn: its state lives in `live_group_collapsed` until the turn
    /// ends and the group is frozen. Returns false (no anchor, no scroll
    /// change) when the index no longer addresses a live group.
    fn toggle_activity_group(&mut self, g: usize) -> bool {
        if g < self.activity_groups.len() {
            self.activity_groups[g].expanded = !self.activity_groups[g].expanded;
        } else if g == self.activity_groups.len() && self.streaming {
            self.live_group_collapsed = !self.live_group_collapsed;
        } else {
            // stale tag from a turn that has since ended
            return false;
        }
        self.dirty = true;
        true
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
        // anchor BEFORE mutating so the header keeps its screen line
        if let Some(tag) = self.cache_rowseg.iter().position(|t| *t == Some(idx)) {
            self.capture_anchor_for_view(Some(id), tag, screen);
        } else {
            self.pause_follow_for_inspection();
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
        let found = self
            .cache_rowseg
            .iter()
            .position(|t| *t == Some(tag))
            .is_some();
        self.pending_anchor = found.then_some((view, tag, offset));
    }

    /// Which fenced code block of a segment contains an absolute row, by
    /// walking that segment's cached wrapped rows and tracking code-frame
    /// borders (`╭` opens, `╰` closes). Tables use `┌`-corners, so a `│`
    /// heuristic can no longer mistake a table row for code.
    /// View-aware: resolves the id in the currently shown transcript.
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
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(out)) {
            Ok(()) => {}
            Err(e) => self.status(&format!("copy failed: {e}"), StatusKind::Err),
        }
        self.dirty = true;
    }

    // ---------- rendering ----------

    /// cache key for a segment's rendered content; changing it forces a repaint.
    /// Byte lengths (O(1)) rather than char counts: counting chars over the
    /// whole transcript on every streamed frame was the long-chat lag.
    pub(super) fn seg_key(&self, seg: &Segment) -> usize {
        match seg {
            Segment::User(t) => t.len(),
            Segment::Assistant { text, .. } => text.len(),
            Segment::Commentary(text) => text.len(),
            Segment::AskUser {
                questions,
                picked,
                custom,
                focus,
                answered,
                ..
            } => {
                let mut k = questions.len() * 1000;
                for (q, p) in picked.iter().enumerate() {
                    for (opt, v) in p.iter().enumerate() {
                        if *v {
                            k = k.wrapping_add(
                                ((1usize << (opt.min(60))) | 1).wrapping_mul((q + 1) * 100_003),
                            );
                        }
                    }
                }
                for c in custom {
                    k += c.len();
                }
                k += focus * 101;
                if let Some(a) = answered {
                    k += a.len() * 3 + 1_000_000;
                }
                // hover highlight is part of the painted row
                if let Some(hover) = self.ask_hover {
                    k = k.wrapping_add(match hover {
                        AskRow::Option { q, opt } => (q + 1) * 1_000_007 + (opt + 1) * 1_009,
                        AskRow::Custom { q } => (q + 1) * 2_000_033 + 7,
                        AskRow::Confirm => 3_000_037,
                    });
                }
                if self.ask_custom_focus.is_some() {
                    k = k.wrapping_add(5_000_021);
                }
                k
            }
            Segment::PlanProposal { draft, decided, .. } => {
                let mut k = draft.goal.text.len() * 3 + draft.steps.len() * 1000;
                k += draft.constraints.len() * 101 + draft.acceptance.len() * 103;
                for s in &draft.steps {
                    k += s.title.len();
                }
                if let Some(accepted) = decided {
                    k += 1_000_000 + usize::from(*accepted) * 7;
                }
                if let Some(hover) = self.proposal_hover {
                    k = k.wrapping_add(match hover {
                        ProposalRow::View => 7_000_037,
                        ProposalRow::Accept => 7_000_039,
                        ProposalRow::Decline => 7_000_043,
                    });
                }
                k
            }
            Segment::Subagent {
                id,
                task,
                status,
                output,
                expanded,
            } => id
                .wrapping_add(task.len() as u64)
                .wrapping_add(status.len() as u64)
                .wrapping_add(output.len() as u64)
                .wrapping_add(*expanded as u64) as usize,
            Segment::Thinking {
                text,
                expanded,
                live,
                started,
                ..
            } => {
                // content identity is (id, rev); the key carries paint state
                // only. Elapsed seconds advance while live — a finished block
                // must not re-render every second for a clock that froze.
                text.len() * 2
                    + *expanded as usize
                    + usize::from(*live) * 3
                    + if *live {
                        started.map(|t| t.elapsed().as_secs() as usize).unwrap_or(0)
                    } else {
                        0
                    }
            }
            Segment::Tool {
                name,
                args,
                ok,
                output,
                diff,
                expanded,
                ..
            } => {
                let mut k = name.len()
                    + args.len()
                    + output.len()
                    + diff.as_ref().map(|d| d.len() * 3).unwrap_or(0)
                    + usize::from(*expanded) * 5;
                k = match ok {
                    // running: the spinner frame is part of the key
                    None => k.wrapping_add(self.spinner_tick * 7),
                    Some(true) => k.wrapping_add(1),
                    Some(false) => k.wrapping_add(2),
                };
                k
            }
            Segment::Status {
                text,
                kind,
                expanded,
            } => {
                // content identity is (id, rev) — statuses are always pushed
                // with a fresh id — but the key must still see text and kind,
                // or two statuses would alias one cache entry. The old key
                // (`expanded` alone) repainted stale rows for same-state
                // updates whenever revisions aligned.
                text.len() * 3
                    + usize::from(*expanded)
                    + match kind {
                        StatusKind::Info => 0,
                        StatusKind::Ok => 7,
                        StatusKind::Warn => 14,
                        StatusKind::Err => 21,
                    }
            }
        }
    }

    /// Render one segment. `segs` is the owning transcript (main chat or one
    /// subagent's), `interactive` enables live ask/proposal highlight — only
    /// the main view is interactive; subagent rows always render inactive.
    pub(super) fn render_segment(
        &self,
        segs: &[Segment],
        idx: usize,
        w: u16,
        interactive: bool,
    ) -> Vec<(Line<'static>, Option<usize>)> {
        let mut out: Vec<(Line<'static>, Option<usize>)> = Vec::new();
        match &segs[idx] {
            Segment::User(text) => {
                for l in user_box(text, w, &self.hl) {
                    out.push((l, Some(idx)));
                }
            }
            Segment::Assistant { text, .. } => {
                for l in render(text, w, &self.hl) {
                    out.push((l, Some(idx)));
                }
            }
            Segment::Commentary(text) => {
                let inner_w = w.saturating_sub(2).max(1);
                for l in render(text, inner_w, &self.hl) {
                    out.push((indent_line(l, 2), Some(idx)));
                }
            }
            Segment::AskUser {
                questions,
                picked,
                custom,
                focus,
                answered,
                ..
            } => {
                let width = usize::from(w).max(1);
                let live = answered.is_none();
                let is_active_seg = interactive && self.active_ask_seg() == Some(idx);
                for (q_idx, q) in questions.iter().enumerate() {
                    let is_focused = live && is_active_seg && q_idx == *focus;
                    let header_style = if is_focused {
                        Theme::accent_bold()
                    } else {
                        Theme::dim()
                    };
                    if !q.header.is_empty() {
                        let head = if is_focused {
                            format!(" {} ●", q.header)
                        } else {
                            format!(" {} ", q.header)
                        };
                        out.push((
                            Line::from(vec![Span::styled(
                                truncate_display_width(&head, width),
                                header_style,
                            )]),
                            Some(idx),
                        ));
                    }
                    out.push((
                        Line::from(vec![Span::styled(
                            truncate_display_width(&format!(" ? {}", q.question), width),
                            if live {
                                Theme::accent_bold()
                            } else {
                                Theme::dim()
                            },
                        )]),
                        Some(idx),
                    ));
                    for (o_idx, opt) in q.options.iter().enumerate() {
                        let is_picked = picked
                            .get(q_idx)
                            .and_then(|v| v.get(o_idx).copied())
                            .unwrap_or(false);
                        let marker = if q.multiple {
                            if is_picked { " [x] " } else { " [ ] " }
                        } else if is_picked {
                            " ● "
                        } else {
                            " ○ "
                        };
                        let hovered = live
                            && is_active_seg
                            && self.ask_hover
                                == Some(AskRow::Option {
                                    q: q_idx,
                                    opt: o_idx,
                                });
                        let mut text = format!("{marker}{}. {}", o_idx + 1, opt.label);
                        if opt.recommended {
                            text.push_str(" (Recommended)");
                        }
                        if let Some(d) = &opt.description {
                            text.push_str(&format!(" — {d}"));
                        }
                        let text = truncate_display_width(&text, width);
                        let base = if !live {
                            Theme::dim()
                        } else if hovered {
                            Style::new()
                                .fg(Theme::BG())
                                .bg(Theme::ACCENT())
                                .add_modifier(Modifier::BOLD)
                        } else if is_picked || opt.recommended {
                            Theme::accent()
                        } else {
                            Theme::base()
                        };
                        // keep the leading marker quiet even on a highlighted
                        // row so the option number stays scannable
                        out.push((Line::from(vec![Span::styled(text, base)]), Some(idx)));
                    }
                    if q.allow_free {
                        let c = custom.get(q_idx).map(|s| s.as_str()).unwrap_or("");
                        let custom_focused =
                            live && is_active_seg && self.ask_custom_focus == Some(q_idx);
                        let hovered = live
                            && is_active_seg
                            && self.ask_hover == Some(AskRow::Custom { q: q_idx });
                        let raw = if c.is_empty() {
                            "  ✎ Type your own answer…".to_string()
                        } else if custom_focused {
                            format!("  ✎ {c}▌")
                        } else {
                            format!("  ✎ {c}")
                        };
                        let style = if !live {
                            Theme::dim()
                        } else if custom_focused || hovered {
                            Style::new()
                                .fg(Theme::BG())
                                .bg(Theme::ACCENT())
                                .add_modifier(Modifier::BOLD)
                        } else if !c.is_empty() {
                            Theme::accent()
                        } else {
                            Theme::dim()
                        };
                        out.push((
                            Line::from(vec![Span::styled(
                                truncate_display_width(&raw, width),
                                style,
                            )]),
                            Some(idx),
                        ));
                    }
                    if q_idx + 1 < questions.len() {
                        out.push((
                            Line::from(vec![Span::styled(
                                truncate_display_width("  ──", width),
                                Theme::dim(),
                            )]),
                            Some(idx),
                        ));
                    }
                }
                if let Some(answer) = answered {
                    out.push((
                        Line::from(vec![Span::styled(
                            truncate_display_width(&format!("  ✓ {answer}"), width),
                            Theme::ok(),
                        )]),
                        Some(idx),
                    ));
                } else {
                    let hovered = live && is_active_seg && self.ask_hover == Some(AskRow::Confirm);
                    let style = if hovered {
                        Style::new()
                            .fg(Theme::BG())
                            .bg(Theme::ACCENT())
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::new().fg(Theme::ACCENT_SOFT())
                    };
                    out.push((
                        Line::from(vec![Span::styled(
                            truncate_display_width(
                                "  confirm ⏎ · 1-5 select · tab next question",
                                width,
                            ),
                            style,
                        )]),
                        Some(idx),
                    ));
                }
            }
            Segment::PlanProposal { draft, decided, .. } => {
                let width = usize::from(w).max(1);
                let live = decided.is_none();
                let is_active_seg = interactive && self.active_proposal_seg() == Some(idx);
                out.push((
                    Line::from(vec![Span::styled(
                        truncate_display_width(" ▸ proposed plan", width),
                        if live {
                            Theme::accent_bold()
                        } else {
                            Theme::dim()
                        },
                    )]),
                    Some(idx),
                ));
                out.push((
                    Line::from(vec![Span::styled(
                        truncate_display_width(&format!(" ? {}", draft.goal.text), width),
                        if live {
                            Theme::accent_bold()
                        } else {
                            Theme::dim()
                        },
                    )]),
                    Some(idx),
                ));
                out.push((
                    Line::from(vec![Span::styled(
                        truncate_display_width(
                            &format!(
                                "   {} steps · {} acceptance · {} constraints",
                                draft.steps.len(),
                                draft.acceptance.len(),
                                draft.constraints.len()
                            ),
                            width,
                        ),
                        Theme::dim(),
                    )]),
                    Some(idx),
                ));
                if let Some(accepted) = decided {
                    let (mark, style) = if *accepted {
                        ("✓ accepted", Theme::ok())
                    } else {
                        ("✗ declined", Theme::warn())
                    };
                    out.push((
                        Line::from(vec![Span::styled(
                            truncate_display_width(&format!("  {mark}"), width),
                            style,
                        )]),
                        Some(idx),
                    ));
                } else {
                    for (n, (label, target)) in [
                        ("посмотреть план", ProposalRow::View),
                        ("принять", ProposalRow::Accept),
                        ("отклонить", ProposalRow::Decline),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        let hovered = live && is_active_seg && self.proposal_hover == Some(target);
                        let style = if hovered {
                            Style::new()
                                .fg(Theme::BG())
                                .bg(Theme::ACCENT())
                                .add_modifier(Modifier::BOLD)
                        } else if matches!(target, ProposalRow::Accept) {
                            Theme::accent()
                        } else {
                            Theme::base()
                        };
                        out.push((
                            Line::from(vec![Span::styled(
                                truncate_display_width(&format!(" ○ {}. {label}", n + 1), width),
                                style,
                            )]),
                            Some(idx),
                        ));
                    }
                }
            }
            Segment::Thinking {
                text,
                expanded,
                started,
                duration_ms,
                live,
            } => {
                let elapsed = if *live {
                    started.map_or(0u64, |t| t.elapsed().as_secs())
                } else {
                    duration_ms / 1000
                };
                if !*expanded {
                    let label = if text.is_empty() {
                        format!("  thinking… {elapsed}s")
                    } else {
                        format!("  thinking · {elapsed}s")
                    };
                    let spans = vec![Span::styled(label, Theme::dim())];
                    out.push((Line::from(spans), Some(idx)));
                } else {
                    for l in render(text, w, &self.hl) {
                        out.push((dim_all(l), Some(idx)));
                    }
                    out.push((
                        Line::from(vec![Span::styled(
                            format!("  click to collapse · {elapsed}s"),
                            Style::new().fg(Theme::DIM()).add_modifier(Modifier::ITALIC),
                        )]),
                        Some(idx),
                    ));
                }
            }
            Segment::Subagent {
                id,
                task: _,
                status,
                output,
                expanded,
            } => {
                let marker = match status.as_str() {
                    "completed" => "✓",
                    "failed" => "✗",
                    _ => "→",
                };
                out.push((
                    Line::from(vec![Span::styled(
                        format!("  {marker} subagent-{id}"),
                        if status == "failed" {
                            Theme::err()
                        } else {
                            Theme::accent()
                        },
                    )]),
                    Some(idx),
                ));
                if *expanded {
                    for line in wrap_tagged(
                        output
                            .lines()
                            .map(|l| (Line::from(l.to_string()), None))
                            .collect(),
                        w.saturating_sub(4),
                    )
                    .0
                    {
                        out.push((
                            Line::from(vec![Span::styled(format!("    {line}"), Theme::dim())]),
                            Some(idx),
                        ));
                    }
                }
            }
            Segment::Tool {
                name,
                args,
                ok,
                output,
                diff,
                preview,
                preview_total,
                expanded,
            } => {
                // Every tool uses the same three-part row: state marker, tool
                // name, and a quiet one-line argument summary. Keeping the
                // geometry identical makes running, successful, and failed
                // calls scan as one list.
                let marker = match ok {
                    None => (
                        format!(
                            "  {} ",
                            WORKING_SPINNER[self.spinner_tick % WORKING_SPINNER.len()]
                        ),
                        Theme::accent(),
                    ),
                    Some(true) => ("  ✓ ".to_string(), Theme::ok()),
                    Some(false) => ("  ✗ ".to_string(), Theme::err()),
                };
                let marker_width = 4usize;
                let name_width = name
                    .width()
                    .min(usize::from(w).saturating_sub(marker_width));
                let shown_name = truncate_display_width(name, name_width);
                let available = usize::from(w)
                    .saturating_sub(marker_width + name_width)
                    .saturating_sub(2);
                let edit_counts = if name == "edit" {
                    diff.as_deref().map(edit_change_counts)
                } else {
                    None
                };
                let counts_width = edit_counts
                    .map(|(added, removed)| format!("  +{added} -{removed}").width())
                    .unwrap_or(0);
                let summary_width = available.saturating_sub(counts_width);
                let summary = if args.is_empty() || summary_width == 0 {
                    String::new()
                } else {
                    format!("  {}", truncate_display_width(args, summary_width))
                };
                let mut head_spans = vec![
                    Span::styled(marker.0, marker.1),
                    Span::styled(shown_name, Theme::accent()),
                    Span::styled(summary, Theme::dim()),
                ];
                if let Some((added, removed)) = edit_counts {
                    head_spans.push(Span::styled(format!("  +{added}"), Theme::ok()));
                    head_spans.push(Span::styled(format!(" -{removed}"), Theme::err()));
                }
                let head = Line::from(head_spans);
                out.push((head, Some(idx)));
                if *expanded {
                    // ask_user history rows store only a one-line summary in
                    // `args` (not the JSON), so never try to parse it: show
                    // the question summary and the recorded answer instead of
                    // an empty expansion.
                    // Preview normally arrives precomputed with the output;
                    // the on-the-fly fallback only serves fixtures and legacy
                    // rows that predate it — never the live transcript.
                    let (preview, preview_total) =
                        if preview.is_empty() && (!output.is_empty() || diff.is_some()) {
                            tool_preview(diff.as_deref(), output)
                        } else {
                            (preview.clone(), *preview_total)
                        };
                    let shown: Vec<String> = if name == "ask_user" {
                        let mut s = String::new();
                        if !args.is_empty() {
                            s.push_str(&format!("Q: {args}\n"));
                        } else {
                            s.push_str("Q: (question)\n");
                        }
                        if !output.is_empty() {
                            s.push_str(&format!("A: {output}"));
                        } else {
                            s.push_str("A: (no answer yet)");
                        }
                        let shown: Vec<String> = s.lines().map(str::to_string).collect();
                        shown
                    } else {
                        // Precomputed at output-arrival time: no clone of the
                        // full body and no full line scan on every rebuild.
                        preview.clone()
                    };
                    let total = if name == "ask_user" {
                        shown.len()
                    } else {
                        preview_total
                    };
                    let border = Theme::border_dim();
                    let width = usize::from(w).saturating_sub(6).max(1);
                    // Expanded output has no surrounding box. Keep one quiet
                    // left rail so the body remains visibly attached to the
                    // tool row while every line stays within the chat width.
                    out.push((
                        Line::from(vec![Span::styled("    │".to_string(), border)]),
                        Some(idx),
                    ));
                    for l in &shown {
                        let st = if l.starts_with('+') && !l.starts_with("+++") {
                            Theme::ok()
                        } else if l.starts_with('-') && !l.starts_with("---") {
                            Theme::err()
                        } else if l.starts_with("@@") {
                            Theme::accent()
                        } else {
                            Theme::dim()
                        };
                        // single truncation: the line is already capped, do
                        // not re-truncate the truncated result
                        out.push((
                            Line::from(vec![
                                Span::styled("    │ ", border),
                                Span::styled(truncate_display_width(l, width), st),
                            ]),
                            Some(idx),
                        ));
                    }
                    if total > shown.len() {
                        let more = format!("… {} more lines", total - shown.len());
                        out.push((
                            Line::from(vec![
                                Span::styled("    │ ", border),
                                Span::styled(truncate_display_width(&more, width), Theme::dim()),
                            ]),
                            Some(idx),
                        ));
                    }
                }
            }
            Segment::Status {
                text,
                kind,
                expanded,
            } => {
                let st = match kind {
                    StatusKind::Info => Theme::dim(),
                    StatusKind::Ok => Theme::ok(),
                    StatusKind::Warn => Theme::warn(),
                    StatusKind::Err => Theme::err(),
                };
                let parts: Vec<&str> = text.split('\n').collect();
                // A provider dump must not flood the chat: multi-line errors
                // — and single lines wider than the row — arrive collapsed to
                // one width-capped line and unfold on click.
                let wide = UnicodeWidthStr::width(parts[0]) > (w as usize).saturating_sub(2);
                let collapsed =
                    matches!(kind, StatusKind::Err) && !expanded && (parts.len() > 1 || wide);
                let show: &[&str] = if collapsed { &parts[..1] } else { &parts[..] };
                for part in show {
                    // Collapsed is one width-capped line. Expanded rows pass
                    // through untouched: wrap_tagged downstream wraps them to
                    // the width, so unfolding never loses text.
                    let row = if collapsed {
                        let more = parts.len() - 1;
                        let suffix = if more > 0 {
                            format!("… ({} more)", more)
                        } else {
                            "…".to_string()
                        };
                        let head = truncate_display_width(
                            part,
                            (w as usize).saturating_sub(4 + suffix.chars().count()),
                        );
                        format!("  {head} {suffix}")
                    } else {
                        format!("  {part}")
                    };
                    out.push((Line::from(vec![Span::styled(row, st)]), Some(idx)));
                }
            }
        }
        out
    }

    pub(super) fn rebuild_cache(&mut self, width: u16) {
        // timed for the `/debug` perf log (two clock reads per rebuild —
        // noise next to any real render work)
        let t0 = std::time::Instant::now();
        // No cache wipe on width change: every entry carries its own width,
        // so only rows wrapped for the old size re-render — the rest survive.
        let w = width.saturating_sub(2).max(10); // side padding
        // chunks of the new assembly + whether each was (re)built this pass.
        // Untouched segments contribute an empty placeholder: the merge step
        // reuses their live rows without cloning them.
        let mut chunks: Vec<RowChunk> = Vec::new();
        let mut fresh: Vec<bool> = Vec::new();
        let mut struct_ord = 0u64;
        macro_rules! struct_row {
            ($line:expr, $tag:expr) => {{
                chunks.push(struct_chunk(struct_ord, $line, $tag, w));
                fresh.push(true);
                struct_ord += 1;
            }};
        }
        let mut in_group = false; // inside one "agent" turn
        let mut last_block = BlockKind::None;

        // A turn's working content is wrapped in one activity group. Finished
        // turns are frozen in `activity_groups`; the running turn is recomputed
        // here, so its header counts grow while events stream in. The running
        // turn's group sits at index `activity_groups.len()` — which is exactly
        // where finalize_activity_group will store it.
        let mut groups = self.activity_groups.clone();
        if self.streaming
            && let Some(run) = self.trailing_work_run()
        {
            let mut live = self.build_activity_group(run);
            live.expanded = !self.live_group_collapsed;
            groups.push(live);
        }
        let mut gi = 0usize; // next group waiting to be opened
        let mut hide_until = 0usize; // collapsed group: skip [seg_start, seg_end)
        let mut inside_until = 0usize; // expanded group: indent [seg_start, seg_end)

        for idx in 0..self.segments.len() {
            if gi < groups.len() && idx == groups[gi].seg_start {
                let g = &groups[gi];
                if !in_group {
                    struct_row!(blank(), None);
                    in_group = true;
                }
                last_block = BlockKind::Activity;
                // the running turn's group (if any) sits past the finished
                // ones: only its "activity" word shimmers, the rest is static
                let live = self.streaming && gi == self.activity_groups.len();
                struct_row!(
                    activity_header_line(g, live.then_some(self.spinner_tick)),
                    Some(GROUP_BASE + gi)
                );
                if g.expanded {
                    inside_until = g.seg_end;
                } else {
                    hide_until = g.seg_end;
                }
                gi += 1;
            }
            if idx < hide_until {
                continue;
            }
            let indent = if idx < inside_until {
                usize::from(GROUP_INDENT)
            } else {
                0
            };

            let seg = &self.segments[idx];
            // group spacing rules (cheap, done per assembly pass)
            match seg {
                Segment::AskUser { .. } | Segment::PlanProposal { .. } => {
                    in_group = false;
                    last_block = BlockKind::None;
                    struct_row!(blank(), None);
                }
                Segment::User(_) => {
                    in_group = false;
                    last_block = BlockKind::None;
                    struct_row!(blank(), None);
                }
                Segment::Assistant { .. } => {
                    if !in_group {
                        struct_row!(blank(), None);
                        in_group = true;
                    } else if last_block == BlockKind::ThoughtExpanded {
                        struct_row!(blank(), None);
                    }
                    last_block = BlockKind::Answer;
                }
                Segment::Commentary(_) => {
                    if !in_group {
                        struct_row!(blank(), None);
                        in_group = true;
                    }
                }
                Segment::Thinking { expanded, .. } => {
                    if !in_group {
                        struct_row!(blank(), None);
                        in_group = true;
                    } else if last_block == BlockKind::Answer {
                        struct_row!(blank(), None);
                    }
                    last_block = if *expanded {
                        BlockKind::ThoughtExpanded
                    } else {
                        BlockKind::ThoughtCollapsed
                    };
                }
                Segment::Subagent { .. } => {
                    if !in_group {
                        struct_row!(blank(), None);
                        in_group = true;
                    }
                }
                Segment::Tool { .. } => {
                    // tool rows belong to the agent's turn, keep them grouped
                    if !in_group {
                        struct_row!(blank(), None);
                        in_group = true;
                    }
                }
                Segment::Status { .. } => {}
            }

            // expensive part: reuse rendered AND wrapped lines unless the
            // segment changed. Entries are keyed by stable segment id, so an
            // append or a stream update to one segment never invalidates the
            // others; only (id, rev, width, interactive key) all matching
            // reuses the cached rows. Indent goes in before wrapping, same
            // as a whole-list pass would do (wrapping is per-line
            // independent, so chunks concatenate exactly).
            let key = self.seg_key(seg);
            let meta = self
                .seg_meta
                .get(idx)
                .copied()
                .unwrap_or(SegMeta { id: 0, rev: 0 });
            // nested rows render narrower so the indent cannot push them past
            // the chat width; the width is part of the cache check so a segment
            // that moves in or out of a group is repainted at the right size.
            let render_w = w.saturating_sub(indent as u16);
            let hit = matches!(self.seg_cache.get(&meta.id), Some(entry)
                if entry.rev == meta.rev && entry.width == render_w && entry.key == key);
            if hit {
                // live rows are reused by the merge step: no clone here
                chunks.push((AsmTag::Seg(meta.id), Vec::new()));
                fresh.push(false);
                continue;
            }
            self.test_renders += 1;
            let lines = self.render_segment(&self.segments, idx, render_w, true);
            let chunk: Vec<(Line<'static>, Option<usize>)> = lines
                .into_iter()
                .map(|(line, tag)| (indent_line(line, indent), tag))
                .collect();
            let (rows, tags) = wrap_tagged(chunk, w);
            let rows: Vec<(Line<'static>, Option<usize>)> = rows.into_iter().zip(tags).collect();
            self.seg_cache.insert(
                meta.id,
                SegCacheEntry {
                    rev: meta.rev,
                    width: render_w,
                    key,
                    rows: rows.clone(),
                },
            );
            chunks.push((AsmTag::Seg(meta.id), rows));
            fresh.push(true);
        }
        // Drop cache entries for segments that no longer exist anywhere (main
        // transcript or any open subagent chat). Ids are never reused, so a
        // surviving entry always belongs to live content.
        self.prune_seg_cache();
        self.merge_chunks(chunks, fresh);
        self.cache_w = width;
        self.last_rebuild_us = t0.elapsed().as_micros();
    }

    /// Forget wrapped rows whose segment id is gone from every transcript.
    fn prune_seg_cache(&mut self) {
        let mut live = std::collections::HashSet::new();
        for meta in &self.seg_meta {
            live.insert(meta.id);
        }
        for chat_meta in self.subagent_meta.values() {
            for meta in chat_meta {
                live.insert(meta.id);
            }
        }
        self.seg_cache.retain(|id, _| live.contains(id));
    }

    /// Cached rows for one chunk tag, for assembly paths whose fresh rows
    /// are unavailable. Segment chunks always come from the id-keyed cache;
    /// structural chunks are rebuilt every pass, so a miss means empty.
    fn chunk_rows(&self, tag: AsmTag) -> Vec<(Line<'static>, Option<usize>)> {
        match tag {
            AsmTag::Seg(id) => self
                .seg_cache
                .get(&id)
                .map(|e| e.rows.clone())
                .unwrap_or_default(),
            AsmTag::Struct(_) => Vec::new(),
        }
    }

    /// Do the live buffers already hold `rows` at `[at, at + len)`?
    /// Structural chunks (blanks, group headers) are rebuilt every pass but
    /// almost always identical — skipping the no-op splice avoids an O(tail)
    /// memmove per rebuild for rows that did not change.
    fn range_eq(&self, at: usize, len: usize, rows: &[(Line<'static>, Option<usize>)]) -> bool {
        if rows.len() != len {
            return false;
        }
        let end = at.checked_add(len);
        let (Some(old_l), Some(old_t)) = (
            end.and_then(|e| self.cache_lines.get(at..e)),
            end.and_then(|e| self.cache_rowseg.get(at..e)),
        ) else {
            return false;
        };
        old_l
            .iter()
            .zip(old_t.iter())
            .zip(rows.iter())
            .all(|((l, t), (nl, nt))| l == nl && t == nt)
    }

    /// Same chunk-tag sequence as the live buffers: splice only rebuilt
    /// chunks in place. Untouched segments cost nothing — no render, no wrap,
    /// no row clones. The caller verifies buffer consistency first, so every
    /// `[at, at + old_len)` range below is in bounds.
    fn splice_chunks(&mut self, built: Vec<RowChunk>, fresh: Vec<bool>) {
        let mut at = 0usize;
        for (i, (tag, rows)) in built.into_iter().enumerate() {
            let old_len = self.asm_lens.get(i).copied().unwrap_or(0);
            if !fresh.get(i).copied().unwrap_or(false) {
                at += old_len;
                continue;
            }
            // structural chunks (blanks, group headers) are rebuilt every
            // pass but almost always identical: skip the no-op splice and
            // its O(tail) memmove
            if matches!(tag, AsmTag::Struct(_)) && self.range_eq(at, old_len, &rows) {
                at += old_len;
                continue;
            }
            let new_len = rows.len();
            let (ls, ts): (Vec<Line>, Vec<Option<usize>>) = rows.into_iter().unzip();
            self.cache_lines.splice(at..at + old_len, ls);
            self.cache_rowseg.splice(at..at + old_len, ts);
            if let Some(lens) = self.asm_lens.get_mut(i) {
                *lens = new_len;
            }
            at += new_len;
        }
    }

    /// Concatenate every chunk into fresh buffers (group fold, mid-list
    /// insert/remove, or most chunks rebuilt e.g. after a resize).
    fn concat_chunks(&mut self, built: Vec<RowChunk>, fresh: Vec<bool>) {
        let mut tags = Vec::with_capacity(built.len());
        let mut lens = Vec::with_capacity(built.len());
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut rowseg: Vec<Option<usize>> = Vec::new();
        for (i, (tag, rows)) in built.into_iter().enumerate() {
            let rows = if fresh.get(i).copied().unwrap_or(false) {
                rows
            } else {
                self.chunk_rows(tag)
            };
            tags.push(tag);
            lens.push(rows.len());
            for (l, t) in rows {
                lines.push(l);
                rowseg.push(t);
            }
        }
        self.cache_lines = lines;
        self.cache_rowseg = rowseg;
        self.asm_tags = tags;
        self.asm_lens = lens;
    }

    /// Merge freshly built chunks into the live row buffers. An identical
    /// tag sequence splices only rebuilt chunks (hot path); an old sequence
    /// that is a strict prefix pushes the appended tail; anything else
    /// concatenates fully. Appends and stream tokens therefore never
    /// re-clone the history.
    fn merge_chunks(&mut self, built: Vec<RowChunk>, fresh: Vec<bool>) {
        debug_assert_eq!(built.len(), fresh.len());
        self.last_fresh = fresh.iter().filter(|b| **b).count();
        let new_tags: Vec<AsmTag> = built.iter().map(|(t, _)| *t).collect();
        // the chunk map must describe the live buffers exactly, or no fast
        // path is safe: fall back to full concatenation
        let total: usize = self.asm_lens.iter().sum();
        let consistent = self.asm_lens.len() == self.asm_tags.len()
            && total == self.cache_lines.len()
            && total == self.cache_rowseg.len();
        if consistent && new_tags == self.asm_tags {
            if self.last_fresh * 2 <= new_tags.len().max(1) {
                self.splice_chunks(built, fresh);
                self.last_merge = MergeKind::Splice;
            } else {
                self.concat_chunks(built, fresh);
                self.last_merge = MergeKind::Concat;
            }
            return;
        }
        if consistent
            && new_tags.len() > self.asm_tags.len()
            && new_tags[..self.asm_tags.len()] == self.asm_tags[..]
        {
            // append-only tail: push the new chunks' rows, keep the map
            // aligned. A tail chunk is normally freshly built; a non-fresh
            // one falls back to its cache entry (defensive, ids are unique).
            let tail_start = self.asm_tags.len();
            for (n, ((_, rows), tag)) in built
                .into_iter()
                .skip(tail_start)
                .zip(new_tags[tail_start..].iter())
                .enumerate()
            {
                let i = tail_start + n;
                let rows = if fresh.get(i).copied().unwrap_or(false) {
                    rows
                } else {
                    self.chunk_rows(*tag)
                };
                let len = rows.len();
                let (ls, ts): (Vec<Line>, Vec<Option<usize>>) = rows.into_iter().unzip();
                self.cache_lines.extend(ls);
                self.cache_rowseg.extend(ts);
                self.asm_tags.push(*tag);
                self.asm_lens.push(len);
            }
            self.last_merge = MergeKind::Append;
            return;
        }
        self.concat_chunks(built, fresh);
        self.last_merge = MergeKind::Concat;
    }

    /// Cheap transcript fingerprint: identities + revisions in order, group
    /// fold state, animation and hover bits, theme and width. Same input to
    /// this function always assembles the same rows, so a draw whose
    /// fingerprint matches the last one skips reassembly entirely — typing,
    /// scrolling, selection and hover never rebuild the transcript.
    /// O(segments), small integers only: no content hashing.
    fn transcript_fp(&self, width: u16, meta: &[SegMeta]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        width.hash(&mut h);
        self.theme_rev.hash(&mut h);
        for m in meta {
            m.id.hash(&mut h);
            m.rev.hash(&mut h);
        }
        self.activity_groups.len().hash(&mut h);
        for g in &self.activity_groups {
            g.expanded.hash(&mut h);
        }
        self.live_group_collapsed.hash(&mut h);
        self.spinner_tick.hash(&mut h);
        self.ask_hover.hash(&mut h);
        self.proposal_hover.hash(&mut h);
        self.ask_custom_focus.hash(&mut h);
        h.finish()
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
        let max = self.cache_lines.len().saturating_sub(h);
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
        if let Some(abs) = self.cache_rowseg.iter().position(|t| *t == Some(tag)) {
            self.view_top = abs.saturating_sub(screen).min(max);
            self.follow = false;
        }
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
        let layout = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(input_h),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
        let chat = Rect {
            x: area.x + 1,
            y: layout[0].y,
            width: area.width.saturating_sub(2),
            height: layout[0].height,
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
                self.last_fp = self.transcript_fp(chat.width, &self.seg_meta);
            }
        } else {
            let fp = self.transcript_fp(chat.width, &self.seg_meta);
            if fp != self.last_fp {
                self.rebuild_cache(chat.width);
                self.last_fp = fp;
            }
        }
        self.last_chat = chat;
        self.last_input = layout[2];
        self.apply_pending_anchor(None);

        Block::new().style(Theme::base()).render(area, buf);

        if self.startup && self.menu_stack.is_empty() {
            self.render_startup_screen(buf, chat);
        } else {
            let top = self.chat_top(chat.height);
            // User-message surface strips intentionally extend beyond the chat
            // gutter, all the way to the terminal edges. Paint these rows first;
            // the transcript below supplies the prefix and text over that fill.
            for (screen_row, abs_row) in (top..top + chat.height as usize).enumerate() {
                let is_user = self
                    .cache_rowseg
                    .get(abs_row)
                    .and_then(|tag| *tag)
                    .is_some_and(|idx| matches!(self.segments.get(idx), Some(Segment::User(_))));
                if is_user {
                    let strip = Rect {
                        x: area.x,
                        y: chat.y + screen_row as u16,
                        width: area.width,
                        height: 1,
                    };
                    Paragraph::new(" ".repeat(area.width as usize))
                        .style(Style::new().bg(Theme::USER_SURFACE()))
                        .render(strip, buf);
                }
            }
            let sel = self.sel;
            let visible: Vec<Line> = self
                .cache_lines
                .iter()
                .enumerate()
                .skip(top)
                .take(chat.height as usize)
                .map(|(abs, l)| match sel {
                    Some(s) => {
                        let (first, second) = s.ordered();
                        if abs < first.row || abs > second.row {
                            return l.clone();
                        }
                        let chars = line_text(l).chars().count();
                        if chars == 0 {
                            // empty row inside the selection: full-width highlight
                            return Line::from(vec![Span::styled(
                                " ".repeat(chat.width as usize),
                                Style::new().add_modifier(Modifier::REVERSED),
                            )]);
                        }
                        let cs = if abs == first.row {
                            first.col.min(chars)
                        } else {
                            0
                        };
                        let ce = if abs == second.row {
                            second.col.min(chars)
                        } else {
                            chars
                        };
                        apply_sel(l, cs, ce.max(cs))
                    }
                    _ => l.clone(),
                })
                .collect();
            // Lines carry their own styles. Do not apply the base background at
            // widget level: it would override USER_SURFACE on user-strip rows.
            Paragraph::new(visible).render(chat, buf);
            // The transcript widget repaints its own rectangle, so apply the
            // full-width fill again afterwards to restore the two outer gutters.
            for (screen_row, abs_row) in (top..top + chat.height as usize).enumerate() {
                let is_user = self
                    .cache_rowseg
                    .get(abs_row)
                    .and_then(|tag| *tag)
                    .is_some_and(|idx| matches!(self.segments.get(idx), Some(Segment::User(_))));
                if is_user {
                    let y = chat.y + screen_row as u16;
                    let fill = Paragraph::new(" ").style(Style::new().bg(Theme::USER_SURFACE()));
                    fill.clone().render(
                        Rect {
                            x: area.x,
                            y,
                            width: 1,
                            height: 1,
                        },
                        buf,
                    );
                    fill.render(
                        Rect {
                            x: area.x + area.width.saturating_sub(1),
                            y,
                            width: 1,
                            height: 1,
                        },
                        buf,
                    );
                }
            }
        }

        let rule = Paragraph::new(Line::from(Span::styled(
            "─".repeat(area.width as usize),
            Theme::border_dim(),
        )))
        .style(Theme::base());
        rule.clone().render(layout[1], buf);
        self.input.set_block(Self::input_block());
        // the cursor is rendered by tui-textarea; the input has no frame.
        // `› ` marks the top input row (like the user strip); the textarea
        // is shifted right so text aligns under it on every row.
        let marker_w = self.input_marker_w();
        let input_rect = Rect {
            x: layout[2].x + marker_w,
            y: layout[2].y,
            width: layout[2].width.saturating_sub(marker_w),
            height: layout[2].height,
        };
        if marker_w > 0 {
            Paragraph::new(Line::from(Span::styled(
                "› ",
                Style::new().add_modifier(Modifier::BOLD | Modifier::DIM),
            )))
            .render(
                Rect {
                    x: layout[2].x,
                    y: layout[2].y,
                    width: marker_w,
                    height: 1,
                },
                buf,
            );
        }
        self.input.render(input_rect, buf);
        rule.render(layout[3], buf);

        self.status_y = layout[4].y;
        let sb = self.status_bar(area.width);
        sb.render(layout[4], buf);

        self.draw_popup(buf, layout[2]);
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
        let fp = self.transcript_fp(chat.width, &meta);
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
        let visible: Vec<Line> = self
            .cache_lines
            .iter()
            .skip(top)
            .take(chat.height as usize)
            .cloned()
            .collect();
        Paragraph::new(visible).style(Theme::base()).render(chat, buf);
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
    /// minus activity groups (a subagent chat has no turn folding).
    /// Borrows are scoped per index so the cache insert never aliases the
    /// transcript slice it was rendered from.
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
        for idx in 0..count {
            let (mid, mrev, key) = match (self.subagent_chats.get(&id), self.subagent_meta.get(&id))
            {
                (Some(chat), Some(meta)) => match (chat.get(idx), meta.get(idx)) {
                    (Some(seg), Some(m)) => (m.id, m.rev, self.seg_key(seg)),
                    _ => continue,
                },
                _ => continue,
            };
            let hit = matches!(self.seg_cache.get(&mid), Some(entry)
                if entry.rev == mrev && entry.width == w && entry.key == key);
            if hit {
                // live rows are reused by the merge step: no clone here
                chunks.push((AsmTag::Seg(mid), Vec::new()));
                fresh.push(false);
                continue;
            }
            self.test_renders += 1;
            let lines = {
                // immutable borrow ends before the cache insert below
                let chat = self.subagent_chats.get(&id).unwrap();
                self.render_segment(chat, idx, w, false)
            };
            let (rows, tags) = wrap_tagged(lines, w);
            let rows: Vec<(Line<'static>, Option<usize>)> = rows.into_iter().zip(tags).collect();
            self.seg_cache.insert(
                mid,
                SegCacheEntry {
                    rev: mrev,
                    width: w,
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

    pub(super) fn draw_menu(&mut self, buf: &mut Buffer, area: Rect) {
        let menu = self.cur_menu().cloned();
        if menu.is_none() || area.height < 8 || area.width < 20 {
            return;
        }
        let is_form = self.is_form_menu();
        // Event handlers rebuild rows when the menu actually changes. A full
        // rebuild here would run on every frame — for /plan that means
        // re-reading and re-parsing the plan file 20 times a second. Throttle
        // the background refresh so step updates from a running agent still
        // appear, at a fraction of the cost.
        let now = std::time::Instant::now();
        let stale = self
            .menu_built_at
            .is_none_or(|t| now.duration_since(t) >= std::time::Duration::from_millis(500));
        if stale {
            self.build_menu_rows();
            self.menu_built_at = Some(now);
        }

        // Effort gets a horizontal slider card instead of the generic
        // row list (narrow terminals fall back to the list below).
        if matches!(menu, Some(Menu::Effort)) && area.width >= 56 {
            self.draw_effort_slider(buf, area);
            return;
        }
        self.effort_hits.clear();
        self.menu_footer_text = None;
        // Popup actions do not show transient green notices at the bottom.
        let footer_h = 0usize;
        let avail_inner = (area.height.saturating_sub(6)).max(3) as usize;
        let content_rows: usize = if is_form {
            self.form_fields.len() + 1
        } else {
            // scrollable window; never smaller than what fits
            self.menu_rows
                .len()
                .min(avail_inner - footer_h.min(avail_inner - 1))
        };
        let inner = if is_form {
            content_rows
        } else {
            content_rows + footer_h
        };
        let max_h = area.height.saturating_sub(4).max(4);
        let h = (inner as u16 + 2).clamp(4, max_h);
        // Width cap order matters: the 30-column minimum must not win over
        // the terminal's real width — in a 20..29-column terminal that would
        let is_graph_wide = matches!(self.cur_menu(), Some(Menu::GraphView { .. })) && area.width >= 110;
        let w = if is_graph_wide {
            110.min(area.width.saturating_sub(4)).max(30).min(area.width)
        } else {
            78.min(area.width.saturating_sub(4)).max(30).min(area.width)
        };
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(w)) / 2,
            y: area.y + (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };
        self.menu_rect = rect;

        // Sessions rows bake the pinned frame width in at build time, when
        // the rect may still be stale (0 or another menu's card). Re-resolve
        // against the real card and rebuild once — header and content can
        // never disagree on screen.
        if matches!(menu, Some(Menu::Sessions)) {
            let want = super::menus::sessions_frame_w(rect.width) as u16;
            if want != self.sessions_frame_built_w {
                self.build_menu_rows();
                self.sessions_frame_built_w = want;
            }
        }

        // Menus are modal surfaces: dim the already-rendered screen while
        // preserving its text AND colors, then paint the menu at normal
        // contrast. DIM alone only dulls foreground intensity, so painted
        // backgrounds (user band, block cursor, selections) would keep
        // glowing behind the menu: their bg is stepped down too.
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    let mut style = cell.style().add_modifier(Modifier::DIM);
                    if let Some(bg) = style.bg {
                        style.bg = Some(dim_bg(bg));
                    }
                    cell.set_style(style);
                }
            }
        }

        // Free-scroll view: the wheel moves menu_scroll on its own and the
        // selection may sit outside the window — never snap back to it
        // here (keyboard nav pulls the view via menu_follow_sel). Only
        // cap the trailing empty space.
        let list_rows = if is_form { 0 } else { content_rows };
        self.menu_visible_rows = list_rows;
        if !is_form && list_rows > 0 {
            if self.menu_scroll + list_rows > self.menu_rows.len() {
                self.menu_scroll = self.menu_rows.len().saturating_sub(list_rows);
            }
        }

        let mut rows: Vec<Line> = Vec::new();
        let mut focused_field_rect: Option<(usize, Rect)> = None;
        if is_form {
            // column layout: " {label:>w$} : " then the value, with the
            // column sized to the longest label so long setting names
            // never collide with the value
            let label_w = self.form_label_w();
            let inner_w = (label_w as usize).saturating_sub(4);
            for (n, field) in self.form_fields.iter().enumerate() {
                let focused = n == self.form_focus;
                let lstyle = if focused {
                    Theme::accent()
                } else {
                    Theme::dim()
                };
                let prefix = Span::styled(format!(" {:>inner_w$} : ", field.label()), lstyle);
                let row_y = rect.y + 1 + n as u16;
                match field {
                    FormField::Text { ta, .. } => {
                        if focused {
                            // value + live cursor are drawn later by
                            // rendering the textarea itself over this row
                            focused_field_rect = Some((
                                n,
                                Rect {
                                    x: rect.x + 1 + label_w,
                                    y: row_y,
                                    width: rect.width.saturating_sub(label_w + 2),
                                    height: 1,
                                },
                            ));
                            rows.push(Line::from(prefix));
                        } else {
                            rows.push(Line::from(vec![
                                prefix,
                                Span::styled(ta.lines().join(""), Theme::base()),
                            ]));
                        }
                    }
                    FormField::Choice { options, sel, .. } => {
                        let vstyle = if focused {
                            Style::new().fg(Theme::FG()).bg(Theme::SURFACE())
                        } else {
                            Theme::base()
                        };
                        let val = options.get(*sel).copied().unwrap_or("");
                        rows.push(Line::from(vec![
                            prefix,
                            Span::styled(format!("‹{val}›"), vstyle),
                        ]));
                    }
                }
            }
        } else if matches!(self.cur_menu(), Some(Menu::TestAnims)) {
            // gallery: live frame + name per row, list-style selection.
            // slowed 4x (one frame per 200ms) so each animation is examinable.
            let tick = self.spinner_tick / 4;
            for (n, _) in self
                .menu_rows
                .iter()
                .skip(self.menu_scroll)
                .take(content_rows)
                .enumerate()
            {
                let abs = self.menu_scroll + n;
                let entry =
                    crate::tui::spinners::ALL.get(abs % crate::tui::spinners::ALL.len());
                let mut spans = match entry {
                    Some(e) if e.name == "shimmer-live" => {
                        let mut v = crate::tui::shimmer::shimmer_spans("Working", tick);
                        v.push(Span::styled("  shimmer-live".to_string(), Theme::dim()));
                        v
                    }
                    Some(e) => vec![
                        Span::styled(
                            format!(
                                "{}  ",
                                truncate_display_width(crate::tui::spinners::frame(e, tick), 24)
                            ),
                            Theme::accent(),
                        ),
                        Span::styled(e.name.to_string(), Theme::base()),
                    ],
                    None => vec![Span::styled(String::new(), Theme::base())],
                };
                if abs == self.menu_sel {
                    spans = spans
                        .into_iter()
                        .map(|s| Span::styled(s.content.to_string(), Theme::accent_bold()))
                        .collect();
                }
                rows.push(Line::from(spans));
            }
        } else {
            // render only the visible window of the list
            for (n, (line, _)) in self
                .menu_rows
                .iter()
                .skip(self.menu_scroll)
                .take(content_rows)
                .enumerate()
            {
                let abs = self.menu_scroll + n;
                if abs == self.menu_sel {
                    // Codex-style selection: the whole row goes cyan bold,
                    // no inverted block.
                    rows.push(Line::from(
                        line.spans
                            .iter()
                            .map(|s| {
                                Span::styled(s.content.to_string(), Theme::accent_bold())
                            })
                            .collect::<Vec<_>>(),
                    ));
                } else {
                    rows.push(line.clone());
                }
            }
        }

        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Theme::border_popup())
            .title(Span::styled(
                format!(" {} ", self.menu_title()),
                Style::new()
                    .fg(ratatui::style::Color::White)
                    .add_modifier(Modifier::BOLD),
            ));
        if matches!(self.cur_menu(), Some(Menu::Sessions)) {
            block = block.title_bottom(
                Line::from(Span::styled(" p: pin · d: delete ", Theme::dim())).right_aligned(),
            );
        } else if matches!(self.cur_menu(), Some(Menu::TestAnims)) {
            block = block.title_bottom(
                Line::from(Span::styled(" enter/esc: close ", Theme::dim())).right_aligned(),
            );
        } else if matches!(
            self.cur_menu(),
            Some(
                Menu::EditProvider { .. }
                    | Menu::EditModel { .. }
                    | Menu::EditSessionTitle { .. }
                    | Menu::EditScalar(..)
                    | Menu::AddListItem(..)
                    | Menu::EditMcpServer { .. }
                    | Menu::EditLspServer { .. }
            )
        ) {
            block = block.title_bottom(
                Line::from(Span::styled(" enter: save · esc: cancel ", Theme::dim()))
                    .right_aligned(),
            );
        }

        if is_graph_wide {
            let left_w = 54.min(rect.width.saturating_sub(30));
            let list_rect = Rect {
                x: rect.x,
                y: rect.y,
                width: left_w,
                height: rect.height,
            };
            let details_rect = Rect {
                x: rect.x + left_w,
                y: rect.y,
                width: rect.width.saturating_sub(left_w),
                height: rect.height,
            };

            Clear.render(list_rect, buf);
            Paragraph::new(rows).style(Theme::base()).block(block).render(list_rect, buf);

            Clear.render(details_rect, buf);
            let (focus_key, trail) = if let Some(Menu::GraphView { focus_key, trail, .. }) = self.cur_menu() {
                (focus_key.clone(), trail.clone())
            } else {
                (String::new(), Vec::new())
            };
            let inspected_key = self
                .menu_rows
                .get(self.menu_sel)
                .and_then(|(_, action)| match action {
                    MenuAction::GraphFocus(k) => Some(k.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| focus_key.clone());

            use crate::agent::graph::GraphStore;
            let store = crate::agent::graph::SqliteGraphStore::open(&self.project_root).ok();
            let node = store.as_ref().and_then(|s| s.find_node(&inspected_key).ok().flatten());

            let mut detail_lines: Vec<Line> = Vec::new();
            let badge = node
                .as_ref()
                .map(|n| crate::tui::app::menus::node_badge(&n.kind))
                .unwrap_or("[?]");
            let name_or_key = node
                .as_ref()
                .and_then(|n| n.name.as_deref())
                .unwrap_or(&inspected_key);

            detail_lines.push(Line::from(vec![
                Span::styled(format!(" {badge} "), Theme::accent_bold()),
                Span::styled(name_or_key.to_string(), Theme::base().add_modifier(ratatui::style::Modifier::BOLD)),
            ]));

            detail_lines.push(Line::from(vec![
                Span::styled(" Key: ", Theme::dim()),
                Span::styled(inspected_key.clone(), Theme::base()),
            ]));

            if let Some(node) = &node {
                if let Some(p) = &node.path {
                    detail_lines.push(Line::from(vec![
                        Span::styled(" Path: ", Theme::dim()),
                        Span::styled(p.clone(), Theme::base()),
                    ]));
                }
                if let (Some(s), Some(e)) = (node.line_start, node.line_end) {
                    detail_lines.push(Line::from(vec![
                        Span::styled(" Lines: ", Theme::dim()),
                        Span::styled(format!("{s}..{e}"), Theme::base()),
                    ]));
                }
                if let Some(sig) = &node.signature {
                    detail_lines.push(Line::from(vec![
                        Span::styled(" Sig: ", Theme::dim()),
                        Span::styled(sig.clone(), Theme::accent()),
                    ]));
                }
                if let Some(author) = node.properties.get("author").and_then(|v| v.as_str()) {
                    detail_lines.push(Line::from(vec![
                        Span::styled(" Author: ", Theme::dim()),
                        Span::styled(author.to_string(), Theme::base()),
                    ]));
                }
                if let Some(jref) = node.properties.get("journal_ref").and_then(|v| v.as_str()) {
                    detail_lines.push(Line::from(vec![
                        Span::styled(" Ref: ", Theme::dim()),
                        Span::styled(jref.to_string(), Theme::base()),
                    ]));
                }
                if let Some(text) = node.properties.get("text").and_then(|v| v.as_str()) {
                    detail_lines.push(Line::from(""));
                    for l in text.lines().take(10) {
                        detail_lines.push(Line::from(Span::styled(format!(" {l}"), Theme::base())));
                    }
                }
            }

            if !trail.is_empty() {
                detail_lines.push(Line::from(""));
                detail_lines.push(Line::from(vec![
                    Span::styled(" Trail: ", Theme::dim()),
                    Span::styled(trail.join(" -> "), Theme::dim()),
                ]));
            }

            let details_block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(Theme::border_popup())
                .title(Span::styled(" Node Details ", Theme::dim()));

            Paragraph::new(detail_lines)
                .style(Theme::base())
                .block(details_block)
                .wrap(ratatui::widgets::Wrap { trim: true })
                .render(details_rect, buf);
        } else {
            Clear.render(rect, buf);
            Paragraph::new(rows).style(Theme::base()).block(block).render(rect, buf);
        }
        // mini scrollbar inside the right border when the list overflows —
        // exactly like the command popup (last content column, border intact)
        if !is_form && !is_graph_wide && !self.menu_rows.is_empty() {
            let total = self.menu_rows.len();
            let shown = total
                .saturating_sub(self.menu_scroll)
                .min(content_rows)
                .max(1);
            let max_scroll = total.saturating_sub(content_rows);
            if max_scroll > 0 {
                let thumb = 1.max(shown * shown / total);
                let pos = self.menu_scroll * (shown - thumb) / max_scroll.max(1);
                let bx = rect.right().saturating_sub(2);
                for i in 0..shown {
                    if let Some(cell) = buf
                        .cell_mut(ratatui::layout::Position::new(bx, rect.y + 1 + i as u16))
                        && i >= pos
                        && i < pos + thumb
                        // never punch through drawn frames (pinned rails,
                        // table borders): the thumb yields, the frame wins
                        && !matches!(
                            cell.symbol(),
                            "│" | "─" | "┌" | "┐" | "└" | "┘" | "├" | "┤" | "┬" | "┴" | "┼"
                        )
                    {
                        cell.set_symbol("▐")
                            .set_style(Style::new().fg(Theme::rule_color()));
                    }
                }
            }
        }

        // draw the focused text field as a real textarea: same block cursor
        // and editing behavior as the message input
        if let Some((idx, field_rect)) = focused_field_rect
            && let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(idx)
        {
            ta.as_ref().render(field_rect, buf);
        }
    }

    /// Horizontal effort slider card: level names above, ○/● dots on a
    /// track below (off/gray on the left, max/magenta on the right). The
    /// filled part of the track takes the selected level's color; the rest
    /// stays dim. Arrows preview, click previews, Enter commits.
    fn draw_effort_slider(&mut self, buf: &mut Buffer, area: Rect) {
        use crate::config::EffortLevel;
        const COL_W: u16 = 8;
        let n = EffortLevel::SELECTABLE.len();
        let sel = self.menu_sel.min(n.saturating_sub(1));
        let active = EffortLevel::SELECTABLE[sel];
        let active_color = Theme::effort_color(active);
        let active_style = Theme::effort(active);

        // card geometry: 1 pad + n columns; labels + track + one air row
        let inner_w = 1 + COL_W * n as u16;
        let w = (inner_w + 2).clamp(30, area.width.saturating_sub(4).max(30));
        let h: u16 = 5;
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(w)) / 2,
            y: area.y + (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h.min(area.height.saturating_sub(2).max(4)),
        };
        self.menu_rect = rect;
        self.effort_hits.clear();

        // modal dim, same as generic menus (text and colors preserved)
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    let style = cell.style();
                    cell.set_style(style.add_modifier(Modifier::DIM));
                }
            }
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Theme::border_popup())
            .title(Span::styled(
                format!(" {} ", self.menu_title()),
                Style::new()
                    .fg(ratatui::style::Color::White)
                    .add_modifier(Modifier::BOLD),
            ))
            .title_bottom(
                Line::from(Span::styled(
                    " ← → move · click · enter ",
                    Theme::dim(),
                ))
                .right_aligned(),
            );
        Clear.render(rect, buf);
        let inner = Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(2),
        };
        block.render(rect, buf);
        if inner.height < 3 || inner.width < inner_w {
            return;
        }

        let labels_y = inner.y;
        let track_y = inner.y + 1;
        let base_x = inner.x + 1;

        // labels row + hit rects (label cell and dot cell share one target)
        for (i, lvl) in EffortLevel::SELECTABLE.iter().enumerate() {
            let name = lvl.as_str();
            let col_x = base_x + COL_W * i as u16;
            let name_w = name.len() as u16;
            let pad = COL_W.saturating_sub(name_w) / 2;
            let style = if i == sel {
                active_style
            } else {
                Theme::dim()
            };
            Paragraph::new(Line::from(vec![
                Span::styled(" ".repeat(pad as usize), Theme::base()),
                Span::styled(name.to_string(), style),
            ]))
            .render(
                Rect {
                    x: col_x,
                    y: labels_y,
                    width: COL_W,
                    height: 1,
                },
                buf,
            );
            self.effort_hits.push((
                Rect {
                    x: col_x,
                    y: labels_y,
                    width: COL_W,
                    height: 2,
                },
                i,
            ));
        }

        // track row: dots at column centers, connectors between them.
        // Built cell by cell, then compressed into style runs, so wide
        // glyphs or multibyte dashes can never misalign the dots.
        let total = COL_W as usize * n;
        let dot_off = (COL_W / 2) as usize;
        let dim = Theme::dim();
        let fill = Style::new().fg(active_color);
        // each cell: (glyph, style)
        let mut cells: Vec<(&str, Style)> = vec![(" ", Theme::base()); total];
        for i in 0..n {
            let dx = i * COL_W as usize + dot_off;
            // progress dots: every level up to the selection is filled,
            // the selection itself additionally bold; the rest are hollow
            let (dot, dot_style) = if i == sel {
                ("●", active_style)
            } else if i < sel {
                ("●", fill)
            } else {
                ("○", dim)
            };
            cells[dx] = (dot, dot_style);
            // connector to the next dot: filled iff fully left of selection
            if i + 1 < n {
                let cstyle = if i + 1 <= sel { fill } else { dim };
                for c in cells.iter_mut().take((i + 1) * COL_W as usize + dot_off).skip(dx + 1) {
                    *c = ("─", cstyle);
                }
            }
        }
        let mut spans: Vec<Span> = Vec::new();
        for (glyph, style) in cells {
            let same = spans.last().map(|s: &Span| s.style == style).unwrap_or(false);
            if same {
                let last: &mut Span = spans.last_mut().expect("nonempty");
                last.content.to_mut().push_str(glyph);
            } else {
                spans.push(Span::styled(glyph.to_string(), style));
            }
        }
        Paragraph::new(Line::from(spans)).render(
            Rect {
                x: base_x,
                y: track_y,
                width: inner.width.saturating_sub(1),
                height: 1,
            },
            buf,
        );
    }

    pub(super) fn draw_popup(&mut self, buf: &mut Buffer, input_area: Rect) {
        if !self.popup_visible() {
            self.popup_rows.clear();
            return;
        }
        let items = self.popup_items();
        if items.is_empty() {
            self.popup_rows.clear();
            return;
        }
        let shown = items
            .len()
            .min(POPUP_MAX_ROWS)
            .min((input_area.y as usize).saturating_sub(2).max(3));
        let max_scroll = items.len().saturating_sub(shown);
        let skip = self.popup_scroll.min(max_scroll);
        let h = (shown + 2) as u16; // + borders
        let w = 64.min(input_area.width.saturating_sub(2)).max(24);
        let y = input_area.y.saturating_sub(h);
        let rect = Rect {
            x: input_area.x,
            y,
            width: w,
            height: h,
        };

        // This is an inline command completion list, not a modal popup:
        // it must not dim or otherwise alter the underlying chat.
        let mut rows: Vec<Line> = Vec::new();
        self.popup_rows.clear();
        for (n, item) in items.iter().skip(skip).take(shown).enumerate() {
            let hovered = self.hover.as_deref() == Some(item.as_str());
            let cmd_style = if hovered {
                Theme::accent_bold()
            } else {
                Theme::base()
            };
            let pad = 1usize;
            rows.push(Line::from(vec![Span::styled(
                format!(" {item}{}", " ".repeat(pad)),
                cmd_style,
            )]));
            self.popup_rows.push((rect.y + 1 + n as u16, item.clone()));
        }

        Clear.render(rect, buf);
        Paragraph::new(rows).style(Theme::base()).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(Theme::border_dim()),
        ).render(rect, buf);
        // mini scrollbar inside the right border when the list overflows
        // (on the last content column, so the border stays intact)
        if max_scroll > 0 {
            let track = shown;
            let thumb = 1.max(track * shown / items.len());
            let pos = skip * (track - thumb) / max_scroll.max(1);
            let bx = rect.right().saturating_sub(2);
            for i in 0..track {
                if let Some(cell) = buf
                    .cell_mut(ratatui::layout::Position::new(bx, rect.y + 1 + i as u16))
                    && i >= pos
                    && i < pos + thumb
                {
                    cell.set_symbol("▐")
                        .set_style(Style::new().fg(Theme::rule_color()));
                }
            }
        }
    }

    #[allow(dead_code)]
    pub(super) fn header_line(&self) -> Paragraph<'static> {
        if self.segments.is_empty() {
            return Paragraph::new(Line::from(vec![
                Span::styled(" sqwai", Theme::accent_bold()),
                Span::styled(format!(" · v{}", env!("CARGO_PKG_VERSION")), Theme::dim()),
            ]))
            .style(Theme::base());
        }
        let context_used = self.session.context_tokens_used();
        let pct = self.session.context_percent() as u64;
        let tok = fmt_k(context_used);
        let mut spans = vec![
            Span::styled(" sqwai", Theme::accent_bold()),
            Span::styled(
                format!(" · {}", truncate_chars(&self.session.title, 30)),
                Theme::dim(),
            ),
            // last activity, same format as the sessions menu
            Span::styled(
                format!(" {}", fmt_date(self.session.last_activity())),
                Theme::dim(),
            ),
            Span::styled(
                format!(" · {}@{}", self.model_cfg.id, self.model_cfg.provider),
                Style::new().fg(Theme::ACCENT_SOFT()),
            ),
            Span::styled(format!(" · {tok} tok / {pct}% ctx"), Theme::accent()),
            // cumulative session total — billing/statistics only, kept separate
            // from the live context meter above (don't mix the two sizes)
            Span::styled(
                format!(" · Σ{} tok", fmt_k(self.session.cumulative_tokens())),
                Theme::dim(),
            ),
        ];
        // Only claim prompt-cache savings once the provider has actually
        // reported cached tokens. A documented cache key we never see a hit
        // for stays unverified, so it is not advertised.
        if self.session.cache_confirmed
            && let Some(c) = self.session.usage.cached_tokens
        {
            let cp = c
                .saturating_mul(100)
                .min(context_used)
                .checked_div(context_used)
                .unwrap_or(0);
            spans.push(Span::styled(format!(" · cache {cp}%"), Theme::dim()));
        }
        // cost meter: enabled later from the settings menu ([ui] show_cost)
        if self.cfg.ui.show_cost
            && let (Some(pi), Some(po)) = (self.model_cfg.price_in, self.model_cfg.price_out)
        {
            let cost = self.session.usage.prompt_tokens as f64 * pi / 1e6
                + self.session.usage.completion_tokens as f64 * po / 1e6;
            spans.push(Span::styled(format!(" · ${cost:.2}"), Theme::accent()));
        }
        Paragraph::new(Line::from(spans)).style(Theme::base())
    }

    pub(super) fn status_bar(&mut self, w: u16) -> Paragraph<'static> {
        let spans = self.status_bar_spans(w);
        Paragraph::new(Line::from(spans)).style(Theme::base())
    }

    pub(super) fn status_bar_spans(&mut self, w: u16) -> Vec<Span<'static>> {
        // a live retry overrides everything else on the left side, then the
        // 3s toast (any notice, errors included), then the checkpoint hint
        let plan_label = self.plan_step_label.clone();
        let (activity, activity_style) = if let Some(line) = &self.retry_line {
            (format!(" {line}"), Theme::warn())
        } else if let Some((text, kind)) = self.live_toast() {
            let st = match kind {
                StatusKind::Info => Theme::dim(),
                StatusKind::Ok => Theme::ok(),
                StatusKind::Warn => Theme::warn(),
                StatusKind::Err => Theme::err(),
            };
            (format!(" {}", truncate_chars(&text, 60)), st)
        } else {
            match &self.last_checkpoint {
                // reassurance that the undo insurance exists
                Some(cp) => (
                    format!(" checkpoint: {}", truncate_chars(cp, 44)),
                    Theme::dim(),
                ),
                None => (String::new(), Theme::dim()),
            }
        };
        let dir = self.cwd_label.clone();

        let context_used = self.session.context_tokens_used();
        let ctx_pct = (self.session.context_percent() as u64).min(100);
        let tok_str = fmt_k(context_used);

        let cached_tokens = self
            .session
            .last_usage
            .and_then(|u| u.cached_tokens)
            .or(self.session.usage.cached_tokens)
            .unwrap_or(0);
        let cp = cached_tokens
            .saturating_mul(100)
            .checked_div(context_used)
            .unwrap_or(0)
            .min(100);
        let mut ctx_metrics_label = format!(" cache {cp}% · {ctx_pct}% · {tok_str} ·");

        let model_label = format!(" {} ", self.model_cfg.id);
        // reports the effective mapping, not the raw selection (§5.1)
        let effort_plan = self.effort_plan();
        let ef_label = format!(" {} ", effort_plan.short_label());
        let running = self
            .subagents
            .iter()
            .filter(|(_, _, status, _, _)| status == "running")
            .count();
        let waiting = self
            .subagents
            .iter()
            .filter(|(_, _, status, _, _)| status == "waiting")
            .count();
        let failed = self
            .subagents
            .iter()
            .filter(|(_, _, status, _, _)| status == "failed")
            .count();
        let agents_label = if self.subagents.is_empty() {
            String::new()
        } else if running + waiting > 0 {
            format!(" agents:{running}/{} ", self.subagents.len())
        } else if failed > 0 {
            format!(" agents:{} · {failed} failed ", self.subagents.len())
        } else {
            format!(" agents:{} ", self.subagents.len())
        };
        self.ef_click = None;
        self.agents_click = None;

        // right side: [agents] [ctx metrics] [model] [working] [folder] [th:level] [MODE chip]
        let lsp_label = if self.lsp_diagnostics > 0 {
            format!(" LSP:{} ", self.lsp_diagnostics)
        } else {
            String::new()
        };
        // The status bar is laid out in terminal columns, so every measurement
        // here is a column count. `chars().count()` undercounts any wide glyph
        // — a project directory such as `~/仕事/proj` made the padding too
        // wide, pushed the right-hand group past the edge and shifted every
        // click target computed below.
        let left = format!(" {}  {}", self.mode.label(), plan_label);
        let left_base_cols = cols(&left);

        let mut fixed_len: usize = 1
            + cols(&agents_label)
            + cols(&ctx_metrics_label)
            + cols(&model_label)
            + cols(&ef_label)
            + cols(&lsp_label); // mode chip always present

        if left_base_cols + fixed_len > w as usize && !ctx_metrics_label.is_empty() {
            ctx_metrics_label.clear();
            fixed_len = 1
                + cols(&agents_label)
                + cols(&model_label)
                + cols(&ef_label)
                + cols(&lsp_label);
        }

        let avail_for_act = (w as usize).saturating_sub(left_base_cols + fixed_len);
        let activity = if avail_for_act < 8 {
            String::new()
        } else if cols(&activity) > avail_for_act {
            truncate_display_width(&activity, avail_for_act)
        } else {
            activity
        };

        let lw = (left_base_cols + cols(&activity)) as u16;

        // Everything except the directory, which is the least important item
        // and therefore the one that yields when the row is too narrow. `pad`
        // below saturates at zero, so without this the row simply grew past
        // the terminal: at width 70 a directory named `仕事プロジェクト`
        // produced a 72-column status bar, and the click targets derived from
        // these same numbers landed outside the row.
        let dir_budget = (w as usize)
            .saturating_sub(lw as usize + fixed_len)
            .min(DIR_MAX_COLS);
        let dir_label = if dir.is_empty() || dir_budget == 0 {
            String::new()
        } else {
            // one trailing space, so the label itself gets one column less
            format!("{} ", truncate_display_width(&dir, dir_budget - 1))
        };
        let right_len = fixed_len + cols(&dir_label);
        let mut spans = vec![Span::styled(
            format!(" {} ", self.mode.label()),
            Theme::status_chip(),
        )];
        if !plan_label.is_empty() {
            spans.push(Span::styled(format!("  {plan_label}"), Theme::dim()));
        }
        spans.push(Span::styled(activity, activity_style));
        let pad = (w as usize).saturating_sub(lw as usize + right_len);
        let agents_x0 = lw + pad as u16;
        spans.push(Span::styled(" ".repeat(pad), Theme::base()));
        if !agents_label.is_empty() {
            let agents_style = if failed > 0 {
                Theme::err()
            } else if running > 0 {
                Theme::accent()
            } else {
                Theme::dim()
            };
            spans.push(Span::styled(agents_label.clone(), agents_style));
            self.agents_click = Some((agents_x0, agents_x0 + cols(&agents_label) as u16));
        }
        spans.push(Span::styled(ctx_metrics_label.clone(), Theme::dim()));
        let model_x0 = agents_x0 + cols(&agents_label) as u16 + cols(&ctx_metrics_label) as u16;
        spans.push(Span::styled(model_label, Theme::dim()));
        // click targets are measured from the same numbers, so `ef_x0`
        // accounts for the model group width.
        let ef_x0 = model_x0 + cols(&self.model_cfg.id) as u16 + 2;
        let ef_style = if self.model_cfg.effort == EffortLevel::Off || !effort_plan.is_honoured() {
            // a level the model will not act on must not be lit up as if it
            // were doing work
            Theme::dim()
        } else {
            Theme::effort(self.model_cfg.effort)
        };
        spans.push(Span::styled(ef_label.clone(), ef_style));
        self.ef_click = Some((ef_x0, ef_x0 + cols(&ef_label) as u16));
        if !lsp_label.is_empty() {
            spans.push(Span::styled(lsp_label, Theme::warn()));
        }
        if !dir_label.is_empty() {
            spans.push(Span::styled(dir_label, Theme::dim()));
        }
        spans
    }

    pub(super) fn render_startup_screen(&self, buf: &mut Buffer, chat: Rect) {
        if chat.width < 20 || chat.height < 4 {
            return;
        }
        let fallback_data;
        let data = if let Some(d) = &self.startup_data {
            d
        } else {
            // data is collected on a background thread (refresh_startup_data);
            // never collect here — this runs every frame while waiting
            fallback_data = StartupData {
                version: env!("CARGO_PKG_VERSION"),
                project_path: shorten_path(&self.project_root),
                git_branch: None,
                git_modified: None,
                model: self.model_cfg.id.clone(),
                active_plan: None,
                last_session: None,
                memory: MemoryInfo::default(),
                recent: Vec::new(),
                warnings: Vec::new(),
                has_sqwai_dir: self.project_root.join(".sqwai").exists(),
            };
            &fallback_data
        };

        let is_narrow = chat.width < 80;
        let mut raw_lines: Vec<Line<'static>> = Vec::new();

        // 1. Identification line
        let mut id_spans = Vec::new();
        id_spans.push(Span::styled(
            format!("sqwai {}", data.version),
            Theme::base(),
        ));
        id_spans.push(Span::styled(" · ", Theme::dim()));
        id_spans.push(Span::styled(data.project_path.clone(), Theme::base()));
        id_spans.push(Span::styled(" · ", Theme::dim()));
        if let Some(branch) = &data.git_branch {
            id_spans.push(Span::styled(branch.clone(), Theme::base()));
            id_spans.push(Span::styled(" ", Theme::base()));
            match data.git_modified {
                Some(0) => {
                    let label = if is_narrow { "✓" } else { "✓ clean" };
                    id_spans.push(Span::styled(label, Theme::ok()));
                }
                Some(n) => {
                    let label = if is_narrow {
                        format!("● {n}")
                    } else {
                        format!("● {n} modified")
                    };
                    id_spans.push(Span::styled(label, Theme::warn()));
                }
                None => {}
            }
        } else {
            id_spans.push(Span::styled("no git", Theme::dim()));
        }
        if !is_narrow {
            id_spans.push(Span::styled(" · ", Theme::dim()));
            id_spans.push(Span::styled(data.model.clone(), Theme::base()));
        }
        raw_lines.push(Line::from(id_spans));
        raw_lines.push(Line::default()); // blank line

        // 2. State section
        if let Some(plan) = &data.active_plan {
            raw_lines.push(Line::from(vec![
                Span::styled("▸ active plan   ", Theme::accent()),
                Span::styled(
                    format!("\"{}\"", plan.title),
                    Theme::base().add_modifier(Modifier::BOLD),
                ),
            ]));
            raw_lines.push(Line::from(vec![Span::styled(
                format!(
                    "  step {}/{} {}",
                    plan.current_step, plan.total_steps, plan.status_text
                ),
                Theme::dim(),
            )]));
            if let Some(ls) = &data.last_session {
                let prefix = if is_narrow {
                    "  last    "
                } else {
                    "  last session  "
                };
                raw_lines.push(Line::from(vec![
                    Span::styled(prefix, Theme::dim()),
                    Span::styled(format!("{} · {}", ls.date, ls.outcome), Theme::dim()),
                ]));
            }

            // Memory line
            let mut mem_parts = Vec::new();
            if data.memory.has_memory_md {
                mem_parts.push("MEMORY.md".to_string());
            }
            if let Some(diary) = &data.memory.latest_diary {
                mem_parts.push(format!("diary {diary}"));
            }
            if data.memory.graph_ready {
                mem_parts.push("graph ready".to_string());
            }
            if !mem_parts.is_empty() {
                let prefix = if is_narrow {
                    "  memory  "
                } else {
                    "  memory        "
                };
                raw_lines.push(Line::from(vec![
                    Span::styled(prefix, Theme::dim()),
                    Span::styled(mem_parts.join(" · "), Theme::dim()),
                ]));
            }
        } else if data.has_sqwai_dir {
            // No active plan, but has .sqwai history
            if is_narrow {
                raw_lines.push(Line::from(vec![Span::styled(
                    "no active plan",
                    Theme::dim(),
                )]));
            } else {
                let mut spans = vec![Span::styled("no active plan", Theme::dim())];
                if let Some(ls) = &data.last_session {
                    spans.push(Span::styled(
                        format!(" · last session {}", ls.date),
                        Theme::dim(),
                    ));
                }
                raw_lines.push(Line::from(spans));
            }

            // Memory line
            let mut mem_parts = Vec::new();
            if data.memory.has_memory_md {
                mem_parts.push("MEMORY.md".to_string());
            }
            if let Some(diary) = &data.memory.latest_diary {
                mem_parts.push(format!("diary {diary}"));
            }
            if data.memory.graph_ready {
                mem_parts.push("graph ready".to_string());
            }
            if !mem_parts.is_empty() {
                let prefix = if is_narrow {
                    "memory  "
                } else {
                    "memory        "
                };
                raw_lines.push(Line::from(vec![
                    Span::styled(prefix, Theme::dim()),
                    Span::styled(mem_parts.join(" · "), Theme::dim()),
                ]));
            }
        } else {
            // First run: no .sqwai
            raw_lines.push(Line::from(vec![Span::styled(
                "this project has no .sqwai yet",
                Theme::dim(),
            )]));
            raw_lines.push(Line::from(vec![
                Span::styled("/init", Theme::base()),
                Span::styled(
                    " create AGENTS.md and memory or just type a task",
                    Theme::dim(),
                ),
            ]));
        }

        // 3. Warnings (maximum 2 lines)
        for w in data.warnings.iter().take(2) {
            raw_lines.push(Line::from(vec![Span::styled(w.clone(), Theme::warn())]));
        }

        // 4. Recent (only wide, if non-empty)
        if !is_narrow && !data.recent.is_empty() {
            raw_lines.push(Line::default()); // blank line
            for (i, session) in data.recent.iter().take(3).enumerate() {
                let prefix = if i == 0 {
                    "recent        "
                } else {
                    "              "
                };
                raw_lines.push(Line::from(vec![
                    Span::styled(prefix, Theme::dim()),
                    Span::styled(format!("{:<16}", session.date), Theme::dim()),
                    Span::styled(
                        format!("{:<24}", truncate_chars(&session.title, 22)),
                        Theme::base(),
                    ),
                    Span::styled(session.outcome.clone(), Theme::dim()),
                ]));
            }
        }

        // 5. Hints
        raw_lines.push(Line::default()); // blank line
        if is_narrow {
            let mut hint_items: Vec<(&str, &str)> = Vec::new();
            if data.active_plan.is_some() {
                hint_items.push(("enter", "continue plan"));
            }
            hint_items.push(("tab", "plan / act"));
            hint_items.push(("/help", "controls"));
            if !data.has_sqwai_dir {
                hint_items.push(("/init", "set up project"));
            }
            for (k, d) in hint_items {
                raw_lines.push(Line::from(vec![
                    Span::styled(format!("{:<7}", k), Theme::base()),
                    Span::styled(d, Theme::dim()),
                ]));
            }
        } else {
            let mut col1_items: Vec<(&str, &str)> = Vec::new();
            let mut col2_items: Vec<(&str, &str)> = Vec::new();
            let mut col3_items: Vec<(&str, &str)> = Vec::new();

            if data.active_plan.is_some() {
                col1_items.push(("enter", "continue plan"));
                col2_items.push(("tab", "plan / act"));
                col3_items.push(("/help", "controls"));

                col1_items.push(("type a task to start", ""));
                if !data.has_sqwai_dir {
                    col3_items.push(("/init", "set up project"));
                }
            } else {
                col1_items.push(("type a task to start", ""));
                col2_items.push(("tab", "plan / act"));
                col3_items.push(("/help", "controls"));

                if !data.has_sqwai_dir {
                    col2_items.push(("/init", "set up project"));
                }
            }

            let num_rows = col1_items.len().max(col2_items.len()).max(col3_items.len());
            for r in 0..num_rows {
                let mut row_spans = Vec::new();
                if let Some(&(k, d)) = col1_items.get(r) {
                    let pair_text = if d.is_empty() {
                        k.to_string()
                    } else {
                        format!("{k}  {d}")
                    };
                    let pad = 26usize.saturating_sub(pair_text.chars().count());
                    if d.is_empty() {
                        row_spans.push(Span::styled(k, Theme::dim()));
                    } else {
                        row_spans.push(Span::styled(k, Theme::base()));
                        row_spans.push(Span::styled(format!("  {d}"), Theme::dim()));
                    }
                    row_spans.push(Span::styled(" ".repeat(pad), Theme::base()));
                } else {
                    row_spans.push(Span::styled(" ".repeat(26), Theme::base()));
                }

                if let Some(&(k, d)) = col2_items.get(r) {
                    let pair_text = if d.is_empty() {
                        k.to_string()
                    } else {
                        format!("{k}  {d}")
                    };
                    let pad = 24usize.saturating_sub(pair_text.chars().count());
                    if d.is_empty() {
                        row_spans.push(Span::styled(k, Theme::dim()));
                    } else {
                        row_spans.push(Span::styled(k, Theme::base()));
                        row_spans.push(Span::styled(format!("  {d}"), Theme::dim()));
                    }
                    row_spans.push(Span::styled(" ".repeat(pad), Theme::base()));
                } else {
                    row_spans.push(Span::styled(" ".repeat(24), Theme::base()));
                }

                if let Some(&(k, d)) = col3_items.get(r) {
                    if d.is_empty() {
                        row_spans.push(Span::styled(k, Theme::dim()));
                    } else {
                        row_spans.push(Span::styled(k, Theme::base()));
                        row_spans.push(Span::styled(format!("  {d}"), Theme::dim()));
                    }
                }
                raw_lines.push(Line::from(row_spans));
            }
        }

        // Layout padding and wrapping
        let left_pad = if is_narrow {
            1u16
        } else {
            ((chat.width as i32 - 84) / 2).max(2) as u16
        };
        let pad_str = " ".repeat(left_pad as usize);
        let max_content_width = (chat.width as usize)
            .saturating_sub(left_pad as usize + 1)
            .max(10);

        let tagged_raw: Vec<(Line<'static>, Option<usize>)> =
            raw_lines.into_iter().map(|l| (l, None)).collect();
        let (wrapped_lines, _) =
            crate::tui::markdown::wrap_tagged(tagged_raw, max_content_width as u16);

        let padded_lines: Vec<Line<'static>> = wrapped_lines
            .into_iter()
            .map(|line| {
                if line.spans.is_empty()
                    || (line.spans.len() == 1 && line.spans[0].content.is_empty())
                {
                    Line::default()
                } else {
                    let mut spans = vec![Span::styled(pad_str.clone(), Theme::base())];
                    spans.extend(line.spans);
                    Line::from(spans)
                }
            })
            .collect();

        let total_lines = padded_lines.len() as u16;
        let top_pad = if chat.height > total_lines + 3 {
            (chat.height / 4).max(1)
        } else if chat.height > total_lines {
            1
        } else {
            0
        };

        let available_height = chat.height.saturating_sub(top_pad) as usize;
        let render_lines: Vec<Line<'static>> =
            padded_lines.into_iter().take(available_height).collect();
        let render_rect = Rect {
            x: chat.x,
            y: chat.y + top_pad,
            width: chat.width,
            height: render_lines.len() as u16,
        };
        Paragraph::new(render_lines).style(Theme::base()).render(render_rect, buf);
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
        assert_eq!(
            dim_bg(Color::Rgb(100, 150, 200)),
            Color::Rgb(60, 90, 120)
        );
    }

    #[test]
    fn shorten_path_shortens_user_home() {
        use crate::tui::app::shorten_path;
        use std::path::Path;
        if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            let path = Path::new(&home).join("dev").join("sqwai");
            let shortened = shorten_path(&path);
            assert!(
                shortened.starts_with("~/"),
                "must start with ~/: {shortened}"
            );
        }
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

/// Widest the project directory may get in the status bar before it is
/// truncated. It shrinks further when the rest of the row needs the space.
const DIR_MAX_COLS: usize = 20;

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
fn dim_bg(bg: ratatui::style::Color) -> ratatui::style::Color {
    use ratatui::style::Color;
    match bg {
        Color::Reset | Color::Black | Color::DarkGray => bg,
        Color::Indexed(i) if (232..=255).contains(&i) => {
            Color::Indexed(232 + (i - 232) * 3 / 5)
        }
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
fn struct_chunk(ord: u64, line: Line<'static>, tag: Option<usize>, width: u16) -> RowChunk {
    let (rows, tags) = wrap_tagged(vec![(line, tag)], width);
    (AsmTag::Struct(ord), rows.into_iter().zip(tags).collect())
}

pub(super) fn blank() -> Line<'static> {
    Line::from(vec![Span::styled(String::new(), Theme::base())])
}

/// The one-line summary of a turn's working content. Failures are spelled out
/// here: a collapsed block must never hide the fact that something broke.
/// While the turn runs (`live_tick` set), the "activity" word shimmers
/// Codex-style; finished headers stay static dim.
pub(super) fn activity_header_line(
    g: &ActivityGroup,
    live_tick: Option<usize>,
) -> Line<'static> {
    let arrow = if g.expanded { "▾" } else { "▸" };
    let mut spans = vec![Span::styled(format!("  {arrow} "), Theme::dim())];
    match live_tick {
        Some(tick) => spans.extend(crate::tui::shimmer::shimmer_spans("activity", tick)),
        None => spans.push(Span::styled("activity".to_string(), Theme::dim())),
    }
    let mut text = format!(" · {} calls", g.calls);
    if g.thinking > 0 {
        text.push_str(&format!(" · {} thinking", g.thinking));
    }
    text.push_str(&format!(" · {}s", g.duration_ms / 1000));
    spans.push(Span::styled(text, Theme::dim()));
    if g.errors > 0 {
        spans.push(Span::styled(format!(" · {} error", g.errors), Theme::err()));
    }
    if g.rejected > 0 {
        spans.push(Span::styled(
            format!(" · {} rejected", g.rejected),
            Theme::warn(),
        ));
    }
    Line::from(spans)
}

/// Shift a rendered line right by `n` columns without touching its styles.
fn indent_line(l: Line<'static>, n: usize) -> Line<'static> {
    if n == 0 {
        return l;
    }
    let mut spans = Vec::with_capacity(l.spans.len() + 1);
    spans.push(Span::styled(" ".repeat(n), Theme::base()));
    spans.extend(l.spans);
    Line::from(spans)
}

fn dim_all(l: Line<'static>) -> Line<'static> {
    Line::from(
        l.spans
            .into_iter()
            .map(|s| Span::styled(s.content, s.style.patch(Style::new().fg(Theme::DIM()))))
            .collect::<Vec<_>>(),
    )
}

fn line_text(l: &Line<'_>) -> String {
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
