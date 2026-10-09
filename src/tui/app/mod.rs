use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Block;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tui_textarea::TextArea;

use crate::agent::loop_task::{
    AgentEvent, AgentHandle, AgentOutcome, ApprovalDecision, ControlMsg, spawn_agent,
};
use crate::config::{Config, EffortLevel, ModelConfig, ModelStatus};
use crate::plan;
use crate::providers::{self, Message as PMessage, Role, SharedProvider};
use crate::session::{ActivitySummary, Session, SessionHeader, TurnNote};
use crate::tui::markdown::Highlighter;
use crate::tui::theme::Theme;

/// In-flight frame built by the UI thread, awaiting the presenter's report.
/// Joined by sequence number when the report arrives (perf log correlation).
struct PendingFrame {
    seq: u64,
    ts_built: Instant,
    render_us: Duration,
    rebuild_us: u128,
    merge: &'static str,
    fresh: usize,
}

fn reopened_step_ids(
    active: &plan::Plan,
    records: &[crate::agent::journal::Record],
    session_id: &str,
    files: &[String],
) -> Vec<String> {
    // Conservative rule (§3.6): ANY reverted part of a done step's result
    // reopens it. Reverting a subset while the rest stays changed cannot
    // leave the step marked completed. Paths are separator-normalized so
    // git forward slashes match journal backslashes on Windows.
    let file_set: std::collections::HashSet<String> =
        files.iter().map(|f| f.replace('\\', "/")).collect();
    active
        .steps
        .iter()
        .filter(|step| step.status == plan::StepStatus::Done)
        .filter(|step| {
            let evidence_paths: Vec<String> = records
                .iter()
                .filter(|record| {
                    record.plan.as_deref() == Some(active.id.as_str())
                        && record.step.as_deref() == Some(step.id.as_str())
                        && record.kind == "file_diff"
                        && step.evidence.iter().any(|reference| {
                            (reference.session.is_empty() || reference.session == session_id)
                                && reference.seq == record.seq
                        })
                })
                .filter_map(|record| record.fields.get("path").and_then(|value| value.as_str()))
                .map(|path| path.replace('\\', "/"))
                .collect();
            !evidence_paths.is_empty() && evidence_paths.iter().any(|path| file_set.contains(path))
        })
        .map(|step| step.id.clone())
        .collect()
}

fn reopen_undone_steps(
    root: &std::path::Path,
    session_id: &str,
    files: &[String],
    checkpoint: &str,
) -> Vec<String> {
    let Some(mut active) = plan::open_active_for_session(root, Some(session_id))
        .ok()
        .flatten()
    else {
        return Vec::new();
    };
    let records = crate::agent::journal::Journal::records_for(root, session_id).unwrap_or_default();
    let reopened = reopened_step_ids(&active, &records, session_id, files);
    for step_id in &reopened {
        let _ = plan::reopen_for_undo(
            &mut active,
            step_id,
            format!("reopened by undo to {checkpoint}"),
        );
    }
    if !reopened.is_empty() {
        let args = serde_json::json!({
            "ids": reopened,
            "reason": format!("reopened by undo to {checkpoint}"),
        });
        let _ = plan::commit(root, session_id, &mut active, "reopen", "host", true, args);
    }
    if let Ok(mut journal) = crate::agent::journal::Journal::open(root, session_id) {
        journal.set_attribution(None, Some(active.id.clone()), "host");
        let _ = journal.append_undo(checkpoint, files, &reopened);
    }
    reopened
}

/// Outcome of a background `/why` narration (AB), rendered by
/// [`App::poll_why`]. Everything in here is owned and `Send`: the task
/// boundary demands both.
enum WhyOutcome {
    Answered { text: String },
    Failed(String),
}

mod events;
mod forms;
mod menus;
mod perf;
#[cfg(test)]
mod tests;
mod view;

use forms::FormField;
use menus::{Menu, MenuAction};
use view::{ActivityGroup, AskRow, CellPos, SegMeta, Segment, Selection, StoredView};

use menus::COMMANDS;

#[derive(Debug, Clone, Copy, PartialEq)]
enum StatusKind {
    Info,
    Ok,
    Warn,
    Err,
}

/// How long a bottom-bar notice stays up before vanishing.
const TOAST_TTL: std::time::Duration = std::time::Duration::from_secs(3);

/// Transient bottom-bar notice: every status/error lands here for 3s, the
/// chat stays clean. A new notice replaces the current one outright.
#[derive(Debug, Clone)]
struct Toast {
    text: String,
    kind: StatusKind,
    until: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Plan,
    Act,
}

impl Mode {
    fn toggle(self) -> Self {
        match self {
            Self::Plan => Self::Act,
            Self::Act => Self::Plan,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Plan => "PLAN",
            Self::Act => "ACT",
        }
    }
    /// Sweep endpoint for the chip color transition.
    fn chip_rgb(self) -> (u8, u8, u8) {
        match self {
            Self::Act => MODE_ACT_RGB,
            Self::Plan => MODE_PLAN_RGB,
        }
    }
}

/// Mode-chip color sweep length: a quick flash, not a lingering animation.
const MODE_BLEND_MS: u64 = 250;
/// Sweep endpoints: the effort Low/High hues, so the settle frame does
/// not pop (see `Theme::mode_chip_act/plan`) — which is why they name the
/// theme's numbers instead of carrying their own copy.
const MODE_ACT_RGB: (u8, u8, u8) = crate::tui::theme::Theme::OK_RGB;
const MODE_PLAN_RGB: (u8, u8, u8) = (110, 165, 255);

const WORKING_SPINNER: [char; 6] = ['◜', '◠', '◝', '◞', '◡', '◟'];

/// Typewriter flush window: chars move from the reveal queue into the live
/// answer at most once per interval. Each flush bumps the live segment's rev
/// and a rev bump re-renders (wraps, highlights) the whole growing answer, so
/// this window is the direct cap on the streaming render cost — the pace the
/// reader sees comes out identical because the step is due-based.
const REVEAL_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(90);
/// Reveal speed in chars/s: the old 12-chars-per-50ms-tick pace, unchanged.
const REVEAL_RATE: usize = 240;

/// Chars to reveal in one flush: due at REVEAL_RATE since the window opened,
/// at least 2, capped by the queue. The upper bound is pre-clamped — `queued`
/// can be 1 on a slow stream, and `clamp` panics when min > max.
fn reveal_step(since_ms: u128, queued: usize) -> usize {
    (since_ms as usize * REVEAL_RATE / 1000).clamp(2, queued.max(2))
}

/// Connection-check state for one provider, rendered on its menu rows.
#[derive(Clone)]
pub(super) enum ProviderCheck {
    /// worker thread running; row shows a spinner text
    Checking,
    /// reachable; carries the short detail (`"3 models"`, `"ok"`)
    Ok(String),
    /// unreachable; carries the trimmed reason
    Err(String),
}

type BuiltinUpdateRx = (
    bool,
    std::sync::mpsc::Receiver<Result<Option<crate::config::BuiltinCatalog>, String>>,
);

/// State of a `/test churn` fake turn: how many synthetic calls were
/// emitted, when the last one went out, and which call is still open.
struct TestChurn {
    n: usize,
    last: Instant,
    open: Option<String>,
}

pub struct App {
    cfg: Config,
    model_cfg: ModelConfig,
    provider: SharedProvider,
    session: Session,
    hl: Highlighter,
    /// Stable system prefix: role, rules, project instructions, static
    /// environment. Captured once per session and reused byte-for-byte so a
    /// provider-side prefix cache can hit on every request.
    stable_prefix: String,
    /// This instance is read-only because another sqwai process owns the project lock.
    read_only: bool,
    /// Session-scoped environment facts, rebuilt only at startup and compaction.
    session_environment: String,
    /// Whether the current prompt still needs the full tool-oriented context.
    context_bootstrap_pending: bool,
    active_skills: Vec<crate::prompts::skills::Skill>,

    /// Project root, resolved once at construction. The status bar used to
    /// call `std::env::current_dir()` on every frame; nothing in the process
    /// ever changes directory, so this is the same value with none of the
    /// per-redraw syscalls — and it is injectable in tests.
    pub(super) project_root: PathBuf,
    /// Name of the project directory as shown in the status bar.
    pub(super) cwd_label: String,

    input: TextArea<'static>,
    segments: Vec<Segment>,

    streaming: bool,
    aborted: bool,
    agent: Option<AgentHandle>,
    /// background `/why` answer (AB): collection end of a oneshot
    /// the task sends its answer through. `Some` while it flies.
    why_rx: Option<tokio::sync::oneshot::Receiver<WhyOutcome>>,
    /// derived visible steps from the active structured plan
    todos: Vec<String>,
    /// tracked child agents shown in the overview
    subagents: Vec<(u64, String, String, String, bool)>,
    /// full read-only transcripts for each child agent
    subagent_chats: std::collections::BTreeMap<u64, Vec<Segment>>,
    /// child transcript currently replacing the main chat on screen
    active_subagent: Option<u64>,
    /// checked options in the current multi-select ask_user, per question
    /// (legacy menu path; the inline segment below is the source of truth)
    ask_picked: Vec<Vec<bool>>,
    /// custom text per question for ask_user (legacy menu path)
    ask_custom: Vec<String>,
    /// which question is focused (for Tab switching) (legacy menu path)
    ask_focus: usize,
    /// which question's custom field is being edited, if any
    ask_custom_focus: Option<usize>,
    /// id of the AskUser awaiting an answer, if any. Tracked by id rather
    /// than index: tool/thinking rows inserted later shift indices, but the
    /// live question is always found by lookup. While `Some`, the chat
    /// itself is the interactive surface (no overlay).
    active_ask_id: Option<u64>,
    /// mouse hover target inside the active inline AskUser, for highlight
    ask_hover: Option<AskRow>,
    assistant_buf: String,
    /// arrived text not yet revealed to the screen (typewriter effect)
    pending_reveal: String,
    thinking_open: bool,
    thinking_idx: Option<usize>,
    /// `/test churn` fake turn: synthetic tool rows through the real
    /// handlers (zero tokens, works offline). While `Some`, the frame
    /// loop emits a row every few seconds; Esc stops it.
    test_churn: Option<TestChurn>,
    mode: Mode,
    /// true while the current session has no user turns yet: a fresh,
    /// unsaved scratch session (never persisted until the first message)
    startup: bool,
    pub(super) last_ctrl_c: Option<Instant>,
    /// Tab is physically held down (seen Press without Release): repeats
    /// while held must not toggle the mode, only a fresh press does
    tab_held: bool,
    /// transient bottom-bar notice (3s): every status/error lands here, the
    /// chat stays clean. A new notice replaces the current one.
    toast: Option<Toast>,
    /// user messages typed mid-turn: sent as new turns FIFO after the
    /// running one ends (or on abort); cleared on session switch
    pending_queue: Vec<String>,
    /// previous turn ended successfully — gates retry notifications
    prev_turn_ok: bool,
    /// already toasted for the current retry cycle
    retry_notified: bool,
    /// last typewriter flush: reveal batches once per REVEAL_FLUSH_INTERVAL
    last_reveal: Option<Instant>,
    /// live retry indicator rendered in the status bar (single updating line)
    retry_line: Option<String>,
    /// the next request may carry the resume notice once, then it disarms:
    /// armed when a session with history is loaded or a compaction changed
    /// something — the only genuine restores
    resume_notice_armed: bool,
    /// label of the last shadow checkpoint (design §10 indicator)
    last_checkpoint: Option<String>,
    /// user message index pushed for the active turn, if any
    turn_user_index: Option<usize>,

    /// latest diagnostic count reported by the LSP manager
    lsp_diagnostics: usize,
    follow: bool,
    /// absolute top line of the viewport when not following (None-equivalent: follow == true)
    view_top: usize,
    spinner_tick: usize,
    /// wall-clock origin for animation ticks: loop iterations run at
    /// wildly varying rates (input bursts vs idle), so the tick is derived
    /// from elapsed time — otherwise shimmer/spinners speed up and slow
    /// down with the input flood
    tick_origin: Instant,
    /// cached terminal size (set at startup, refreshed on Resize events);
    /// frames are built for exactly this area, the presenter resets on change
    term_size: ratatui::layout::Size,
    /// built-frame sequence (mailbox latest-wins, reports join by seq)
    frame_seq: u64,
    last_reported_seq: u64,
    pending_frames: std::collections::VecDeque<PendingFrame>,
    last_presented_at: Option<Instant>,
    renderer_dead_reported: bool,
    /// last time draw_menu rebuilt menu_rows (throttled background refresh)
    menu_built_at: Option<std::time::Instant>,
    quit: bool,

    dirty: bool,
    cache_w: u16,
    cache_lines: Vec<Line<'static>>,
    cache_rowseg: Vec<Option<usize>>,
    /// chunk map of the live row buffers above: `asm_tags[i]` owns
    /// `asm_lens[i]` rows starting at the running offset. Powers the splice
    /// fast path — unchanged chunks are never re-cloned on rebuild.
    asm_tags: Vec<view::AsmTag>,
    asm_lens: Vec<usize>,
    last_chat: Rect,
    last_input: Rect,
    /// per-segment render cache keyed by segment id (not position):
    /// appends and stream updates never invalidate other segments' rows.
    /// Entry holds (revision, width, content key, wrapped rows).
    seg_cache: std::collections::HashMap<u64, view::SegCacheEntry>,
    /// incremental parser state for the live assistant answer (markdown.rs)
    live_md: crate::tui::markdown::LiveRender,
    /// identity + revision per segment, aligned 1:1 with `segments`.
    /// Maintained ONLY through the push/insert/remove/set/clear/touch
    /// helpers below so the render cache survives appends and stream updates
    /// instead of being wiped on every structural change.
    seg_meta: Vec<view::SegMeta>,
    /// monotonic segment id source; never reused, so a removed id can never
    /// collide with a later segment (no ABA in cache keys or click targets)
    next_seg_id: u64,
    /// bumped on palette switch; part of the transcript fingerprint so a
    /// theme change always rebuilds even when no segment changed
    theme_rev: u64,
    /// fingerprint of the last assembled transcript; a draw whose fingerprint
    /// matches skips reassembly entirely (typing, scroll, selection, hover)
    last_fp: u64,
    /// per-frame perf log behind the `/debug` toggle; disabled by default
    perf: perf::PerfLog,
    /// how the last rebuild merged chunks (Skip = fingerprint gate hit)
    last_merge: view::MergeKind,
    /// segment chunks rebuilt in the last rebuild
    last_fresh: usize,
    /// microseconds spent in the last rebuild_cache/rebuild_sub_cache
    last_rebuild_us: u128,
    /// last terminal resize event; while fresh, width-driven rebuilds wait
    /// for the size to settle instead of re-rendering the whole transcript
    /// on every intermediate drag event
    last_resize: Option<Instant>,
    /// set by draw when it deferred a width rebuild: the loop must keep
    /// `dirty` so the next tick retries instead of parking stale rows
    defer_rebuild: bool,
    /// scroll anchor set by toggles: (view, rowseg tag, screen offset).
    /// Applied after the next rebuild so an expanding block keeps its header
    /// on the same screen row instead of jumping with `follow`.
    pending_anchor: Option<(Option<u64>, usize, usize)>,
    /// semantic click target resolved at mouse-down; re-validated by id at
    /// mouse-up so a layout shift in between cannot hit a stale row number
    press_target: Option<view::ClickTarget>,
    /// per-subagent scroll: (view_top, follow). Assembled rows live in
    /// `sub_stores`; the main view's rows live in `main_store` while parked.
    /// The ACTIVE view always owns `cache_lines`/`cache_rowseg`/`asm_*`/
    /// `view_top`/`follow`/`last_fp` directly, so painting and hit-testing
    /// never care which transcript is shown — only open/close move buffers.
    sub_views: std::collections::HashMap<u64, (usize, bool)>,
    /// parked assembled rows of the main transcript while a subagent view
    /// is open (restored whole on close, no reassembly)
    main_store: StoredView,
    /// parked assembled rows per subagent view
    sub_stores: std::collections::HashMap<u64, StoredView>,
    /// segment renders performed by rebuilds (cumulative). Tests reset and
    /// assert it; the `/debug` perf log differences it per frame.
    test_renders: u32,
    /// main scroll stashed while a subagent view is open; restored on close
    stashed_main_scroll: Option<(usize, bool)>,
    /// identity per subagent-chat row, aligned with `subagent_chats` vecs
    subagent_meta: std::collections::BTreeMap<u64, Vec<view::SegMeta>>,

    /// Clipboard text inserted by Ctrl+V, used to consume the terminal's
    /// replay of the same payload before it reaches normal key handling.
    /// Offset-based (no suffix copies); dies on full consume or deadline.
    paste_replay: Option<events::PasteReplay>,
    /// Suppresses the synthetic Enter some terminals emit after Ctrl+V.
    /// Timestamped: an unconsumed guard must never eat a later genuine Enter.
    paste_enter_until: Option<Instant>,
    /// Classifies ordinary Windows Enter events as submit vs pasted newlines.
    enter_gate: events::EnterGate,
    /// Pending events queued during burst detection.
    pending_events: std::collections::VecDeque<crossterm::event::Event>,

    /// Finalized activity groups (one per completed agent turn). The currently
    /// streaming turn is rendered live and only lands here at `finish_turn`.
    activity_groups: Vec<ActivityGroup>,
    /// Wall-clock start of the turn currently streaming, used to freeze the
    /// activity duration once the turn completes.
    turn_started: Option<Instant>,

    // command popup
    hover: Option<String>,
    popup_dismiss: bool,
    popup_scroll: usize,
    popup_rows: Vec<(u16, String)>,

    // providers/models menu (Ctrl+P)
    menu_stack: Vec<Menu>,
    menu_sel: usize,
    /// hovered form row: highlight only, focus stays on click
    form_hover_row: Option<usize>,
    /// scroll offset for long list menus
    menu_scroll: usize,
    /// list-window height from the last draw: wheel/nav clamp against it
    menu_visible_rows: usize,
    /// fixed hint line under a list menu (not part of the scrolled rows)
    menu_footer_text: Option<String>,
    menu_rows: Vec<(Line<'static>, MenuAction)>,
    /// pinned action rows below the scroll window (Providers actions):
    /// always visible, navigable, never scrolled. Empty everywhere else.
    menu_sticky_footer: Vec<(Line<'static>, MenuAction)>,
    /// frozen column header for table menus (Sessions/Models/Providers):
    /// drawn above the scroll window, never a nav step
    menu_table_header: Option<Line<'static>>,
    menu_rect: Rect,
    /// when the current approval dialog opened: Enter inside the grace
    /// window does not commit (focus-steal protection), it only keeps
    /// the preselected deny
    approval_opened_at: Option<Instant>,
    /// table width the Sessions/Models/Providers rows were built for:
    /// draw_menu rebuilds once when the real card disagrees, so a stale
    /// rect never desyncs header from content
    table_built_w: u16,
    /// card width the md showcase was wrapped for (same rebuild rule)
    md_built_w: u16,
    /// click/hover targets of the effort slider (rect → SELECTABLE index),
    /// rebuilt on every slider draw; empty when another menu is open
    effort_hits: Vec<(Rect, usize)>,
    /// effort slider color sweep: (displayed color, arm time) plus the
    /// selection it was armed for. Mirrors the mode-chip blend: a level
    /// change redirects mid-sweep instead of restarting it. Retired (→ None)
    /// past the sweep, and cleared whenever the popup opens fresh.
    effort_blend: Option<((u8, u8, u8), Instant)>,
    effort_blend_sel: Option<usize>,
    form_fields: Vec<FormField>,
    form_focus: usize,
    /// stashed text drafts surviving a submenu detour (e.g. form → Effort
    /// menu → back): prefill rebuilds fields from config, which would eat
    /// unsaved typing. Restored once on return, then cleared.
    form_draft: Vec<(String, String)>,
    /// cached session list for the sessions menus (headers only — see
    /// `Session::list_visible_headers`)
    sessions: Vec<SessionHeader>,
    /// live filter typed inside the sessions menu
    sessions_filter: String,
    ef_click: Option<(u16, u16)>,
    /// Connection-check results per provider, shown on the provider's menu
    /// rows: green on success, red with the reason on failure.
    provider_checks: std::collections::HashMap<String, ProviderCheck>,
    /// In-flight check and the channel its worker thread reports back on,
    /// polled on the UI tick so the check never blocks rendering.
    provider_check_rx: Option<(String, std::sync::mpsc::Receiver<Result<String, String>>)>,
    /// Background check/download of builtin providers: (manual, rx)
    builtin_update_rx: Option<BuiltinUpdateRx>,
    /// Background undo maintenance (blobs + shadow GC) — the scan+gc can
    /// take seconds with many sessions, so `/new` must not block on it.
    maintain_rx:
        Option<std::sync::mpsc::Receiver<anyhow::Result<crate::agent::checkpoints::Maintenance>>>,
    /// What the provider told us about the effort level, as opposed to what
    /// the config claims: (model id, level, reason). Cleared when either the
    /// model or the level changes, since the observation was about that pair.
    effort_observed_ignored: Option<(String, EffortLevel, String)>,
    /// Mode-chip color sweep: RGB the sweep started from + start time. The
    /// target is always the current mode's color, so a toggle mid-sweep
    /// re-anchors from the displayed color — rapid toggles redirect the
    /// blend instead of breaking it. Retired (→ None) past the sweep.
    mode_blend: Option<((u8, u8, u8), Instant)>,
    status_y: u16,

    // mouse selection
    press: Option<CellPos>,
    /// menu stack depth at mouse-down: a press that opens a submenu (form
    /// effort row → Effort menu) must not let its paired mouse-up act on
    /// the new menu — the up lands outside the fresh card and would close
    /// (or misfire) it instantly.
    press_menu_depth: Option<usize>,
    /// last seen mouse position, updated on every mouse event: opening a
    /// menu snapshots it (see menu_open_mouse), so hover can tell ambient
    /// jitter on a resting mouse from a deliberate move.
    last_mouse: Option<(u16, u16)>,
    /// mouse position when the top menu opened: hover within a 1-cell
    /// radius of it is ambient jitter, not intent — a resting mouse must
    /// never yank a fresh card's cursor (selected level owns it until a
    /// deliberate move or click).
    menu_open_mouse: Option<(u16, u16)>,
    /// semantic drag anchor: (view, segment id, offset within the segment's
    /// contiguous row run). Rows shift under a press while streaming; the id
    /// + offset still names the pressed content when the drag continues.
    press_anchor: Option<(Option<u64>, u64, usize)>,
    dragging: bool,
    sel: Option<Selection>,
    input_dragging: bool,
}

impl App {
    /// Stable part of the system block: built-in markdown (overridable by
    /// config_dir/system.md), AGENTS.md and the static environment.
    fn stable_prefix(&self) -> String {
        let mut prompt = crate::prompts::stable_prefix();
        let root = self.project_root.clone();
        let mut loaded = crate::prompts::skills::load(&self.cfg.skills, &root);
        for selected in &self.active_skills {
            if !loaded.iter().any(|skill| skill.name == selected.name) {
                loaded.push(selected.clone());
            }
        }
        if let Some(skills) = crate::prompts::skills::prompt(&loaded) {
            prompt.push_str("\n\n");
            prompt.push_str(&skills);
        }
        prompt
    }

    fn rebuild_session_environment(&mut self) {
        let root = self.project_root.clone();
        self.session_environment = crate::prompts::env::session_block(&root);
    }

    // ---------- segment identity ----------
    //
    // `segments` and `seg_meta` are always the same length; every mutation
    // goes through these helpers so render-cache keys and click targets stay
    // bound to stable segment ids instead of shifting positions.

    fn alloc_seg_id(&mut self) -> u64 {
        let id = self.next_seg_id;
        self.next_seg_id += 1;
        id
    }

    /// Append a segment; returns its index. The render cache entry is created
    /// lazily on the next rebuild — nothing already cached is touched.
    fn push_segment(&mut self, seg: Segment) -> usize {
        let id = self.alloc_seg_id();
        self.segments.push(seg);
        self.seg_meta.push(SegMeta { id, rev: 0 });
        self.segments.len() - 1
    }

    fn insert_segment(&mut self, pos: usize, seg: Segment) {
        let id = self.alloc_seg_id();
        let pos = pos.min(self.segments.len());
        self.segments.insert(pos, seg);
        self.seg_meta.insert(pos, SegMeta { id, rev: 0 });
        // cached rows bake in the positional row tag, so every segment whose
        // index shifted re-renders with its new tag
        for m in self.seg_meta.iter_mut().skip(pos + 1) {
            m.rev += 1;
        }
    }

    fn remove_segment(&mut self, i: usize) {
        if i < self.segments.len() {
            self.segments.remove(i);
            self.seg_meta.remove(i);
            // see insert_segment: the shifted tail must repaint its tags
            for m in self.seg_meta.iter_mut().skip(i) {
                m.rev += 1;
            }
        }
    }

    /// Whole-content replacement: logically a new segment, so a fresh id.
    fn set_segment(&mut self, i: usize, seg: Segment) {
        if i < self.segments.len() {
            let id = self.alloc_seg_id();
            self.segments[i] = seg;
            self.seg_meta[i] = SegMeta { id, rev: 0 };
        }
    }

    fn clear_segments(&mut self) {
        self.segments.clear();
        self.seg_meta.clear();
    }

    /// Retain segments by predicate, keeping identity metadata aligned.
    /// Dropped ids are never reused, so cache entries and click targets that
    /// still reference them simply miss instead of hitting new content.
    /// Survivors whose index shifted repaint (cached rows carry the old
    /// positional tag); survivors that kept their index are untouched.
    fn retain_segments(&mut self, mut pred: impl FnMut(&Segment) -> bool) {
        let mut kept_segs = Vec::with_capacity(self.segments.len());
        let mut kept_meta = Vec::with_capacity(self.seg_meta.len());
        for (old_i, (seg, mut meta)) in self
            .segments
            .drain(..)
            .zip(self.seg_meta.drain(..))
            .enumerate()
        {
            if pred(&seg) {
                if kept_segs.len() != old_i {
                    meta.rev += 1;
                }
                kept_segs.push(seg);
                kept_meta.push(meta);
            }
        }
        self.segments = kept_segs;
        self.seg_meta = kept_meta;
    }

    /// Drop transient retry notices. Called when a turn completes
    /// successfully: a recovered retry leaves no scar in the chat. Must run
    /// before `finalize_activity_group`, which would otherwise fold the
    /// notice into the turn's range. Terminal failures keep theirs.
    fn retract_transient_status(&mut self) {
        let before = self.segments.len();
        self.retain_segments(|seg| {
            !matches!(
                seg,
                Segment::Status {
                    transient: true,
                    ..
                }
            )
        });
        if self.segments.len() != before {
            self.dirty = true;
        }
    }

    /// Bump the revision of one subagent-chat row (see `touch_segment`).
    fn sub_touch(&mut self, id: u64, pos: usize) {
        if let Some(meta) = self
            .subagent_meta
            .get_mut(&id)
            .and_then(|meta| meta.get_mut(pos))
        {
            meta.rev += 1;
        }
    }

    /// Bump every row of one subagent transcript. Subagent chats are short
    /// and viewed on demand; coarse invalidation here keeps the main
    /// transcript's cache precise without borrow puzzles at each call site.
    fn sub_touch_all(&mut self, id: u64) {
        if let Some(meta) = self.subagent_meta.get_mut(&id) {
            for m in meta.iter_mut() {
                m.rev += 1;
            }
        }
    }

    /// Insert into a subagent transcript, keeping identity aligned.
    /// The shifted tail repaints (cached rows carry the old positional tag).
    fn sub_insert(&mut self, id: u64, pos: usize, seg: Segment) {
        let nid = self.alloc_seg_id();
        if let Some(chat) = self.subagent_chats.get_mut(&id) {
            let pos = pos.min(chat.len());
            chat.insert(pos, seg);
            if let Some(meta) = self.subagent_meta.get_mut(&id) {
                meta.insert(pos, SegMeta { id: nid, rev: 0 });
                for m in meta.iter_mut().skip(pos + 1) {
                    m.rev += 1;
                }
            }
        }
    }

    /// Mark a segment's content changed after in-place mutation through
    /// `segments.get_mut`. Call at every site that writes through the
    /// borrow — an unchanged rev with changed content paints stale rows.
    fn touch_segment(&mut self, i: usize) {
        if let Some(meta) = self.seg_meta.get_mut(i) {
            meta.rev += 1;
        }
    }

    fn index_of_seg(&self, id: u64) -> Option<usize> {
        self.seg_meta.iter().position(|meta| meta.id == id)
    }

    /// Assemble the system block for one request.
    ///
    /// Order matters: the stable prefix comes first, the durable plan next
    /// (it only changes when the agent rewrites it), and everything that moves
    /// while the agent works goes last so it cannot invalidate the prefix.
    fn system_block(&mut self) -> Vec<crate::providers::SystemPart> {
        use crate::providers::SystemPart;
        let mut parts = vec![SystemPart::cached(self.stable_prefix.clone())];
        if !self.session_environment.is_empty() {
            parts.push(SystemPart::cached(self.session_environment.clone()));
        }
        // Plan state rides every request (built in the turn loop, not
        // here): a turn-start snapshot would predate this turn's plan
        // mutations, leaving the model two plans — one live, one stale.
        let root = self.project_root.clone();
        // The model is never told its mode elsewhere: without this line it
        // learns Plan vs Act from the first refusal, burning a turn.
        parts.push(SystemPart::volatile(format!(
            "Mode: {}",
            match self.mode {
                Mode::Plan => "plan",
                Mode::Act => "act",
            }
        )));
        // Verify-command names from .sqwai/config.toml ([verify]): the model
        // needs them to write cmd: acceptance (/init collects them, nothing
        // else shows them).
        let checks = crate::config::Config::project_verify_commands(&root);
        if !checks.is_empty() {
            parts.push(SystemPart::volatile(format!(
                "Checks: {}",
                checks
                    .iter()
                    .map(|(name, command)| format!("{name} = {command}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }
        // The anchor is host-generated from this session's journal, and it
        // rides only where the transcript has actually lost detail: once a
        // compaction has dropped messages. Rebuilt fresh each turn so
        // resume/compaction never relies on a model-written summary.
        //
        // It used to ride every request while system.md called it the source of
        // truth "because earlier history may be gone" — a standing order to
        // distrust the conversation in front of the model, and the engine of a
        // live session that re-answered its own previous turn and ran five
        // identical `git_diff` calls (§18.1.5).
        if self.session.summary.is_some() {
            parts.push(SystemPart::volatile(crate::agent::context::anchor(
                &root,
                &self.session.id.to_string(),
            )));
        }
        // One-shot resume notice: only the first request after a genuine
        // restore (session loaded with history, or a compaction that changed
        // something) may claim anything was resumed. Injecting it every turn
        // while a step is merely open taught the model that context is
        // restored constantly — and it re-verified the plan before each step.
        if self.resume_notice_armed {
            self.resume_notice_armed = false;
            if !self.session.messages.is_empty()
                && let Some(notice) =
                    crate::agent::context::resume_notice(&root, &self.session.id.to_string())
            {
                parts.push(SystemPart::volatile(notice));
            }
        }
        // One-shot stopped-turn notice: the previous turn was killed by the
        // user mid-work. The transcript survived via live syncs and the plan
        // rides live, so no details here — just the rule that matters: done
        // steps are settled, continue from the pending one or report.
        if self.session.prev_turn_aborted {
            self.session.prev_turn_aborted = false;
            self.session.save().ok();
            let state =
                crate::plan::open_active_for_session(&root, Some(&self.session.id.to_string()))
                    .ok()
                    .flatten()
                    .map(|plan| {
                        let done: Vec<String> = plan
                            .steps
                            .iter()
                            .filter(|s| {
                                matches!(
                                    s.status,
                                    crate::plan::StepStatus::Done
                                        | crate::plan::StepStatus::Cancelled
                                )
                            })
                            .map(|s| s.id.clone())
                            .collect();
                        let pending: Vec<String> = plan
                            .steps
                            .iter()
                            .filter(|s| {
                                matches!(
                                    s.status,
                                    crate::plan::StepStatus::InProgress
                                        | crate::plan::StepStatus::Pending
                                        | crate::plan::StepStatus::Blocked
                                        | crate::plan::StepStatus::Reopened
                                )
                            })
                            .map(|s| s.id.clone())
                            .collect();
                        format!("done=[{}] pending=[{}]", done.join(","), pending.join(","))
                    })
                    .unwrap_or_else(|| "no active plan".to_string());
            parts.push(SystemPart::volatile(format!(
                "The previous turn was stopped by the user mid-work; the transcript above is complete. \
Steps marked done are settled — do not redo them, do not re-verify them. \
Continue from the pending step, or report to the user if the settled work looks wrong. ({state})"
            )));
        }
        let runtime = crate::prompts::runtime_context();
        if !runtime.is_empty() {
            parts.push(SystemPart::volatile(runtime));
        }
        parts
    }

    pub fn new(cfg: Config, session: Session, startup: bool, read_only: bool) -> Result<Self> {
        let model_key = session.model_key.clone();
        let model_cfg = cfg
            .models
            .get(&model_key)
            .cloned()
            .unwrap_or_else(|| ModelConfig {
                provider: String::new(),
                id: model_key.clone(),
                context: session.context_limit,
                effort: EffortLevel::Off,
                effort_control: None,
                effort_always_on: false,
                fallback: None,
                status: ModelStatus::Active,
            });
        let resolved = cfg.resolve_provider(&model_cfg)?;
        let provider = providers::create(&resolved)?;

        #[cfg(test)]
        let project_root: std::path::PathBuf =
            tempfile::tempdir().map(|d| d.keep()).unwrap_or_else(|_| {
                std::env::temp_dir().join(format!("sqwai-test-{}", std::process::id()))
            });
        #[cfg(not(test))]
        let project_root = std::env::current_dir().unwrap_or_default();
        if !read_only {
            // Heal a crash between a journal intent and its plan store
            // (§2.1.4, §3.7) before any plan read. No-op on a clean tree.
            let _ = crate::plan::replay(&project_root);
        }
        // display form of the project root: home-relative (~/sqwai),
        // full path outside home
        let cwd_label = match std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            Ok(home) => project_root
                .strip_prefix(std::path::PathBuf::from(home))
                .ok()
                .map(|rel| format!("~/{}", rel.display()).replace('\\', "/"))
                .unwrap_or_else(|| project_root.display().to_string()),
            Err(_) => project_root.display().to_string(),
        };

        // stamp an unstamped empty session (fresh stub): resumed sessions
        // keep the project they were born in, legacy ones stay universal
        let mut session = session;
        if session.project.is_none() && session.messages.is_empty() {
            session.project = Some(project_root.clone());
        }

        let mut app = Self {
            project_root,
            cwd_label,
            input: Self::fresh_input(String::new()),
            model_cfg,
            provider,
            session,
            hl: Highlighter::new(),
            stable_prefix: String::new(),
            session_environment: String::new(),
            context_bootstrap_pending: true,
            active_skills: Vec::new(),
            cfg,
            segments: Vec::new(),
            streaming: false,
            aborted: false,
            agent: None,
            test_churn: None,
            why_rx: None,
            todos: Vec::new(),
            subagents: Vec::new(),
            subagent_chats: std::collections::BTreeMap::new(),
            active_subagent: None,
            ask_picked: Vec::new(),
            ask_custom: Vec::new(),
            ask_focus: 0,
            ask_custom_focus: None,
            active_ask_id: None,
            ask_hover: None,
            assistant_buf: String::new(),
            pending_reveal: String::new(),
            thinking_open: false,
            thinking_idx: None,
            mode: Mode::Act,
            startup,
            last_ctrl_c: None,
            tab_held: false,
            read_only,
            toast: None,
            pending_queue: Vec::new(),
            prev_turn_ok: false,
            retry_notified: true, // no toast for the very first turn
            last_reveal: None,
            retry_line: None,
            resume_notice_armed: false,
            turn_user_index: None,
            lsp_diagnostics: 0,
            follow: true,
            view_top: 0,
            spinner_tick: 0,
            tick_origin: Instant::now(),
            term_size: ratatui::layout::Size::default(),
            frame_seq: 0,
            last_reported_seq: 0,
            pending_frames: std::collections::VecDeque::new(),
            last_presented_at: None,
            renderer_dead_reported: false,
            menu_built_at: None,
            quit: false,
            dirty: true,
            cache_w: 0,
            cache_lines: Vec::new(),
            cache_rowseg: Vec::new(),
            asm_tags: Vec::new(),
            asm_lens: Vec::new(),
            last_chat: Rect::default(),
            last_input: Rect::default(),
            seg_cache: std::collections::HashMap::new(),
            live_md: crate::tui::markdown::LiveRender::new(),
            seg_meta: Vec::new(),
            next_seg_id: 1,
            theme_rev: 0,
            last_fp: 0,
            perf: perf::PerfLog::new(),
            last_merge: view::MergeKind::Skip,
            last_fresh: 0,
            last_rebuild_us: 0,
            last_resize: None,
            defer_rebuild: false,
            pending_anchor: None,
            press_target: None,
            sub_views: std::collections::HashMap::new(),
            main_store: StoredView::default(),
            sub_stores: std::collections::HashMap::new(),
            test_renders: 0,
            stashed_main_scroll: None,
            subagent_meta: std::collections::BTreeMap::new(),
            hover: None,
            popup_dismiss: false,
            popup_scroll: 0,
            popup_rows: Vec::new(),
            menu_stack: Vec::new(),
            menu_sel: 0,
            form_hover_row: None,
            menu_scroll: 0,
            menu_visible_rows: 0,
            menu_footer_text: None,
            menu_rows: Vec::new(),
            menu_sticky_footer: Vec::new(),
            menu_table_header: None,
            approval_opened_at: None,
            menu_rect: Rect::default(),
            table_built_w: 0,
            md_built_w: 0,
            effort_hits: Vec::new(),
            effort_blend: None,
            effort_blend_sel: None,
            form_fields: Vec::new(),
            form_focus: 0,
            form_draft: Vec::new(),
            sessions: Vec::new(),
            sessions_filter: String::new(),
            ef_click: None,
            provider_checks: std::collections::HashMap::new(),
            provider_check_rx: None,
            builtin_update_rx: None,
            maintain_rx: None,
            effort_observed_ignored: None,
            mode_blend: None,
            status_y: 0,
            press: None,
            press_menu_depth: None,
            last_mouse: None,
            menu_open_mouse: None,
            last_checkpoint: None,
            press_anchor: None,
            dragging: false,
            sel: None,
            input_dragging: false,
            paste_replay: None,
            paste_enter_until: None,
            enter_gate: events::EnterGate::default(),
            pending_events: std::collections::VecDeque::new(),
            activity_groups: Vec::new(),
            turn_started: None,
        };
        if !startup {
            app.load_history_segments();
        }
        if let Some(ref plan_id) = app.session.plan_id
            && crate::plan::open(&app.project_root, plan_id).is_err()
        {
            crate::tui::event_log::log(
                "PLAN",
                format!("plan {plan_id} not found or corrupted; resetting plan_id to None"),
            );
            app.session.plan_id = None;
        }
        if app.session.plan_id.is_none() {
            app.session.plan_id = crate::plan::open_active_for_session(
                &app.project_root,
                Some(&app.session.id.to_string()),
            )
            .ok()
            .flatten()
            .map(|plan| plan.id);
        }
        app.stable_prefix = app.stable_prefix();
        app.rebuild_session_environment();
        app.context_bootstrap_pending = true;
        crate::providers::set_conversation_id(&app.session.id.to_string());
        app.start_builtin_update(false);
        Ok(app)
    }

    /// A thought row rebuilt from a saved session: folded and finished, with
    /// the clock the save carried. Saves from before durations existed say 0,
    /// and a 0 renders as a bare `thought` rather than an invented one.
    fn restored_thinking(text: &str, ms: u64) -> Segment {
        Segment::Thinking {
            text: text.to_string(),
            expanded: false,
            started: None,
            duration_ms: ms,
            live: false,
        }
    }

    /// Render persisted messages as chat segments (used on start and on resume).
    fn load_history_segments(&mut self) {
        // Tool calls and their results are part of the durable provider
        // transcript. Keep the rendered row keyed by call id so batched calls
        // and providers that return results out of order are restored safely.
        let mut pending_tools = std::collections::HashMap::<String, usize>::new();
        // A delegated call has no generic tool row (see handle_tool_start):
        // its one row is the child-chat row, and the child's transcript is
        // never persisted. Rebuild that row from the call + its result so a
        // reloaded session matches what the live path painted.
        let mut pending_children = std::collections::HashMap::<String, usize>::new();
        let mut restored_child_id = 0u64;
        // Anchored mode: summaries recorded the user message that started
        // their turn, so each can be re-attached to the right group even when
        // turns were stopped/failed. Legacy saves (all anchors None) fall
        // back to the old sequential order.
        let anchored_mode = self.session.activity.iter().any(|a| a.user_index.is_some());
        let mut remaining_summaries = self.session.activity.clone();
        let turn_notes = self.session.turn_notes.clone();
        let messages = self.session.messages.clone();
        let mut work_start: Option<usize> = None;
        let mut previous_user: Option<usize> = None;
        let mut turn_user: Option<usize> = None;
        for (message_index, m) in messages.iter().enumerate() {
            match m.role {
                Role::User => {
                    // A stopped/failed turn has no Assistant message to give
                    // it a place in the provider transcript. Restore its note
                    // immediately before the next user turn (or after the loop
                    // for the final turn) to preserve chat order.
                    if let Some(user_index) = previous_user {
                        self.restore_turn_notes(&turn_notes, user_index);
                    }
                    // the dangling work run of that turn also ends here: close
                    // it so the stopped turn does not swallow this user
                    // message (and everything after it) into one giant group
                    if let Some(seg_start) = work_start.take() {
                        self.close_restored_group(
                            seg_start,
                            turn_user,
                            &mut remaining_summaries,
                            anchored_mode,
                        );
                    }
                    previous_user = Some(message_index);
                    turn_user = Some(message_index);
                    self.push_segment(Segment::User(m.content.clone()));
                }
                Role::Assistant if m.tool_calls.is_empty() => {
                    // a restored thought row joins the work run it preceded,
                    // exactly where the live path painted it
                    if !m.thinking.is_empty() {
                        self.push_segment(Self::restored_thinking(&m.thinking, m.thinking_ms));
                    }
                    if let Some(seg_start) = work_start.take() {
                        // A saved session is never streaming: historical work
                        // must begin folded, even if it ended with an error.
                        self.close_restored_group(
                            seg_start,
                            turn_user,
                            &mut remaining_summaries,
                            anchored_mode,
                        );
                    }
                    self.push_segment(Segment::Assistant {
                        text: m.content.clone(),
                        live: false,
                    });
                }
                Role::Assistant => {
                    work_start.get_or_insert(self.segments.len());
                    if !m.thinking.is_empty() {
                        self.push_segment(Self::restored_thinking(&m.thinking, m.thinking_ms));
                    }
                    let trimmed = m.content.trim();
                    if !trimmed.is_empty() {
                        self.push_segment(Segment::Commentary(trimmed.to_string()));
                    }
                    for call in &m.tool_calls {
                        if call.name == "subagent" {
                            restored_child_id += 1;
                            let idx = self.push_segment(Segment::Subagent {
                                id: restored_child_id,
                                task: crate::agent::tools::call_summary(&call.name, &call.args),
                                // refined by the matching tool result below;
                                // an interrupted delegation keeps the marker
                                // (its row is the only trace it existed)
                                status: "running".into(),
                                output: String::new(),
                                expanded: false,
                            });
                            pending_children.insert(call.id.clone(), idx);
                            continue;
                        }
                        let idx = self.push_segment(Segment::Tool {
                            name: call.name.clone(),
                            args: crate::agent::tools::call_summary(&call.name, &call.args),
                            call_id: Some(call.id.clone()),
                            ok: None,
                            output: String::new(),
                            diff: None,
                            preview: Vec::new(),
                            preview_total: 0,
                            expanded: false,
                            flash: None,
                        });
                        pending_tools.insert(call.id.clone(), idx);
                    }
                }
                Role::Tool => {
                    if let Some(call_id) = m.tool_call_id.as_ref()
                        && let Some(idx) = pending_children.remove(call_id)
                    {
                        if let Some(Segment::Subagent { status, output, .. }) =
                            self.segments.get_mut(idx)
                        {
                            *status = if m.is_error { "failed" } else { "completed" }.into();
                            *output = m.content.clone();
                        }
                        self.touch_segment(idx);
                    }
                    if let Some(call_id) = m.tool_call_id.as_ref()
                        && let Some(idx) = pending_tools.remove(call_id)
                    {
                        if let Some(Segment::Tool {
                            ok,
                            output,
                            preview,
                            preview_total,
                            ..
                        }) = self.segments.get_mut(idx)
                        {
                            *ok = Some(!m.is_error);
                            *output = m.content.clone();
                            (*preview, *preview_total) = view::tool_preview(None, &m.content);
                        }
                        self.touch_segment(idx);
                    }
                }
                Role::System => {}
            }
        }
        if let Some(user_index) = previous_user {
            self.restore_turn_notes(&turn_notes, user_index);
        }
        if let Some(seg_start) = work_start.take() {
            self.close_restored_group(
                seg_start,
                turn_user,
                &mut remaining_summaries,
                anchored_mode,
            );
        }
        // The panel is derived from the active structured plan; legacy session
        // to-do state is intentionally not loaded.
    }

    /// Close a restored work run `[seg_start..]` into an `ActivityGroup`,
    /// preferring the summary anchored to this turn's user message.
    /// Restored groups always fold shut, like live-finished ones.
    fn close_restored_group(
        &mut self,
        seg_start: usize,
        turn_user: Option<usize>,
        remaining: &mut Vec<ActivitySummary>,
        anchored_mode: bool,
    ) {
        let answer = self.segments.len();
        let derived = self.build_activity_group((seg_start, answer));
        let saved = take_summary(remaining, anchored_mode, turn_user).unwrap_or(ActivitySummary {
            calls: derived.calls,
            thinking: derived.thinking,
            duration_ms: 0,
            errors: derived.errors,
            rejected: 0,
            user_index: turn_user,
        });
        self.activity_groups.push(ActivityGroup {
            seg_start,
            seg_end: answer,
            calls: saved.calls.max(derived.calls),
            thinking: saved.thinking,
            duration_ms: saved.duration_ms,
            errors: saved.errors,
            rejected: saved.rejected,
            turn_user: saved.user_index.or(turn_user),
        });
    }

    fn restore_turn_notes(&mut self, notes: &[TurnNote], user_index: usize) {
        for note in notes.iter().filter(|note| note.user_index == user_index) {
            self.push_segment(Segment::Status {
                text: note.text.clone(),
                kind: if note.is_error {
                    StatusKind::Err
                } else {
                    StatusKind::Info
                },
                expanded: false,
                // restored notes are history, never transient: a retried turn
                // that later succeeded left no retry segment behind
                transient: false,
            });
        }
    }

    fn fresh_input(text: String) -> TextArea<'static> {
        let mut input = TextArea::new(vec![String::new()]);
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                input.insert_newline();
            }
            input.insert_str(line);
        }
        input.set_block(Self::input_block());
        input.set_style(Theme::base());
        input.set_cursor_line_style(Style::new());
        // Classic block cursor / selection on manually colored backgrounds
        // (the styles.md exception for black & white).
        input.set_cursor_style(
            Style::new()
                .bg(ratatui::style::Color::White)
                .fg(ratatui::style::Color::Black),
        );
        input.set_selection_style(
            Style::new()
                .bg(ratatui::style::Color::Cyan)
                .fg(ratatui::style::Color::Black),
        );
        input
    }

    fn input_block() -> Block<'static> {
        Block::default()
    }

    /// Width of the `› ` input marker gutter (0 on degenerate widths).
    /// The marker occupies the gutter on the top input row only; the
    /// textarea itself is shifted right by this on every row.
    pub(super) fn input_marker_w(&self) -> u16 {
        if self.last_input.width >= 6 { 2 } else { 0 }
    }

    pub async fn run(
        mut self,
        frame_tx: crate::tui::presenter::FrameTx,
        stats_rx: std::sync::mpsc::Receiver<crate::tui::presenter::FrameReport>,
        presenter_alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
        term_size: ratatui::layout::Size,
    ) -> Result<()> {
        self.term_size = term_size;
        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<crossterm::event::Event>();
        let input_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let input_notify_tx = std::sync::Arc::clone(&input_notify);
        std::thread::spawn(move || {
            // input latency is the UI: this thread wins the CPU over any
            // batch work the agent is running
            crate::tui::priority::raise_this_thread();
            while let Ok(ev) = crossterm::event::read() {
                crate::tui::event_log::log("READ", crate::tui::event_log::describe(&ev));
                if ev_tx.send(ev).is_err() {
                    break;
                }
                input_notify_tx.notify_one();
            }
        });

        let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // UI-side build gate: widget render is memory-fast, but there is no
        // point rebuilding buffers faster than the presenter can show them.
        // Slow presents coalesce in the mailbox structurally (no EMA needed).
        let mut last_build = Instant::now() - crate::tui::presenter::MIN_PRESENT_INTERVAL;
        // Frame buffer ring: a frame may be mid-present or still sit in the
        // presenter mailbox while the next one is built. Slots are reused in
        // place; `Arc::get_mut` only succeeds once the presenter has dropped
        // its hold, so an in-flight frame can never be painted through. A
        // slot still held when the ring wraps around (presenter starved past
        // the whole ring under CPU load) is replaced, not written.
        const FRAME_BUF_SLOTS: usize = 8;
        let mut frame_bufs: Vec<std::sync::Arc<ratatui::buffer::Buffer>> = Vec::new();
        let mut frame_buf_idx = 0usize;
        while !self.quit {
            // Event-driven wakeup: a mouse/key event wakes the loop instantly
            // instead of waiting up to 50ms for the next tick. The tick stays
            // for spinner/typewriter/background polls while idle. Presenting
            // happens on the presenter thread: this loop never blocks on
            // terminal I/O.
            tokio::select! {
                _ = input_notify.notified() => {},
                _ = tick.tick() => {},
            }
            let presenter_gone = !presenter_alive.load(std::sync::atomic::Ordering::Relaxed);
            if presenter_gone && !self.renderer_dead_reported {
                self.renderer_dead_reported = true;
                self.status("renderer thread died - restart sqwai", StatusKind::Err);
            }
            if self.enter_gate.flush(Instant::now()) {
                self.submit();
            }
            self.poll_input(&ev_rx)?;
            self.poll_agent();
            self.poll_why();
            self.poll_provider_check();
            self.poll_builtin_update();
            self.poll_maintain();
            // typewriter: reveal queued answer text gradually, catching up when
            // the queue grows faster than the reveal speed
            if !self.pending_reveal.is_empty() {
                let step = if self.cfg.ui.typewriter {
                    // One full re-render per flush window, not per loop pass:
                    // the reveal bumps the live segment's rev, and every rev
                    // bump re-renders the whole growing answer. The step is
                    // due-based, so the visible pace stays at REVEAL_RATE
                    // chars/s while the number of full re-renders drops with
                    // the flush interval.
                    let since = self
                        .last_reveal
                        .map(|t0| t0.elapsed())
                        .unwrap_or(REVEAL_FLUSH_INTERVAL);
                    if since < REVEAL_FLUSH_INTERVAL {
                        0
                    } else {
                        self.last_reveal = Some(Instant::now());
                        let queued = self.pending_reveal.chars().count();
                        reveal_step(since.as_millis(), queued)
                    }
                } else {
                    usize::MAX
                };
                if step > 0 {
                    self.dirty |= self.reveal_chars(step);
                }
            }
            // a drained queue resets the flush window: the next turn's first
            // reveal must be paced by the window, not dump whatever a stale
            // one accumulated while nothing was streaming
            if self.pending_reveal.is_empty() {
                self.last_reveal = None;
            }
            if self
                .toast
                .as_ref()
                .is_some_and(|t| Instant::now() >= t.until)
            {
                self.toast = None;
                self.dirty = true;
            }
            let animating = self.streaming
                || self.tool_running()
                || self.toast.is_some()
                // the animation gallery *is* its frame: it must keep its 20 FPS
                // with nothing else running behind it
                || matches!(self.cur_menu(), Some(Menu::TestAnim))
                // live mode-chip sweep gets its 250ms even past streaming
                // end, or the chip would freeze mid-blend on the toggle
                || self.mode_blend.is_some_and(|(_, t0)| {
                    t0.elapsed().as_millis() < MODE_BLEND_MS as u128
                })
                // live effort-slider sweep gets its 250ms too, or it
                // would freeze on the arming frame showing the old color
                || self.effort_blend.is_some_and(|(_, t0)| {
                    t0.elapsed().as_millis() < MODE_BLEND_MS as u128
                })
                // finish waves get their outcome span even past streaming end,
                // or the sweep would freeze mid-row on the last tool
                || self.segments.iter().any(|s| {
                    matches!(s, Segment::Tool { flash: Some(t0), ok: Some(done), .. } if t0.elapsed().as_millis() < crate::tui::shimmer::flash_ms(*done) as u128)
                });
            // Retire expired finish waves: the row renders its static style
            // again, and the rev bump forces the one final repaint even when
            // the wave was the only thing animating (turn ended exactly on
            // the tool finish) — otherwise the last wave frame would stick.
            for i in 0..self.segments.len() {
                let expired = matches!(self.segments.get(i), Some(Segment::Tool { flash: Some(t0), ok: Some(done), .. }) if t0.elapsed().as_millis() >= crate::tui::shimmer::flash_ms(*done) as u128);
                if expired {
                    if let Some(Segment::Tool { flash, .. }) = self.segments.get_mut(i) {
                        *flash = None;
                    }
                    self.touch_segment(i);
                    self.dirty = true;
                }
            }
            let sub_ids: Vec<u64> = self.subagent_chats.keys().copied().collect();
            for id in sub_ids {
                let mut done = Vec::new();
                if let Some(chat) = self.subagent_chats.get(&id) {
                    for (pos, seg) in chat.iter().enumerate() {
                        if matches!(seg, Segment::Tool { flash: Some(t0), ok: Some(done), .. } if t0.elapsed().as_millis() >= crate::tui::shimmer::flash_ms(*done) as u128)
                        {
                            done.push(pos);
                        }
                    }
                }
                if !done.is_empty() {
                    if let Some(chat) = self.subagent_chats.get_mut(&id) {
                        for pos in &done {
                            if let Some(Segment::Tool { flash, .. }) = chat.get_mut(*pos) {
                                *flash = None;
                            }
                        }
                    }
                    for pos in done {
                        self.sub_touch(id, pos);
                    }
                    self.dirty = true;
                }
            }
            // Retire the finished chip sweep: without this the last RGB
            // frame would stick, since nothing else repaints a settled
            // status bar.
            if self
                .mode_blend
                .is_some_and(|(_, t0)| t0.elapsed().as_millis() >= MODE_BLEND_MS as u128)
            {
                self.mode_blend = None;
                self.dirty = true;
            }
            // Retire the finished slider sweep too. Its endpoint is the
            // settled color, so nothing visual sticks — but the stale start
            // color must go, or the next level change anchors a blend from a
            // frame that expired long ago.
            if self
                .effort_blend
                .is_some_and(|(_, t0)| t0.elapsed().as_millis() >= MODE_BLEND_MS as u128)
            {
                self.effort_blend = None;
            }
            if animating {
                // fixed 20 FPS animation rate from the wall clock, not per
                // loop iteration: bursts would otherwise fast-forward it
                self.spinner_tick = (self.tick_origin.elapsed().as_millis() / 50) as usize;
                // the gallery's rows carry the frame, so they are the thing that
                // animates here; scroll and selection survive a rebuild. Every
                // other tick, because the gallery shows one frame per two (see
                // Menu::TestAnim) — rebuilding in between would clone rows
                // whose frame did not move.
                if matches!(self.cur_menu(), Some(Menu::TestAnim))
                    && self.spinner_tick.is_multiple_of(2)
                {
                    self.build_menu_rows();
                }
                self.dirty = true;
            }
            // fake churn turn emits its synthetic rows from here (streaming
            // keeps `animating` true, so this runs every frame for free)
            if self.test_churn.is_some() {
                self.tick_test_churn();
            }
            if self.dirty && last_build.elapsed() >= crate::tui::presenter::MIN_PRESENT_INTERVAL {
                let area =
                    ratatui::layout::Rect::new(0, 0, self.term_size.width, self.term_size.height);
                // cleared every frame (as the old draw path did): only a
                // deferred width rebuild sets it below
                self.defer_rebuild = false;
                if area.width >= 20 && area.height >= 6 && !presenter_gone {
                    // render timing for the `/debug` perf log; the present
                    // timing arrives later with the presenter's report and is
                    // joined by sequence number below
                    let t0 = Instant::now();
                    self.last_merge = view::MergeKind::Skip;
                    self.last_rebuild_us = 0;
                    self.last_fresh = 0;
                    if frame_bufs.len() != FRAME_BUF_SLOTS {
                        frame_bufs = (0..FRAME_BUF_SLOTS)
                            .map(|_| std::sync::Arc::new(ratatui::buffer::Buffer::empty(area)))
                            .collect();
                    }
                    let slot = frame_buf_idx % FRAME_BUF_SLOTS;
                    frame_buf_idx = frame_buf_idx.wrapping_add(1);
                    if std::sync::Arc::strong_count(&frame_bufs[slot]) > 1 {
                        frame_bufs[slot] =
                            std::sync::Arc::new(ratatui::buffer::Buffer::empty(area));
                    }
                    let buf = std::sync::Arc::get_mut(&mut frame_bufs[slot])
                        .expect("slot refcount is 1 after the replace above");
                    // resize replaces the slot; same area only clears cells
                    if buf.area != area {
                        *buf = ratatui::buffer::Buffer::empty(area);
                    } else {
                        buf.reset();
                    }
                    self.render_into(buf, area);
                    let render_us = t0.elapsed();
                    self.frame_seq += 1;
                    let seq = self.frame_seq;
                    self.pending_frames.push_back(PendingFrame {
                        seq,
                        ts_built: t0,
                        render_us,
                        rebuild_us: self.last_rebuild_us,
                        merge: self.last_merge.as_str(),
                        fresh: self.last_fresh,
                    });
                    frame_tx.submit(crate::tui::presenter::FrameData {
                        seq,
                        area,
                        buf: std::sync::Arc::clone(&frame_bufs[slot]),
                    });
                }
                last_build = Instant::now();
                // a deferred width rebuild keeps dirty set: the 50ms tick
                // retries, and the rebuild runs once the size settles
                if !self.defer_rebuild {
                    self.dirty = false;
                }
            }
            // Drain presenter reports (nonblocking, FIFO from one sender):
            // log presented frames, count coalesced drops by sequence gaps.
            while let Ok(rep) = stats_rx.try_recv() {
                let dropped = rep.seq.saturating_sub(self.last_reported_seq + 1);
                self.last_reported_seq = self.last_reported_seq.max(rep.seq);
                let mut render_us = 0u128;
                let mut rebuild_us = 0u128;
                let mut merge = "skip";
                let mut fresh = 0usize;
                let mut ts_built = rep.presented_at;
                self.pending_frames.retain(|p| {
                    if p.seq == rep.seq {
                        render_us = p.render_us.as_micros();
                        rebuild_us = p.rebuild_us;
                        merge = p.merge;
                        fresh = p.fresh;
                        ts_built = p.ts_built;
                        false
                    } else {
                        p.seq > rep.seq
                    }
                });
                let latency_us = rep
                    .presented_at
                    .saturating_duration_since(ts_built)
                    .as_micros();
                let pace_us = match self.last_presented_at {
                    Some(prev) => rep.presented_at.saturating_duration_since(prev).as_micros(),
                    None => 0,
                };
                self.last_presented_at = Some(rep.presented_at);
                let stat = perf::FrameStat {
                    draw_us: rep.draw_us,
                    rebuild_us,
                    bytes: rep.bytes,
                    flush_us: rep.flush_us,
                    pace_us,
                    render_us,
                    latency_us,
                    dropped,
                    merge,
                    fresh,
                    segs: self.segments.len(),
                    rows: self.cache_lines.len(),
                    tick: self.spinner_tick,
                    streaming: self.streaming,
                    running: self.tool_running(),
                    view: self.active_subagent.unwrap_or(0),
                };
                let renders = self.test_renders;
                let wraps = crate::tui::markdown::wrap_tagged_calls();
                self.perf.frame(stat, renders, wraps);
            }
        }
        // Shutdown must never wait for a provider request: the session is
        // saved and nothing else runs on the exit path. The host-only diary
        // entry that used to be appended here is gone with the diary (§20).
        if self.session_has_messages() {
            self.session.save().ok();
        }
        Ok(())
    }

    /// a session is only worth persisting once it carries real conversation;
    /// a bare launch (no 'n', no send) leaves `messages` empty and must not
    /// be written to disk as a stub
    fn session_has_messages(&self) -> bool {
        !self.session.messages.is_empty()
    }

    fn jump_to_bottom_on_typing(&mut self) {
        if !self.follow {
            self.follow = true;
        }
    }

    fn input_text(&self) -> String {
        self.input.lines().join("\n")
    }

    pub(super) fn is_inline_ask(&self) -> bool {
        self.active_ask_seg().is_some()
            || matches!(
                self.cur_menu(),
                Some(Menu::AskUser { .. }) | Some(Menu::AskFree { .. })
            )
    }

    pub(super) fn is_inline_ask_free(&self) -> bool {
        matches!(self.cur_menu(), Some(Menu::AskFree { .. }))
    }

    /// the live AskUser segment awaiting an answer, if it still exists.
    /// Resolved by tool-call id: rows inserted later shift indices.
    pub(super) fn active_ask_seg(&self) -> Option<usize> {
        let id = self.active_ask_id?;
        self.segments.iter().position(|s| {
            matches!(
                s,
                Segment::AskUser {
                    id: qid,
                    answered: None,
                    ..
                } if *qid == id
            )
        })
    }

    /// insert an inline AskUser segment in execution order — before the live
    /// answer, like tool rows — so the finished turn folds it into its
    /// activity group (collapsed by default) instead of leaving the whole
    /// questionnaire rendered below the answer.
    pub(super) fn push_ask_segment(
        &mut self,
        id: u64,
        questions: Vec<crate::agent::loop_task::AskQuestion>,
    ) {
        self.flush_assistant_preamble_to_commentary();
        // a previous unanswered ask (e.g. after abort) is frozen first so at
        // most one segment stays active
        self.freeze_active_ask("(no answer — superseded)");
        let picked = questions
            .iter()
            .map(|q| vec![false; q.options.len()])
            .collect();
        let custom = vec![String::new(); questions.len()];
        let seg = Segment::AskUser {
            id,
            questions,
            picked,
            custom,
            focus: 0,
            answered: None,
            // live questions open unfolded so they can be answered at once
            expanded: true,
        };
        let pos = self
            .segments
            .iter()
            .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
            .unwrap_or(self.segments.len());
        self.insert_segment(pos, seg);
        self.active_ask_id = Some(id);
        self.ask_hover = None;
        self.ask_custom_focus = None;
        self.follow = true;
        self.dirty = true;
    }

    /// build the answer string for an inline AskUser segment, same shape as
    /// the legacy menu confirm (`"label1, label2; custom"` per question,
    /// `"H: answer"` joined with `" | "`, `"(no answer)"` when empty)
    pub(super) fn inline_ask_text(&self, seg_idx: usize) -> String {
        let Some(Segment::AskUser {
            questions,
            picked,
            custom,
            ..
        }) = self.segments.get(seg_idx)
        else {
            return String::new();
        };
        let single_no_header = questions.len() == 1 && questions[0].header.is_empty();
        let mut parts = Vec::new();
        for (q_idx, q) in questions.iter().enumerate() {
            let labels: Vec<String> = picked
                .get(q_idx)
                .map(|v| {
                    v.iter()
                        .enumerate()
                        .filter(|(_, p)| **p)
                        .filter_map(|(i, _)| q.options.get(i))
                        .map(|o| o.label.clone())
                        .collect()
                })
                .unwrap_or_default();
            let free = custom
                .get(q_idx)
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let mut answer = String::new();
            if !labels.is_empty() {
                answer.push_str(&labels.join(", "));
            }
            if !free.is_empty() {
                if !answer.is_empty() {
                    answer.push_str("; ");
                }
                answer.push_str(&free);
            }
            if single_no_header {
                parts.push(if answer.is_empty() {
                    "(no answer)".to_string()
                } else {
                    answer
                });
            } else {
                let header = if q.header.is_empty() {
                    format!("Q{}", q_idx + 1)
                } else {
                    q.header.clone()
                };
                parts.push(format!(
                    "{}: {}",
                    header,
                    if answer.is_empty() {
                        "(no answer)".to_string()
                    } else {
                        answer
                    }
                ));
            }
        }
        parts.join(" | ")
    }

    /// single-choice: pick exactly this option in question q
    pub(super) fn inline_ask_select(&mut self, q: usize, opt: usize) {
        let Some(seg) = self.active_ask_seg() else {
            return;
        };
        if let Some(Segment::AskUser { picked, focus, .. }) = self.segments.get_mut(seg) {
            let Some(opts) = picked.get_mut(q) else {
                return;
            };
            if opt >= opts.len() {
                return;
            }
            for (i, v) in opts.iter_mut().enumerate() {
                *v = i == opt;
            }
            *focus = q;
        }
        self.touch_segment(seg);
        self.ask_custom_focus = None;
        self.follow = true;
        self.dirty = true;
    }

    /// multi-choice: toggle one option in question q
    pub(super) fn inline_ask_toggle(&mut self, q: usize, opt: usize) {
        let Some(seg) = self.active_ask_seg() else {
            return;
        };
        if let Some(Segment::AskUser { picked, focus, .. }) = self.segments.get_mut(seg) {
            let Some(v) = picked.get_mut(q).and_then(|v| v.get_mut(opt)) else {
                return;
            };
            *v = !*v;
            *focus = q;
        }
        self.touch_segment(seg);
        self.ask_custom_focus = None;
        self.follow = true;
        self.dirty = true;
    }

    /// switch the focused question (Tab / Shift+Tab / click)
    pub(super) fn inline_ask_focus(&mut self, q: usize) {
        let Some(seg) = self.active_ask_seg() else {
            return;
        };
        if let Some(Segment::AskUser {
            questions, focus, ..
        }) = self.segments.get_mut(seg)
            && q < questions.len()
        {
            *focus = q;
        }
        self.touch_segment(seg);
        self.ask_custom_focus = None;
        self.dirty = true;
    }

    /// confirm the whole inline ask and send it to the agent
    pub(super) fn inline_ask_confirm(&mut self) {
        let Some(seg) = self.active_ask_seg() else {
            return;
        };
        let text = self.inline_ask_text(seg);
        self.inline_ask_answer(text);
    }

    /// Esc on an inline ask: blur a custom editor first, otherwise skip
    /// (empty answer, as before) but freeze the segment visibly.
    pub(super) fn inline_ask_skip(&mut self) {
        if self.ask_custom_focus.is_some() {
            self.ask_custom_focus = None;
            self.dirty = true;
            return;
        }
        self.inline_ask_answer("(no answer)".to_string());
    }

    /// freeze the active inline segment and deliver the text to the agent
    fn inline_ask_answer(&mut self, text: String) {
        let Some(seg) = self.active_ask_seg() else {
            return;
        };
        let id = match self.segments.get(seg) {
            Some(Segment::AskUser { id, .. }) => *id,
            _ => return,
        };
        if let Some(Segment::AskUser {
            answered, expanded, ..
        }) = self.segments.get_mut(seg)
        {
            *answered = Some(text.clone());
            // settle to the one-line head row, like a finished tool row
            *expanded = false;
        }
        self.touch_segment(seg);
        if let Some(agent) = &self.agent {
            let _ = agent.control.try_send(ControlMsg::AskAnswer { id, text });
        }
        self.active_ask_id = None;
        self.ask_hover = None;
        self.ask_custom_focus = None;
        self.follow = true;
        self.dirty = true;
    }

    /// the agent turn ended (or was aborted) while a question was open:
    /// never leave a ghost active segment behind.
    fn freeze_active_ask(&mut self, note: &str) {
        let Some(seg) = self.active_ask_seg() else {
            self.active_ask_id = None;
            return;
        };
        if let Some(Segment::AskUser {
            answered, expanded, ..
        }) = self.segments.get_mut(seg)
        {
            *answered = Some(note.to_string());
            *expanded = false;
        }
        self.touch_segment(seg);
        self.active_ask_id = None;
        self.ask_hover = None;
        self.ask_custom_focus = None;
        self.dirty = true;
    }

    /// Second-level completion target: `<cmd> <tail>` filters that
    /// command's subcommand list from [`menus::SUBCOMMANDS`].
    /// Gated commands (e.g. `/test` without the experimental flag)
    /// complete nothing.
    fn popup_level2_cmd(&self) -> Option<&'static str> {
        let t = self.input_text();
        menus::SUBCOMMANDS
            .iter()
            .map(|(cmd, _)| *cmd)
            .filter(|cmd| *cmd != "/test" || self.experimental_test())
            .find(|cmd| t.starts_with(&format!("{cmd} ")))
    }

    fn popup_visible(&self) -> bool {
        let t = self.input_text();
        if self.popup_dismiss || !t.starts_with('/') {
            return false;
        }
        if !t.contains(' ') {
            return true;
        }
        self.popup_level2_cmd().is_some()
    }

    /// Completion strings, used for both display and insertion.
    /// Level 1: top-level commands (`/plan`). Level 2: `<cmd> <sub>`.
    fn popup_items(&self) -> Vec<String> {
        let t = self.input_text();
        if let Some(cmd) = self.popup_level2_cmd()
            && let Some(subs) = menus::subcommands_of(cmd)
        {
            let tail = t.strip_prefix(cmd).unwrap_or("").trim_start_matches(' ');
            let word = tail.split_whitespace().next().unwrap_or("");
            return subs
                .iter()
                .filter(|sub| sub.starts_with(word))
                .map(|sub| format!("{cmd} {sub}"))
                .collect();
        }
        COMMANDS
            .iter()
            .filter(|cmd| **cmd != "/test" || self.experimental_test())
            .filter(|cmd| cmd.starts_with(&t))
            .map(|cmd| cmd.to_string())
            .collect()
    }

    /// experimental `/test ...` commands are unlocked in
    /// /settings -> Experimental (off by default)
    fn experimental_test(&self) -> bool {
        self.cfg.ui.experimental_test
    }

    pub(super) fn popup_scroll_by(&mut self, delta: i32) -> bool {
        let items = self.popup_items();
        let shown = items
            .len()
            .min(menus::POPUP_MAX_ROWS)
            .min((self.last_input.y as usize).saturating_sub(2).max(3));
        let max_scroll = items.len().saturating_sub(shown);
        let next = if delta < 0 {
            self.popup_scroll
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.popup_scroll.saturating_add(delta as usize)
        };
        let next = next.min(max_scroll);
        // True when the viewport actually moved: scrolling into a clamped
        // edge must not schedule an empty present. Callers repaint separately
        // when they clear a hover highlight.
        if next != self.popup_scroll {
            self.popup_scroll = next;
            true
        } else {
            false
        }
    }

    /// Rebuild the provider client from the current config so changes made
    /// while sqwai is running — above all the API key — take effect on the next
    /// turn without restarting the app. The agent clones this handle per turn,
    /// so a key edited mid-turn is picked up when the next turn starts (after a
    /// normal Esc stop, or simply the next message).
    /// The host rewrote the transcript. A continuation reference points at the
    /// provider's copy of the history that was *not* rewritten, so continuing
    /// from it would hand the model the very context compaction removed
    /// (§3.3) while the host accounts for the compacted one.
    pub(super) fn note_compaction(&mut self, summarized: bool, before: u64, after: u64) {
        self.context_bootstrap_pending = true;
        self.rebuild_session_environment();
        // a forced /compact that changed nothing must not report X → X tok
        // as if work happened; say it fits instead
        if !summarized && before == after {
            self.status(
                &format!(
                    "history already fits: {} tok, nothing dropped",
                    fmt_k(before)
                ),
                StatusKind::Info,
            );
            return;
        }
        let verb = if summarized { "summarized" } else { "trimmed" };
        // the transcript was just replaced: the next request may orient the
        // model once (open step, if any), then the notice disarms
        self.resume_notice_armed = true;
        // before/after are the HISTORY estimate, not the status-bar context
        // (which adds system + tool schemas) — label them so the numbers
        // match something the user can verify
        self.status(
            &format!(
                "history compacted ({verb}): {} → {} tok",
                fmt_k(before),
                fmt_k(after)
            ),
            if summarized {
                StatusKind::Ok
            } else {
                StatusKind::Info
            },
        );
    }

    /// Whether this provider may be asked to continue from its own copy of the
    /// conversation. Off means the transcript is resent every request, which
    /// is what §1.1 and §2.2 assume: the host owns the context.
    pub(super) fn continuation_enabled(&self) -> bool {
        self.cfg
            .providers
            .get(&self.model_cfg.provider)
            .is_none_or(|p| p.continuation)
    }

    /// What the current model does with the effort slider. The wire format is
    /// the fallback declaration, so an unknown provider is treated as the
    /// conservative case rather than as full support.
    pub(super) fn effort_support(&self) -> crate::config::EffortSupport {
        let format = self
            .cfg
            .providers
            .get(&self.model_cfg.provider)
            .map(|p| p.format)
            .unwrap_or(crate::config::WireFormat::Openai);
        self.model_cfg.effort_support(format)
    }

    pub(super) fn effort_plan_for(&self, level: EffortLevel) -> crate::providers::effort::Plan {
        crate::providers::effort::plan(level, self.effort_support())
    }

    /// Model key the open Effort menu acts on: scoped from the edit form,
    /// else the session model.
    pub(super) fn effort_target_key(&self) -> String {
        match self.cur_menu() {
            Some(crate::tui::app::menus::Menu::Effort { model }) => model
                .clone()
                .unwrap_or_else(|| self.session.model_key.clone()),
            _ => self.session.model_key.clone(),
        }
    }

    /// Effort support for an arbitrary model key (the Effort menu can be
    /// scoped to a model that is not the session one).
    pub(super) fn effort_support_for(&self, key: &str) -> crate::config::EffortSupport {
        if let Some(m) = self.cfg.models.get(key) {
            let format = self
                .cfg
                .providers
                .get(&m.provider)
                .map(|p| p.format)
                .unwrap_or(crate::config::WireFormat::Openai);
            return m.effort_support(format);
        }
        self.effort_support()
    }

    /// The plan for the level in force, with anything the *provider* told us
    /// taking precedence over what the config claims. A declaration is a
    /// claim; a zero reasoning-token count is evidence.
    pub(super) fn effort_plan(&self) -> crate::providers::effort::Plan {
        let mut plan = self.effort_plan_for(self.model_cfg.effort);
        if let Some((model, level, why)) = &self.effort_observed_ignored
            && *model == self.model_cfg.id
            && *level == self.model_cfg.effort
        {
            plan.status = crate::providers::effort::Status::Ignored {
                why: match why.as_str() {
                    "the provider rejected the effort parameter" => {
                        "the provider rejected the effort parameter"
                    }
                    _ => "the provider reported zero reasoning tokens",
                },
            };
        }
        plan
    }

    /// Drop an observed "ignored" notice once reasoning visibly streams: the
    /// zero-token run it was built from is contradicted. A refused parameter
    /// stays — that verdict is the provider's word, and thoughts arriving
    /// anyway are the model's own, not the level the user asked for.
    fn note_effort_honoured(&mut self) {
        let rejected = self
            .effort_observed_ignored
            .as_ref()
            .is_some_and(|(_, _, why)| why == "the provider rejected the effort parameter");
        if !rejected && self.effort_observed_ignored.take().is_some() {
            self.dirty = true;
        }
    }

    fn rebuild_provider(&mut self) {
        let mc = self.model_cfg.clone();
        match self
            .cfg
            .resolve_provider(&mc)
            .and_then(|rp| providers::create(&rp))
        {
            Ok(p) => self.provider = p,
            Err(e) => self.status(&format!("provider {}: {e:#}", mc.id), StatusKind::Warn),
        }
    }

    /// A retried request recovered: the provider is streaming again, so the
    /// failure rows must go now — not linger until the turn ends. Clears the
    /// live retry line and retracts the transient chat notice; a later fresh
    /// failure re-pushes both (retry_notified re-arms).
    fn clear_retry_state(&mut self) {
        if self.retry_line.is_some() {
            self.retry_line = None;
            self.dirty = true;
        }
        if self.retry_notified {
            self.retry_notified = false;
            self.retract_transient_status();
            self.dirty = true;
        }
    }

    fn submit(&mut self) {
        crate::tui::event_log::log(
            "SUBMIT",
            format!(
                "input_len={} input={:?}",
                self.input_text().chars().count(),
                self.input_text()
            ),
        );
        self.toast = None;
        self.retry_notified = false;
        self.retry_line = None;
        self.last_checkpoint = None;
        let mut text = self.input_text().trim().to_string();
        if text.is_empty() {
            if self.startup {
                // Linked or session-scoped plan only: falling back to the
                // project-global newest here would silently adopt a foreign
                // plan (#171) that the Plan menu — correctly — does not
                // show. No plan of ours → Enter is a no-op.
                let active = self.session_plan();
                if let Some(active) = active {
                    text = if let Some(step) = active.steps.iter().find(|s| {
                        s.status == crate::plan::StepStatus::InProgress
                            || s.status == crate::plan::StepStatus::Pending
                    }) {
                        format!("Continue next plan step: {}", step.title)
                    } else {
                        "Continue next plan step".to_string()
                    };
                } else {
                    return;
                }
            } else if !self.streaming
                && !self.pending_queue.is_empty()
                && self.menu_stack.is_empty()
            {
                // idle with queued follow-ups (a turn failed earlier):
                // empty Enter sends the first one
                text = self.pending_queue.remove(0);
            } else {
                return;
            }
        }
        if self.streaming {
            if !text.starts_with('/') {
                // plain message mid-turn: queue it, it goes as a new turn
                // once the running one ends (or on abort)
                self.pending_queue.push(text);
                self.input = Self::fresh_input(String::new());
                self.status("queued — will send when the turn ends", StatusKind::Info);
                return;
            }
            // Whitelist of commands that are safe to run while the agent is
            // streaming: read-only UI, menus, and viewing the plan. Mutating
            // commands (graph rebuild, plan edits, undo, etc.) stay blocked and
            // show the busy notice.
            let allowed = if let Some(rest) = text.strip_prefix('/') {
                let mut parts = rest.split_whitespace();
                match parts.next().unwrap_or("") {
                    "settings" | "debug" | "theme" | "mcp" | "lsp" | "skill" | "skills"
                    | "providers" | "models" | "sessions" | "exit" | "help" => true,
                    "plan" => parts.next().is_none(),
                    _ => false,
                }
            } else {
                false
            };
            if !allowed {
                self.show_busy_status();
                return;
            }
        }
        if !text.starts_with('/')
            && let Some(pc) = self.cfg.providers.get(&self.model_cfg.provider)
            && pc.effective_api_key(&self.model_cfg.provider).is_none()
        {
            self.status(
                &format!("provider '{}' has no api key", self.model_cfg.provider),
                StatusKind::Err,
            );
            return;
        }
        self.input = Self::fresh_input(String::new());
        self.popup_dismiss = false;
        self.hover = None;
        if let Some(rest) = text.strip_prefix('/') {
            self.command(rest);
            return;
        }
        self.startup = false;
        // pick up any provider/key change made since the last turn
        self.rebuild_provider();
        // AGENTS.md, MEMORY.md and skills are re-read every turn: a file the
        // user (or the agent) changed mid-session — including across a
        // compaction — reaches the model on the next request. Identical bytes
        // keep the same cache key, so the rebuild costs disk reads, not cache.
        self.stable_prefix = self.stable_prefix();
        // session-aware gateways (OpenCode Go) route on this per conversation
        crate::providers::set_conversation_id(&self.session.id.to_string());
        self.push_segment(Segment::User(text.clone()));
        self.session.push(Role::User, &text);
        self.turn_user_index = Some(self.session.messages.len().saturating_sub(1));

        // The system block is assembled per request and travels separately
        // from the transcript: nothing here is ever written to the session.
        let system = self.system_block();
        let root = self.project_root.clone();
        // L0 capture-nudge: a fresh restriction the active plan does not
        // cover yet. Marker scan first so ordinary turns never touch disk.
        if crate::agent::loop_task::has_restriction_marker(&text.to_lowercase())
            && let Ok(Some(plan)) =
                crate::plan::open_active_for_session(&root, Some(&self.session.id.to_string()))
            && let Some(tail) = crate::agent::loop_task::capture_nudge(&text, &plan.constraints)
            && let Some(last) = self.session.messages.last_mut()
        {
            last.content.push_str(&tail);
        }
        let msgs: Vec<PMessage> = self.session.messages.clone();
        let fallback_chain = self
            .cfg
            .resolve_fallback_chain(&self.session.model_key)
            .into_iter()
            .filter_map(|(key, mc, resolved)| {
                let provider = providers::create(&resolved).ok()?;
                let effort_support = mc.effort_support(resolved.format);
                Some(crate::agent::loop_task::FallbackCandidate {
                    key,
                    model_id: mc.id.clone(),
                    provider,
                    effort_support,
                    context_limit: mc.context,
                })
            })
            .collect();
        let input = crate::agent::loop_task::AgentInput {
            provider: self.provider.clone(),
            model_id: self.model_cfg.id.clone(),
            model_key: self.session.model_key.clone(),
            effort: if self.model_cfg.effort == EffortLevel::Off {
                None
            } else {
                Some(self.model_cfg.effort)
            },
            effort_support: self.effort_support(),
            max_tokens: None,
            system,
            messages: msgs,
            root,
            session_id: self.session.id.to_string(),
            blocked_patterns: self.cfg.safety.blocked_patterns.clone(),
            web_allow_hosts: self.cfg.web.allow_hosts.clone(),
            plan_mode: self.mode == Mode::Plan,
            context_limit: self.session.context_limit,
            enable_tools: true,
            // interactive TUI turns defer slow baseline capture to the
            // background worker instead of blocking the tool call
            background_baselines: true,
            read_only: self.read_only,
            mcp: self.cfg.mcp.clone(),
            lsp: self.cfg.lsp.clone(),
            // A continuation reference only travels with the model that
            // produced it, for providers that document the field, and only
            // while the user has not turned it off for this provider.
            previous_response_id: if self.context_bootstrap_pending || !self.continuation_enabled()
            {
                None
            } else {
                self.session.response_id_for(&self.session.model_key)
            },
            summary: self.session.summary.clone(),
            compact_only: false,
            memory: self.cfg.memory.clone(),
            compaction: self.cfg.compaction.clone(),
            plan_limits: self.cfg.plan,
            shadow_store: self.cfg.undo.shadow,
            subagent_depth: 0,
            parent_step: None,
            parent_session: None,
            fallback_chain,
        };
        self.context_bootstrap_pending = false;
        self.agent = Some(spawn_agent(input));
        self.streaming = true;
        self.aborted = false;
        self.assistant_buf.clear();
        self.pending_reveal.clear();
        // the activity header shows how long the turn took; measure from here
        self.turn_started = Some(Instant::now());
        // no thinking placeholder up front: the row appears only when real
        // reasoning deltas arrive (handle_thinking_delta builds it lazily),
        // so an empty "thinking... 0s" never flashes on turns that do not
        // think — effort off, or providers that stay silent.
        self.thinking_open = false;
        self.thinking_idx = None;
        self.push_segment(Segment::Assistant {
            text: String::new(),
            live: true,
        });
        // Evidence for the "activity still live after the answer" report: the
        // header shimmers only while `streaming` is true, so a shimmering
        // header past a finished-looking answer means this turn never ended.
        // Pair `turn start` with `turn finish` to tell "still running" from
        // "finished but repainted".
        crate::tui::event_log::log(
            "TURN",
            format!(
                "start segments={} groups={}",
                self.segments.len(),
                self.activity_groups.len()
            ),
        );
        self.jump_to_bottom_on_typing();
        self.dirty = true;
    }

    /// `/test churn`: a fake infinite turn for exercising the queue row,
    /// the footer and folding with no provider and no tokens. Synthetic
    /// tool rows go through the real handlers, so the transcript cannot
    /// tell them apart; Esc (or `/test churn` again) stops it.
    fn start_test_churn(&mut self) {
        if self.test_churn.is_some() {
            self.stop_test_churn();
            return;
        }
        if self.streaming {
            self.show_busy_status();
            return;
        }
        self.streaming = true;
        self.turn_started = Some(Instant::now());
        self.test_churn = Some(TestChurn {
            n: 0,
            last: Instant::now(),
            open: None,
        });
        self.status(
            "test churn running — type to queue, esc stops",
            StatusKind::Info,
        );
    }

    /// one synthetic call every few seconds, called from the frame loop.
    /// Closes the previous fake row before opening the next, like a model
    /// that keeps working.
    fn tick_test_churn(&mut self) {
        const NAMES: [(&str, &str); 3] = [
            ("bash", "sleep 30"),
            ("read", "src/main.rs"),
            ("grep", "fn handle"),
        ];
        let (n, open) = match &self.test_churn {
            Some(ch) if ch.last.elapsed() >= std::time::Duration::from_millis(2500) => {
                (ch.n, ch.open.clone())
            }
            _ => return,
        };
        if let Some(name) = open {
            self.handle_tool_notice(name, String::new(), true, None, None);
        }
        let (name, args) = NAMES[n % NAMES.len()];
        self.handle_tool_start(name.into(), args.into(), None);
        if let Some(ch) = self.test_churn.as_mut() {
            ch.n = n + 1;
            ch.open = Some(name.into());
            ch.last = Instant::now();
        }
        self.dirty = true;
    }

    /// Esc (or a second `/test churn`): close the open fake row, fold the
    /// turn shut like a finished one, stop. Queued follow-ups stay queued.
    fn stop_test_churn(&mut self) {
        let Some(ch) = self.test_churn.take() else {
            return;
        };
        if let Some(name) = ch.open {
            self.handle_tool_notice(name, String::new(), true, None, None);
        }
        self.streaming = false;
        self.finalize_activity_group();
        self.status(
            &format!("test churn stopped after {} fake calls", ch.n),
            StatusKind::Info,
        );
    }

    /// `/compact`: run the compaction policy over the stored transcript.
    /// Reuses the agent plumbing so the summary request streams like any other
    /// turn and can be aborted with esc.
    fn start_compaction(&mut self) {
        if self.streaming {
            self.show_busy_status();
            return;
        }
        if self.session.messages.is_empty() {
            self.status("nothing to compact yet", StatusKind::Warn);
            return;
        }
        // pick up any provider/key change made since the last turn
        self.rebuild_provider();
        self.session.strip_system_messages();
        let input = crate::agent::loop_task::AgentInput {
            provider: self.provider.clone(),
            model_id: self.model_cfg.id.clone(),
            model_key: self.session.model_key.clone(),
            effort: None,
            effort_support: self.effort_support(),
            max_tokens: None,
            // compaction needs no system block and no tools
            system: Vec::new(),
            messages: self.session.messages.clone(),
            root: self.project_root.clone(),
            session_id: self.session.id.to_string(),
            blocked_patterns: Vec::new(),
            web_allow_hosts: Vec::new(),
            plan_mode: false,
            context_limit: self.session.context_limit,
            enable_tools: false,
            background_baselines: false,
            read_only: self.read_only,
            mcp: Default::default(),
            lsp: Default::default(),
            previous_response_id: None,
            summary: self.session.summary.clone(),
            compact_only: true,
            memory: self.cfg.memory.clone(),
            compaction: self.cfg.compaction.clone(),
            plan_limits: self.cfg.plan,
            shadow_store: self.cfg.undo.shadow,
            subagent_depth: 0,
            parent_step: None,
            parent_session: None,
            fallback_chain: Vec::new(),
        };
        self.agent = Some(spawn_agent(input));
        self.streaming = true;
        self.aborted = false;
        self.turn_user_index = None;
        self.assistant_buf.clear();
        self.status("compacting context…", StatusKind::Info);
    }

    fn apply_session(&mut self, mut s: Session) {
        // persist the session we are leaving — but skip a brand-new empty one,
        // otherwise opening an existing session from a fresh stub would
        // litter the list with an extra empty file
        if self.session_has_messages() {
            self.session.save().ok();
        }
        // resolve the session's model against the current config
        if !self.cfg.models.contains_key(&s.model_key) {
            s.model_key = self.cfg.last_model.clone();
        }
        if let Some(mc) = self.cfg.models.get(&s.model_key).cloned() {
            s.context_limit = mc.context;
            match self
                .cfg
                .resolve_provider(&mc)
                .and_then(|rp| providers::create(&rp))
            {
                Ok(p) => self.provider = p,
                Err(e) => self.status(&format!("model {}: {e:#}", mc.id), StatusKind::Warn),
            }
            self.model_cfg = mc;
        }
        self.context_bootstrap_pending = true;
        self.session = s;
        // a session arriving with history is a genuine restore: the next
        // request may orient the model once, then the notice disarms
        self.resume_notice_armed = !self.session.messages.is_empty();
        crate::providers::set_conversation_id(&self.session.id.to_string());
        // queued follow-ups belong to the old conversation
        self.pending_queue.clear();
        if let Some(ref plan_id) = self.session.plan_id
            && crate::plan::open(&self.project_root, plan_id).is_err()
        {
            crate::tui::event_log::log(
                "PLAN",
                format!("plan {plan_id} not found or corrupted; resetting plan_id to None"),
            );
            self.session.plan_id = None;
        }
        if self.session.plan_id.is_none() {
            self.session.plan_id = crate::plan::open_active_for_session(
                &self.project_root,
                Some(&self.session.id.to_string()),
            )
            .ok()
            .flatten()
            .map(|plan| plan.id);
        }
        // defensive: never let a legacy system turn back into the transcript
        self.session.strip_system_messages();
        self.clear_segments();
        self.seg_cache.clear();
        // the transcript is replaced: old group ranges point nowhere
        self.activity_groups.clear();
        self.active_subagent = None;
        self.subagents.clear();
        self.subagent_chats.clear();
        self.subagent_meta.clear();
        self.sub_views.clear();
        // live + parked rows belong to the old transcript: drop them whole
        // so the next draw assembles from scratch instead of splicing
        // against a stale chunk map
        self.cache_lines.clear();
        self.cache_rowseg.clear();
        self.asm_tags.clear();
        self.asm_lens.clear();
        self.main_store = StoredView::default();
        self.sub_stores.clear();
        self.stashed_main_scroll = None;
        self.todos.clear();
        self.turn_user_index = None;
        // per-turn/per-session notices belong to the old conversation: a
        // stale retry countdown, checkpoint hint or "ignored by model"
        // verdict must not leak into the new session's chrome
        self.effort_observed_ignored = None;
        self.retry_line = None;
        self.last_checkpoint = None;
        self.prev_turn_ok = false;
        self.retry_notified = true;
        self.active_ask_id = None;
        self.ask_hover = None;
        self.ask_custom_focus = None;
        self.rebuild_session_environment();
        self.load_history_segments();
        // an empty session is a fresh scratch pad, never persisted until
        // the first message (see the session_has_messages guards)
        self.startup = self.session.messages.is_empty();
        self.menu_home();
        self.follow = true;
        self.view_top = 0;
        self.sel = None;
        self.press = None;
        self.dragging = false;
        self.dirty = true;
        self.status(
            &format!(
                "session {} · {}",
                short_id(&self.session),
                truncate_chars(&self.session.title.clone(), 24)
            ),
            StatusKind::Ok,
        );
        // a session born in another project still opens, but its plan and
        // undo links point at that project's .sqwai — say so instead of
        // leaving only the routine "session opened" notice
        if self.session.project.is_some()
            && !Session::project_is_here(&self.session.project, &self.project_root)
        {
            let where_born = self
                .session
                .project
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.status(
                &format!(
                    "session from another project ({where_born}): plan and undo links may dangle"
                ),
                StatusKind::Warn,
            );
        }
    }

    fn start_new_session(&mut self) -> bool {
        if self.startup {
            return false;
        }
        if self.streaming {
            self.show_busy_status();
            return false;
        }
        if self.session_has_messages() {
            self.session.save().ok();
        }
        // §2.5 runs retention when a session ends. Doing it here rather than
        // on the way out means it also happens for a session the user simply
        // walks away from, and the new session's own chain is protected by
        // name so its undo history survives its own maintenance.
        self.run_undo_maintenance();
        let ctx = self.session.context_limit;
        self.session = Session::new(self.cfg.last_model.clone(), ctx);
        self.session.project = Some(self.project_root.clone());
        crate::providers::set_conversation_id(&self.session.id.to_string());
        self.pending_queue.clear();
        self.session.plan_id = crate::plan::open_active_for_session(
            &self.project_root,
            Some(&self.session.id.to_string()),
        )
        .ok()
        .flatten()
        .map(|plan| plan.id);
        self.context_bootstrap_pending = true;
        // a fresh empty session stays unsaved until the first message
        self.startup = true;
        self.clear_segments();
        self.seg_cache.clear();
        self.cache_lines.clear();
        self.cache_rowseg.clear();
        self.asm_tags.clear();
        self.asm_lens.clear();
        self.main_store = StoredView::default();
        self.sub_stores.clear();
        self.activity_groups.clear();
        self.active_subagent = None;
        self.subagents.clear();
        self.subagent_chats.clear();
        self.subagent_meta.clear();
        self.sub_views.clear();
        self.stashed_main_scroll = None;
        self.todos.clear();
        self.turn_user_index = None;
        // per-turn/per-session notices belong to the old conversation: a
        // stale retry countdown, checkpoint hint or "ignored by model"
        // verdict must not leak into the new session's chrome
        self.effort_observed_ignored = None;
        self.retry_line = None;
        self.last_checkpoint = None;
        self.prev_turn_ok = false;
        self.retry_notified = true;
        self.active_ask_id = None;
        self.ask_hover = None;
        self.ask_custom_focus = None;
        self.rebuild_session_environment();
        self.follow = true;
        self.view_top = 0;
        self.sel = None;
        self.press = None;
        self.dragging = false;
        self.menu_home();
        self.dirty = true;
        self.status(
            &format!("session {} started", short_id(&self.session)),
            StatusKind::Ok,
        );
        true
    }

    /// Retention for both checkpoint layers (§2.5). Reports only when it did
    /// something: a maintenance pass that announces "nothing to do" on every
    /// `/new` is noise.
    pub(super) fn run_undo_maintenance(&mut self) {
        if self.maintain_rx.is_some() {
            return;
        }
        let root = self.project_root.clone();
        let session = self.session.id.to_string();
        let cfg = self.cfg.undo.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.maintain_rx = Some(rx);
        std::thread::spawn(move || {
            let res = crate::agent::checkpoints::maintain(&root, &cfg, &session);
            let _ = tx.send(res);
        });
    }

    fn poll_maintain(&mut self) {
        let Some(rx) = self.maintain_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(report)) if report.did_anything() => {
                let mut note = String::from("undo maintenance:");
                if report.blobs_removed > 0 {
                    note.push_str(&format!(
                        " {} pre-image(s) freed {}",
                        report.blobs_removed,
                        fmt_bytes(report.blobs_freed_bytes)
                    ));
                }
                if !report.chains_dropped.is_empty() {
                    note.push_str(&format!(
                        " {} finished session chain(s) dropped",
                        report.chains_dropped.len()
                    ));
                }
                if report.collected {
                    note.push_str(" shadow repository collected");
                }
                self.status(&note, StatusKind::Info);
                self.dirty = true;
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                crate::providers::log_http(&format!("undo maintenance failed: {e:#}"));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.maintain_rx = Some(rx);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                crate::providers::log_http("undo maintenance thread ended");
            }
        }
    }

    fn switch_model(&mut self, key: &str) {
        if self.streaming {
            self.show_busy_status();
            return;
        }
        let Some(mc) = self.cfg.models.get(key).cloned() else {
            return;
        };
        match self
            .cfg
            .resolve_provider(&mc)
            .and_then(|rp| providers::create(&rp))
        {
            Ok(p) => {
                self.model_cfg = mc;
                self.provider = p;
                self.context_bootstrap_pending = true;
                // a continuation reference belongs to the old model
                self.session.last_response_id = None;
                self.session.last_response_model = None;
                self.session.model_key = key.to_string();
                self.session.context_limit = self.model_cfg.context;
                self.cfg.last_model = key.to_string();
                self.cfg.save().ok();
                self.status(&format!("model: {key}"), StatusKind::Ok);
            }
            Err(e) => self.status(&format!("model {key}: {e:#}"), StatusKind::Err),
        }
        self.dirty = true;
    }

    fn apply_command_insert(&mut self, cmd: &str) {
        let text = self.input_text();
        let rest = text.split_once(' ').map(|(_, r)| r.to_string());
        let new_text = match rest {
            Some(r) => format!("{cmd} {r}"),
            None => format!("{cmd} "),
        };
        self.input = Self::fresh_input(new_text);
        self.hover = None;
        self.dirty = true;
    }

    /// Level-2 insert: replace only the subcommand word, keep already-typed
    /// arguments (`/plan wai` → `/plan waive `, `/plan waive 2` stays intact).
    fn apply_subcommand_insert(&mut self, cmd: &str, sub: &str) {
        let text = self.input_text();
        let tail = text.strip_prefix(cmd).unwrap_or("").trim_start_matches(' ');
        let rest = tail.split_once(' ').map(|(_, r)| r.to_string());
        let new_text = match rest {
            Some(r) => format!("{cmd} {sub} {r}"),
            None => format!("{cmd} {sub} "),
        };
        self.input = Self::fresh_input(new_text);
        self.hover = None;
        self.dirty = true;
    }

    fn command(&mut self, rest: &str) {
        let name = format!("/{rest}")
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        match name.as_str() {
            "/settings" => self.open_menu(Menu::Settings),
            "/test" => {
                if !self.experimental_test() {
                    self.status("unknown command /test", StatusKind::Warn);
                } else if rest.split_whitespace().nth(1) == Some("art") {
                    self.open_menu(Menu::TestArt);
                } else if rest.split_whitespace().nth(1) == Some("anim") {
                    self.open_menu(Menu::TestAnim);
                } else if rest.split_whitespace().nth(1) == Some("colors") {
                    self.open_menu(Menu::TestColors);
                } else if rest.split_whitespace().nth(1) == Some("md") {
                    self.open_menu(Menu::TestMd);
                } else if rest.split_whitespace().nth(1) == Some("churn") {
                    self.start_test_churn();
                } else {
                    self.status(
                        "/test takes: anim, art, churn, colors, md",
                        StatusKind::Warn,
                    );
                }
            }
            "/debug" => self.open_menu(Menu::Debug),
            "/mcp" => self.status(
                "MCP settings are available from /settings (runtime coming in phase 4)",
                StatusKind::Info,
            ),
            "/lsp" => self.status(
                "LSP settings are available from /settings (runtime coming in phase 4)",
                StatusKind::Info,
            ),
            "/skill" => {
                let query = rest.split_whitespace().nth(1);
                let root = self.project_root.clone();
                let loaded = crate::prompts::skills::load_matching(&self.cfg.skills, &root, query);
                if loaded.is_empty() {
                    self.status("skill not found", StatusKind::Warn);
                } else {
                    self.active_skills = loaded.clone();
                    self.stable_prefix = self.stable_prefix();
                    self.context_bootstrap_pending = true;
                    self.status(
                        &format!(
                            "loaded skill(s): {}",
                            loaded
                                .iter()
                                .map(|s| s.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        StatusKind::Ok,
                    );
                }
            }

            "/skills" => {
                self.active_skills.clear();
                self.stable_prefix = self.stable_prefix();
                self.context_bootstrap_pending = true;
                self.status("automatic Skills loading restored", StatusKind::Ok);
            }

            "/help" => self.open_menu(Menu::Help),
            "/init" => {
                let init_root = self.project_root.clone();
                if init_root.join("AGENTS.md").exists() {
                    self.status("AGENTS.md already exists", StatusKind::Warn);
                } else {
                    match std::fs::write(
                        init_root.join("AGENTS.md"),
                        crate::prompts::AGENTS_TEMPLATE,
                    ) {
                        Ok(()) => self.status(
                            "AGENTS.md created — it is sent to the model with every request",
                            StatusKind::Ok,
                        ),
                        Err(e) => self.status(&format!("init: {e}"), StatusKind::Err),
                    }
                }
                // sqwai-only rules live apart so other tools never see them
                if init_root.join("SQWAI.md").exists() {
                    self.status("SQWAI.md already exists", StatusKind::Warn);
                } else {
                    match std::fs::write(init_root.join("SQWAI.md"), crate::prompts::SQWAI_TEMPLATE)
                    {
                        Ok(()) => self.status(
                            "SQWAI.md created — this agent's own rules, highest priority",
                            StatusKind::Ok,
                        ),
                        Err(e) => self.status(&format!("init: {e}"), StatusKind::Err),
                    }
                }
                // seed named verify commands (repo probing + MEMORY.md) so
                // `cmd: $name` in plans resolves; hand-written names win.
                let root = self.project_root.clone();
                let (added, already) = crate::config::Config::seed_verify_commands(&root);
                if !added.is_empty() || already > 0 {
                    let list = added
                        .iter()
                        .map(|(n, c)| format!("{n} = {c}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let msg = if list.is_empty() {
                        format!("verify commands: {already} already set — .sqwai/config.toml")
                    } else {
                        format!(
                            "verify commands: +{} [{list}] ({already} already set) — .sqwai/config.toml",
                            added.len()
                        )
                    };
                    self.status(&msg, StatusKind::Ok);
                }
            }
            "/plan" => self.plan_command(rest),
            "/goal" => self.goal_command(rest),
            "/constraints" => self.constraints_command(rest),
            "/mode" => self.mode_command(rest),
            "/new" => {
                self.start_new_session();
            }
            "/sessions" => self.open_menu(Menu::Sessions),
            "/providers" => {
                let arg = rest.split_whitespace().nth(1);
                if arg == Some("update") {
                    self.start_builtin_update(true);
                } else {
                    self.open_menu(Menu::Providers);
                }
            }
            "/models" => self.open_menu(Menu::Models {
                provider: self.model_cfg.provider.clone(),
            }),
            "/exit" => {
                // §2.5 runs retention on `/new` and `/exit`; the current
                // session's own chain is kept so a resumed session still has
                // its undo history.
                self.run_undo_maintenance();
                self.quit = true;
            }
            "/compact" => self.start_compaction(),
            "/undo" => {
                if self.streaming {
                    self.show_busy_status();
                } else {
                    let mut args = rest.split_whitespace().skip(1);
                    match (args.next(), args.next()) {
                        (None, _) => self.undo(1),
                        // `/undo step 3` — revert one plan step through the
                        // layer-1 pre-images (§2.5), not the last checkpoint
                        (Some("step"), Some(id)) => self.undo_step(id),
                        (Some("step"), None) => self
                            .status("/undo step needs a step id: /undo step 3", StatusKind::Warn),
                        (Some(arg), _) => match arg.parse::<usize>() {
                            Ok(n) => self.undo(n),
                            // Never guess: this used to parse as `/undo 1` and
                            // revert the last checkpoint, reported as success.
                            Err(_) => self.status(
                                &format!(
                                    "/undo takes a count or a step: /undo, /undo 3, \
                                     /undo step 3. Cannot undo {arg:?}."
                                ),
                                StatusKind::Warn,
                            ),
                        },
                    }
                }
            }
            "/export" => {
                if rest.split_whitespace().nth(1).is_some() {
                    self.status("/export takes no arguments", StatusKind::Warn);
                } else {
                    self.export_session();
                }
            }
            "/why" => {
                let question = rest.strip_prefix("why").unwrap_or("").trim();
                if question.is_empty() {
                    self.status(
                        "/why needs a question: /why why did the tests fail",
                        StatusKind::Warn,
                    );
                } else if self.streaming {
                    self.show_busy_status();
                } else if self.why_rx.is_some() {
                    self.status("a why-answer is already running", StatusKind::Warn);
                } else {
                    self.start_why(question.to_string());
                }
            }
            other if COMMANDS.contains(&other) => {
                self.status(&format!("{other}: not implemented yet"), StatusKind::Warn)
            }
            "" => {}
            other => self.status(&format!("unknown command {other}"), StatusKind::Warn),
        }
        self.dirty = true;
    }

    /// The plan this session works on: its linked plan while the file is
    /// still readable on disk, otherwise the session's own active plan —
    /// never another session's (#171). Every TUI read or mutation of
    /// "the plan" goes through here so two sessions never operate on each
    /// other's plans.
    fn session_plan(&self) -> Option<plan::Plan> {
        let root = &self.project_root;
        if let Some(id) = &self.session.plan_id
            && let Some(plan) = plan::read_plan_file(root, id)
        {
            return Some(plan);
        }
        plan::open_active_for_session(root, Some(&self.session.id.to_string()))
            .ok()
            .flatten()
    }

    /// The linked plan if it is still workable, i.e. active. A linked
    /// completed/abandoned plan is read-only history (§2.1.1, defect A):
    /// mutations must refuse it explicitly instead of silently rewriting a
    /// finished plan the agent itself can no longer touch.
    fn workable_plan(&self) -> Result<plan::Plan, String> {
        match self.session_plan() {
            Some(plan) if plan.status == plan::PlanStatus::Active => Ok(plan),
            Some(plan) => Err(format!(
                "plan is {} (read-only, see /plan history); create a new plan to continue",
                match plan.status {
                    plan::PlanStatus::Completed => "completed",
                    plan::PlanStatus::Abandoned => "abandoned",
                    plan::PlanStatus::Blocked => "blocked",
                    plan::PlanStatus::Active => "active",
                }
            )),
            None => Err("no active plan".to_string()),
        }
    }

    /// Plan id `/plan delete` would remove: the session's own plan while its
    /// file is still on disk, otherwise the most recent active plan.
    /// One shared resolver so the command gate and the confirmed action can
    /// never disagree about what is being deleted.
    fn deletable_plan_id(&self) -> Option<String> {
        let root = &self.project_root;
        self.session
            .plan_id
            .clone()
            .filter(|id| plan::plans_dir(root).join(format!("{id}.json")).exists())
            .or_else(|| {
                plan::open_active_for_session(root, Some(&self.session.id.to_string()))
                    .ok()
                    .flatten()
                    .map(|plan| plan.id)
            })
    }

    fn plan_command(&mut self, rest: &str) {
        let root = self.project_root.clone();
        let args: Vec<&str> = rest.split_whitespace().skip(1).collect();
        let result = match args.first().copied() {
            // bare `/plan` opens the overview; there is no `show` alias
            None => {
                self.open_menu(Menu::Plan);
                return;
            }
            Some("delete") => match self.deletable_plan_id() {
                Some(_) => {
                    self.open_menu(Menu::ConfirmDelete {
                        label: "Are you sure? (y/N)".to_string(),
                        action: MenuAction::DeletePlan,
                    });
                    return;
                }
                None => "no active plan".to_string(),
            },
            Some("history") => {
                let plans = plan::list(&root);
                if plans.is_empty() {
                    "no plan history".to_string()
                } else {
                    plans
                        .iter()
                        .filter(|p| p.status != plan::PlanStatus::Active)
                        .map(|p| format!("{} · {:?} · {}", p.id, p.status, p.goal.text))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            Some("limit") => {
                format!(
                    "plan step limit is [plan].max_steps in the config (currently {}); \
                     a runtime override is not implemented yet",
                    self.cfg.plan.max_steps
                )
            }
            Some("complete") => match self.workable_plan() {
                Ok(mut active) => {
                    match plan::apply(
                        &mut active,
                        plan::Op::Complete,
                        &plan::Limits::default(),
                        None,
                    ) {
                        Ok(plan::Applied::Completed) => {
                            let sid = self.session.id.to_string();
                            match plan::commit(
                                &root,
                                &sid,
                                &mut active,
                                "complete",
                                "user",
                                true,
                                serde_json::json!({}),
                            ) {
                                Ok(_) => "plan completed".to_string(),
                                Err(e) => format!("plan write failed: {e:#}"),
                            }
                        }
                        Ok(_) => "plan complete did not change its status".to_string(),
                        Err(e) => format!("plan complete rejected [{}]: {}", e.code, e.reason),
                    }
                }
                Err(message) => message,
            },
            Some("abandon") => {
                let reason = args
                    .get(1..)
                    .map(|v| v.join(" "))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if reason.is_empty() {
                    "usage: /plan abandon <reason>".to_string()
                } else {
                    match self.workable_plan() {
                        Ok(mut active) => {
                            plan::abandon(&mut active);
                            let sid = self.session.id.to_string();
                            let commit_args =
                                serde_json::json!({"op": "abandon", "reason": reason});
                            match plan::commit(
                                &root,
                                &sid,
                                &mut active,
                                "abandon",
                                "user",
                                true,
                                commit_args,
                            ) {
                                Ok(_) => format!("plan abandoned: {reason}"),
                                Err(e) => format!("plan write failed: {e:#}"),
                            }
                        }
                        Err(message) => message,
                    }
                }
            }
            Some("waive") | Some("confirm") => {
                "waive/confirm are retired: acceptance is now plain done-criteria notes the \
                 agent keeps for itself — there is nothing for the host to certify"
                    .to_string()
            }
            Some(other) => format!("unknown /plan action '{other}'"),
        };
        self.status(&result, StatusKind::Info);
    }

    fn goal_command(&mut self, rest: &str) {
        let root = self.project_root.clone();
        let text = rest
            .split_once(' ')
            .map(|(_, value)| value.trim())
            .unwrap_or_default();
        if text.is_empty() {
            self.status("usage: /goal <text>", StatusKind::Warn);
            return;
        }
        match self.workable_plan() {
            Ok(mut active) => {
                plan::set_goal(
                    &mut active,
                    text.to_string(),
                    "user",
                    Some("user: /goal".to_string()),
                );
                let args = serde_json::json!({
                    "text": text,
                    "source": "user",
                    "reason": "user: /goal",
                });
                let sid = self.session.id.to_string();
                match plan::commit(&root, &sid, &mut active, "set_goal", "user", true, args) {
                    Ok(_) => self.status("goal updated; pending steps are stale", StatusKind::Ok),
                    Err(e) => self.status(&format!("goal update failed: {e:#}"), StatusKind::Err),
                }
            }
            Err(message) => self.status(&message, StatusKind::Warn),
        }
    }

    fn constraints_command(&mut self, rest: &str) {
        let root = self.project_root.clone();
        let mut parts = rest.splitn(3, ' ');
        let _ = parts.next();
        let action = parts.next().unwrap_or_default();
        let text = parts.next().unwrap_or_default().trim();
        if text.is_empty() || !matches!(action, "add" | "remove") {
            self.status("usage: /constraints add|remove <text>", StatusKind::Warn);
            return;
        }
        match self.workable_plan() {
            Ok(mut active) => {
                if action == "add" {
                    active.constraints.push(text.to_string());
                } else if let Some(index) = active.constraints.iter().position(|c| c == text) {
                    active.constraints.remove(index);
                }
                active.revision += 1;
                let args = serde_json::json!({"action": action, "text": text});
                let sid = self.session.id.to_string();
                match plan::commit(&root, &sid, &mut active, "constraints", "user", true, args) {
                    Ok(_) => self.status("constraints updated", StatusKind::Ok),
                    Err(e) => self.status(
                        &format!("constraints update failed: {e:#}"),
                        StatusKind::Err,
                    ),
                }
            }
            Err(message) => self.status(&message, StatusKind::Warn),
        }
    }

    fn mode_command(&mut self, rest: &str) {
        match rest.split_whitespace().nth(1) {
            Some("plan") => {
                self.set_mode(Mode::Plan);
                self.status("mode: PLAN", StatusKind::Info);
            }
            Some("act") => {
                self.set_mode(Mode::Act);
                self.status("mode: ACT", StatusKind::Info);
            }
            _ => self.status("usage: /mode plan|act", StatusKind::Warn),
        }
    }

    /// Switch ACT/PLAN through one funnel: arms the chip color sweep from
    /// the currently displayed color (see `mode_blend`), so a toggle
    /// mid-sweep redirects the blend instead of breaking it.
    fn set_mode(&mut self, mode: Mode) {
        let from = self.mode_chip_rgb(Instant::now());
        self.mode = mode;
        self.mode_blend = Some((from, Instant::now()));
        self.dirty = true;
    }

    /// Chip color right now: in-sweep lerp toward the mode endpoint,
    /// otherwise the endpoint itself.
    fn mode_chip_rgb(&self, now: Instant) -> (u8, u8, u8) {
        let to = self.mode.chip_rgb();
        match &self.mode_blend {
            Some((from, t0)) => {
                let el = now.duration_since(*t0).as_millis();
                if el >= MODE_BLEND_MS as u128 {
                    to
                } else {
                    crate::tui::shimmer::blend(*from, to, el as f64 / MODE_BLEND_MS as f64)
                }
            }
            None => to,
        }
    }

    const BUSY_STATUS: &'static str = "busy · esc to stop";

    fn show_busy_status(&mut self) {
        self.status(Self::BUSY_STATUS, StatusKind::Warn);
    }

    /// Every status/error lands here: a 3s bottom-bar notice replacing the
    /// current one. The chat carries no transient notices anymore — only
    /// durable turn notes (stopped / error:) stay as segments.
    fn status(&mut self, text: &str, kind: StatusKind) {
        self.toast = Some(Toast {
            text: text.to_string(),
            kind,
            until: Instant::now() + TOAST_TTL,
        });
        self.dirty = true;
    }

    /// Live toast, if any: expired ones are dropped on read so the bar
    /// never shows a stale notice.
    fn live_toast(&mut self) -> Option<(String, StatusKind)> {
        if self
            .toast
            .as_ref()
            .is_some_and(|t| Instant::now() >= t.until)
        {
            self.toast = None;
            self.dirty = true;
        }
        self.toast.as_ref().map(|t| (t.text.clone(), t.kind))
    }

    /// move up to `k` chars from the reveal queue to the visible answer
    fn reveal_chars(&mut self, k: usize) -> bool {
        if self.pending_reveal.is_empty() {
            return false;
        }
        let end = self
            .pending_reveal
            .char_indices()
            .nth(k)
            .map(|(i, _)| i)
            .unwrap_or(self.pending_reveal.len());
        let chunk: String = self.pending_reveal.drain(..end).collect();
        self.assistant_buf.push_str(&chunk);
        // Keep the live assistant segment in sync with the reveal queue.
        // Without this, the buffer only became visible at finish_turn(), which
        // made a healthy local model look frozen until the full response ended.
        if let Some(pos) = self
            .segments
            .iter()
            .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
            && let Some(Segment::Assistant { text, .. }) = self.segments.get_mut(pos)
        {
            text.push_str(&chunk);
            self.touch_segment(pos);
        }
        !chunk.is_empty()
    }

    /// Collect a finished provider connection check, if any. The worker thread
    /// reports through a channel so the check never blocks the 50 ms tick;
    /// arrival rebuilds the open menu so the row lights up immediately.
    fn poll_provider_check(&mut self) {
        let Some((name, rx)) = self.provider_check_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(outcome) => {
                let state = match outcome {
                    Ok(detail) => ProviderCheck::Ok(detail),
                    Err(reason) => ProviderCheck::Err(reason),
                };
                self.provider_checks.insert(name, state);
                self.build_menu_rows();
                self.dirty = true;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.provider_check_rx = Some((name, rx));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.provider_checks
                    .insert(name, ProviderCheck::Err("check task ended".to_string()));
                self.build_menu_rows();
                self.dirty = true;
            }
        }
    }

    pub(super) fn start_builtin_update(&mut self, manual: bool) {
        if self.builtin_update_rx.is_some() {
            if manual {
                self.status("update check already in progress…", StatusKind::Info);
            }
            return;
        }
        if manual {
            self.status("checking for built-in provider updates…", StatusKind::Info);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.builtin_update_rx = Some((manual, rx));
        std::thread::spawn(move || {
            let outcome = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt
                    .block_on(crate::config::check_and_update_builtins(manual))
                    .map_err(|e| format!("{e:#}")),
                Err(e) => Err(format!("runtime: {e}")),
            };
            let _ = tx.send(outcome);
        });
    }

    fn poll_builtin_update(&mut self) {
        let Some((manual, rx)) = self.builtin_update_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(outcome) => match outcome {
                Ok(Some(new_catalog)) => {
                    self.cfg.apply_builtins();
                    if let Ok(mc) = self.cfg.last_model_config().cloned()
                        && self.model_cfg.id == mc.id
                    {
                        self.model_cfg = mc;
                    }
                    self.build_menu_rows();
                    self.dirty = true;
                    if manual {
                        self.status(
                            &format!(
                                "built-in providers updated (#{} · {} models)",
                                new_catalog.serial,
                                new_catalog.models.len()
                            ),
                            StatusKind::Ok,
                        );
                    } else {
                        crate::tui::event_log::log(
                            "PROVIDERS",
                            format!(
                                "built-in providers updated in background (#{} · {} models)",
                                new_catalog.serial,
                                new_catalog.models.len()
                            ),
                        );
                    }
                }
                Ok(None) => {
                    if manual {
                        self.status("built-in providers are up to date", StatusKind::Info);
                    }
                }
                Err(e) => {
                    // Auto failures surface once as a status line (at most
                    // daily — the check gate keeps it quiet otherwise), not
                    // just the event log nobody opens.
                    crate::tui::event_log::log(
                        "PROVIDERS",
                        format!("background update check failed: {e}"),
                    );
                    if manual {
                        self.status(&format!("update failed: {e}"), StatusKind::Err);
                    } else {
                        self.status(
                            &format!("provider catalog check failed: {e}"),
                            StatusKind::Warn,
                        );
                    }
                }
            },
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.builtin_update_rx = Some((manual, rx));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
    }

    fn poll_agent(&mut self) {
        // Do not drain an entire buffered response in one 50 ms tick. Keeping
        // a small event budget lets the reveal queue and spinner repaint even
        // when a local model delivers several chunks back-to-back.
        const MAX_EVENTS_PER_TICK: usize = 8;
        let mut processed = 0;
        loop {
            if processed >= MAX_EVENTS_PER_TICK {
                return;
            }
            processed += 1;
            let ev = match self.agent.as_mut() {
                Some(handle) => match handle.rx.try_recv() {
                    Ok(ev) => ev,
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        // Agent died without a final Completed event. If Esc
                        // already requested an abort, this is not a successful
                        // turn and must not read the previous session answer.
                        // Otherwise the session was not replaced, so there is
                        // no new answer to authorize either: only what was
                        // actually streamed may fill the slot.
                        crate::tui::event_log::log(
                            "TURN",
                            format!("agent gone without Completed aborted={}", self.aborted),
                        );
                        let result = if self.aborted {
                            Err("aborted".to_string())
                        } else {
                            Ok(())
                        };
                        self.agent = None;
                        self.finish_turn_inner(result, false);
                        return;
                    }
                },
                None => return,
            };
            match ev {
                AgentEvent::TextDelta(t) => {
                    self.clear_retry_state();
                    self.handle_text_delta(t);
                }
                AgentEvent::ThinkingDelta(t) => {
                    self.handle_thinking_delta(t);
                }
                AgentEvent::Usage(u) => {
                    self.session.add_usage(&u);
                    self.dirty = true;
                }
                AgentEvent::EffortIgnored { level, why } => {
                    self.status(
                        &format!("effort: {level} (ignored by model — {why})"),
                        StatusKind::Warn,
                    );
                    self.effort_observed_ignored =
                        Some((self.model_cfg.id.clone(), self.model_cfg.effort, why));
                    self.dirty = true;
                }
                AgentEvent::EffortHonoured => self.note_effort_honoured(),
                AgentEvent::ResponseId(id) => {
                    // the id only continues a conversation for the model that
                    // produced it; the scope is re-checked before each request
                    self.session.last_response_id = Some(id);
                    self.session.last_response_model = Some(self.session.model_key.clone());
                    self.dirty = true;
                }
                AgentEvent::Compaction {
                    summarized,
                    before,
                    after,
                } => self.note_compaction(summarized, before, after),
                AgentEvent::TranscriptSync { messages, summary } => {
                    self.persist_transcript(messages, summary)
                }
                AgentEvent::RequestBreakdown(b) => {
                    crate::providers::log_http(&format!(
                        "request breakdown: system={}B history={}B user={}B provider_state={}B tools={}B total={}B",
                        b.system_bytes,
                        b.history_bytes,
                        b.user_bytes,
                        b.provider_state_bytes,
                        b.tool_schema_bytes,
                        b.total_bytes,
                    ));
                }
                AgentEvent::SubagentStart { id, task } => {
                    self.handle_subagent_start(id, task);
                    self.dirty = true;
                }
                AgentEvent::SubagentThinking { id, text } => {
                    if let Some(chat) = self.subagent_chats.get_mut(&id)
                        && let Some(Segment::Thinking { text: current, .. }) = chat
                            .iter_mut()
                            .find(|segment| matches!(segment, Segment::Thinking { live: true, .. }))
                    {
                        current.push_str(&text);
                    }
                    self.sub_touch_all(id);
                    self.dirty = true;
                }
                AgentEvent::SubagentText { id, text } => {
                    if let Some(chat) = self.subagent_chats.get_mut(&id)
                        && let Some(Segment::Assistant { text: current, .. }) =
                            chat.iter_mut().rev().find(|segment| {
                                matches!(segment, Segment::Assistant { live: true, .. })
                            })
                    {
                        current.push_str(&text);
                    }
                    self.sub_touch_all(id);
                    self.dirty = true;
                }
                AgentEvent::SubagentToolStart { id, name, summary } => {
                    // Compute positions/texts first (borrowing the chat),
                    // mutate through the aligned helpers afterwards.
                    let preamble: Option<(usize, String)> = if let Some(chat) =
                        self.subagent_chats.get_mut(&id)
                    {
                        chat.iter()
                            .rposition(|segment| {
                                matches!(segment, Segment::Assistant { live: true, .. })
                            })
                            .and_then(|pos| {
                                if let Some(Segment::Assistant { text, .. }) = chat.get_mut(pos) {
                                    let t = text.trim();
                                    if !t.is_empty() {
                                        Some((pos, std::mem::take(text)))
                                    } else {
                                        text.clear();
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                    } else {
                        None
                    };
                    if let Some((pos, p)) = preamble {
                        let trimmed = p.trim();
                        if !trimmed.is_empty() {
                            self.sub_insert(id, pos, Segment::Commentary(trimmed.to_string()));
                        }
                    }
                    // the take/clear above emptied a live row in place (both
                    // the Some and the whitespace-clear None path): bump
                    // revisions so its cached rows are re-rendered
                    self.sub_touch_all(id);
                    let tool_pos = self
                        .subagent_chats
                        .get(&id)
                        .and_then(|chat| {
                            chat.iter().rposition(|segment| {
                                matches!(segment, Segment::Assistant { live: true, .. })
                            })
                        })
                        .unwrap_or_else(|| {
                            self.subagent_chats.get(&id).map(|c| c.len()).unwrap_or(0)
                        });
                    self.sub_insert(
                        id,
                        tool_pos,
                        Segment::Tool {
                            name,
                            args: summary,
                            // child rows are matched by name within one
                            // child's chat (sequential there); no call id
                            call_id: None,
                            ok: None,
                            output: String::new(),
                            diff: None,
                            preview: Vec::new(),
                            preview_total: 0,
                            expanded: false,
                            flash: None,
                        },
                    );
                    self.dirty = true;
                }
                AgentEvent::SubagentToolDone {
                    id,
                    name,
                    summary,
                    ok,
                    diff,
                } => {
                    if let Some(chat) = self.subagent_chats.get_mut(&id)
                        && let Some(Segment::Tool {
                            ok: state,
                            output,
                            diff: current_diff,
                            preview,
                            preview_total,
                            flash,
                            ..
                        }) = chat.iter_mut().rev().find(|segment| matches!(segment, Segment::Tool { name: current, ok: None, .. } if current == &name))
                    {
                        *state = Some(ok);
                        *output = summary;
                        *current_diff = diff;
                        (*preview, *preview_total) =
                            view::tool_preview(current_diff.as_deref(), output.as_str());
                        *flash = Some(std::time::Instant::now());
                    }
                    self.sub_touch_all(id);
                    self.dirty = true;
                }
                AgentEvent::SubagentDone { id, ok, output } => {
                    crate::tui::event_log::log(
                        "SUBAGENT",
                        format!("done id={id} ok={ok} output_len={}", output.chars().count()),
                    );
                    if let Some((_, _, status, current, _)) = self
                        .subagents
                        .iter_mut()
                        .find(|(sid, _, _, _, _)| *sid == id)
                    {
                        *status = if ok {
                            "completed".into()
                        } else {
                            "failed".into()
                        };
                        *current = output.clone();
                    }
                    if let Some(pos) = self
                        .segments
                        .iter()
                        .rposition(|s| matches!(s, Segment::Subagent { id: sid, .. } if *sid == id))
                        && let Some(Segment::Subagent {
                            status,
                            output: current,
                            ..
                        }) = self.segments.get_mut(pos)
                    {
                        *status = if ok {
                            "completed".into()
                        } else {
                            "failed".into()
                        };
                        *current = output;
                        self.touch_segment(pos);
                    }
                    if let Some(chat) = self.subagent_chats.get_mut(&id) {
                        for segment in chat {
                            match segment {
                                Segment::Thinking { live, .. }
                                | Segment::Assistant { live, .. } => *live = false,
                                _ => {}
                            }
                        }
                    }
                    // the finished child needs no folding anymore: the flat
                    // transcript shows every row, stored indices stay valid
                    // because no more rows land after Done
                    self.sub_touch_all(id);
                    self.dirty = true;
                }
                AgentEvent::ToolStart {
                    name,
                    summary,
                    call_id,
                } => {
                    self.clear_retry_state();
                    self.handle_tool_start(name, summary, Some(call_id));
                }
                AgentEvent::ToolNotice {
                    name,
                    summary,
                    ok,
                    diff,
                    call_id,
                } => {
                    self.handle_tool_notice(name, summary, ok, diff, Some(call_id));
                }
                AgentEvent::Checkpoint { label } => {
                    self.last_checkpoint = Some(label);
                    self.dirty = true;
                }
                AgentEvent::Todos(items) => {
                    self.todos = items;
                    self.dirty = true;
                }
                AgentEvent::AskUser { id, questions } => {
                    // Inline in chat as an ordinary message: no overlay, no
                    // modal menu, so a small window never covers the history
                    // and the mouse target is the chat row itself.
                    self.push_ask_segment(id, questions);
                }
                AgentEvent::StepCurrent { step } => {
                    // the loop moved the session's current step (§2.2.3):
                    // persist the mirror so resume and the next run agree
                    self.session.current_step_id = step;
                    self.session.save().ok();
                }
                AgentEvent::Approval {
                    id,
                    command,
                    reason,
                } => {
                    self.open_menu(Menu::Approval {
                        id,
                        command,
                        reason,
                    });
                    self.dirty = true;
                }
                AgentEvent::Diagnostics { count } => {
                    self.lsp_diagnostics = count;
                    if count > 0 {
                        self.status(
                            &format!(
                                "LSP: {count} diagnostic{}",
                                if count == 1 { "" } else { "s" }
                            ),
                            StatusKind::Warn,
                        );
                    }
                    self.dirty = true;
                }
                AgentEvent::Retry {
                    attempt,
                    delay_secs,
                    error,
                } => {
                    if !self.retry_notified {
                        self.retry_notified = true;
                        // the full text of the first failure goes into the chat:
                        // the status bar below keeps only a truncated indicator,
                        // while here it stays readable and copyable (click to unfold)
                        self.push_segment(Segment::Status {
                            text: format!("request failed — retrying with backoff: {error}"),
                            kind: StatusKind::Err,
                            expanded: false,
                            // retracted when the turn completes successfully;
                            // a terminal failure keeps it (see finish_turn_inner)
                            transient: true,
                        });
                        if self.prev_turn_ok {
                            crate::agent::notify::windows_toast(
                                "sqwai",
                                &format!(
                                    "request failed — retrying for up to 1h (esc stops): {}",
                                    truncate_chars(&error, 90)
                                ),
                            );
                        }
                    }
                    self.retry_line = Some(format!("retry #{attempt} in {delay_secs}s — {error}"));
                    self.dirty = true;
                }
                AgentEvent::FallbackSwitched { from, to } => {
                    self.push_segment(Segment::Status {
                        text: format!("primary model '{from}' failed; switched to fallback '{to}'"),
                        kind: StatusKind::Warn,
                        expanded: false,
                        transient: false,
                    });
                    self.session.model_key = to.clone();
                    if let Some(mc) = self.cfg.models.get(&to) {
                        self.model_cfg = mc.clone();
                        self.session.context_limit = mc.context;
                    }
                    self.retry_line = None;
                    self.retry_notified = false;
                    self.status(
                        &format!("switched to fallback model: {to}"),
                        StatusKind::Warn,
                    );
                    self.dirty = true;
                }
                AgentEvent::Completed(res) => {
                    crate::tui::event_log::log(
                        "TURN",
                        format!("Completed ok={} aborted={}", res.is_ok(), self.aborted),
                    );
                    // Esc aborts the request, but the provider task may still
                    // race to deliver a buffered successful completion. Once
                    // abort was requested, that completion is stale and must
                    // not replace the visible turn with the previous answer.
                    if self.aborted {
                        self.finish_turn(Err("aborted".into()));
                        return;
                    }
                    match res {
                        Ok(outcome) => self.finish_turn_ok(outcome),
                        Err(e) => self.finish_turn(Err(e)),
                    }
                    return;
                }
            }
        }
    }

    /// Settle rows left running when their turn ended: a dropped done-event
    /// must never leave a spinner turning forever. Failed, not vanished —
    /// the row is the only trace the call existed.
    fn retire_unfinished_rows(&mut self) {
        for i in 0..self.segments.len() {
            let orphaned = matches!(&self.segments[i], Segment::Tool { ok: None, .. })
                || matches!(
                    &self.segments[i],
                    Segment::Subagent { status, .. }
                        if status.as_str() != "completed" && status.as_str() != "failed"
                );
            if !orphaned {
                continue;
            }
            match self.segments.get_mut(i) {
                Some(Segment::Tool { ok, .. }) => *ok = Some(false),
                Some(Segment::Subagent { status, .. }) => *status = "failed".into(),
                _ => {}
            }
            self.touch_segment(i);
        }
    }

    fn freeze_thinking(&mut self, index: usize) {
        if let Some(Segment::Thinking {
            started,
            duration_ms,
            live,
            ..
        }) = self.segments.get_mut(index)
        {
            if let Some(started_at) = *started {
                *duration_ms = started_at.elapsed().as_millis() as u64;
            }
            *live = false;
        }
        self.touch_segment(index);
    }

    /// append reasoning text, opening a fresh thinking row when none is open.
    /// This keeps multiple reasoning blocks separate and interleaved with the
    /// tool calls that follow them (think -> tool -> think -> tool -> answer).
    fn handle_thinking_delta(&mut self, t: String) {
        if !self.thinking_open {
            self.flush_assistant_preamble_to_commentary();
            self.thinking_open = true;
            // reasoning precedes the answer: insert before the live assistant
            let pos = self
                .segments
                .iter()
                .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
                .unwrap_or(self.segments.len());
            self.insert_segment(
                pos,
                Segment::Thinking {
                    text: String::new(),
                    expanded: false,
                    started: Some(std::time::Instant::now()),
                    duration_ms: 0,
                    live: true,
                },
            );
            self.thinking_idx = Some(pos);
        }
        if let Some(i) = self.thinking_idx
            && let Some(Segment::Thinking { text, started, .. }) = self.segments.get_mut(i)
        {
            if started.is_none() {
                *started = Some(std::time::Instant::now());
            }
            text.push_str(&t);
            self.touch_segment(i);
        }
        self.dirty = true;
    }

    /// If the model streamed conversational text/preamble before issuing a tool
    /// call, fold that text into an activity group `Commentary` row instead of
    /// leaving it rendered below the tool activity or letting it be overwritten
    /// by the final answer. Commentary is always visible and never counts as a
    /// tool call in the activity header.
    fn flush_assistant_preamble_to_commentary(&mut self) {
        self.reveal_chars(usize::MAX);
        let mut text = std::mem::take(&mut self.assistant_buf);
        let pos = self
            .segments
            .iter()
            .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }));
        if let Some(pos) = pos {
            if text.is_empty()
                && let Some(Segment::Assistant { text: seg_text, .. }) = self.segments.get(pos)
            {
                text = seg_text.clone();
            }
            if let Some(Segment::Assistant { text: seg_text, .. }) = self.segments.get_mut(pos) {
                seg_text.clear();
            }
            self.touch_segment(pos);
        }
        let trimmed = text.trim();
        if !trimmed.is_empty()
            && let Some(pos) = pos
        {
            self.insert_segment(pos, Segment::Commentary(trimmed.to_string()));
            self.dirty = true;
        }
    }

    /// close any open reasoning block, then insert a running tool row above the
    /// live answer (tool -> result -> answer). A tool call ends the current
    /// reasoning block, so the next ThinkingDelta opens its own row instead of
    /// piling onto the previous one.
    fn handle_tool_start(&mut self, name: String, summary: String, call_id: Option<String>) {
        if self.thinking_open {
            if let Some(i) = self.thinking_idx.take() {
                self.freeze_thinking(i);
                let empty = matches!(
                    self.segments.get(i),
                    Some(Segment::Thinking { text, .. }) if text.is_empty()
                );
                if empty {
                    self.remove_segment(i);
                }
            }
            self.thinking_open = false;
        }
        self.flush_assistant_preamble_to_commentary();
        // ask_user has its own inline Q&A segment (AgentEvent::AskUser); a
        // parallel Tool row would duplicate it and its expansion used to be
        // empty because `args` here is only a one-line summary, not the JSON.
        // subagent is the same: its `Segment::Subagent` row is the call
        // row AND the child-chat entry point, so a generic `✓ subagent` row
        // next to it duplicated the call (and counted it twice).
        if name == "ask_user" || name == "subagent" {
            return;
        }
        self.perf.event(&format!("tool_start {name}"));
        let tool = Segment::Tool {
            name,
            args: summary,
            call_id,
            ok: None,
            output: String::new(),
            diff: None,
            preview: Vec::new(),
            preview_total: 0,
            expanded: false,
            flash: None,
        };
        // The model may stream a short preamble before emitting its tool call.
        // Keep tool activity above the live answer so the chat reads in
        // execution order.
        let pos = self
            .segments
            .iter()
            .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
            .unwrap_or(self.segments.len());
        self.insert_segment(pos, tool);
        self.dirty = true;
    }

    /// Register a spawned child agent: bookkeeping, its private transcript,
    /// and one summary row at the CALL SITE — inserted before the live
    /// answer, like tool rows. Appending would strand the row after the
    /// answer once the live slot fills, leaving stray `✓ subagent-N` lines
    /// under finished turns.
    fn handle_subagent_start(&mut self, id: u64, task: String) {
        crate::tui::event_log::log(
            "SUBAGENT",
            format!("start id={id} task_len={}", task.chars().count()),
        );
        self.subagents
            .push((id, task.clone(), "running".into(), String::new(), false));
        self.subagent_chats.insert(
            id,
            vec![
                Segment::User(task.clone()),
                Segment::Thinking {
                    text: String::new(),
                    expanded: false,
                    started: None,
                    duration_ms: 0,
                    live: true,
                },
                Segment::Assistant {
                    text: String::new(),
                    live: true,
                },
            ],
        );
        // Identity for every chat row, aligned with the vec above.
        let mut meta = Vec::new();
        for _ in 0..3 {
            meta.push(SegMeta {
                id: self.alloc_seg_id(),
                rev: 0,
            });
        }
        self.subagent_meta.insert(id, meta);
        let pos = self
            .segments
            .iter()
            .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
            .unwrap_or(self.segments.len());
        self.insert_segment(
            pos,
            Segment::Subagent {
                id,
                task,
                status: "running".into(),
                output: String::new(),
                expanded: false,
            },
        );
    }

    /// attach a tool result to the running row opened by handle_tool_start,
    /// or create a closed row if the start event was missed.
    fn handle_tool_notice(
        &mut self,
        name: String,
        summary: String,
        ok: bool,
        diff: Option<String>,
        call_id: Option<String>,
    ) {
        // answered inline above; no Tool row exists for it by design
        if name == "ask_user" {
            return;
        }
        // subagent has no generic Tool row either (see handle_tool_start).
        // Without this the "start event was missed" fallback below would
        // re-create exactly the row that was suppressed, already closed.
        if name == "subagent" {
            return;
        }
        self.perf.event(&format!("tool_done {name} ok={ok}"));
        // close the row opened by ToolStart: exact call id first (parallel
        // same-name calls), then the legacy name match for rows predating
        // ids; fall back to a new row when the start event was missed
        let hit = call_id
            .as_ref()
            .and_then(|wanted| {
                self.segments.iter().position(|s| {
                    matches!(s, Segment::Tool { call_id: row_id, ok: None, .. } if row_id.as_ref() == Some(wanted))
                })
            })
            .or_else(|| {
                self.segments.iter().rposition(|s| {
                    matches!(s, Segment::Tool { name: n, ok: None, .. } if *n == name)
                })
            });
        match hit {
            Some(i) => {
                if let Some(Segment::Tool {
                    ok: slot,
                    output,
                    diff: dslot,
                    preview,
                    preview_total,
                    flash,
                    ..
                }) = self.segments.get_mut(i)
                {
                    *slot = Some(ok);
                    *output = summary;
                    *dslot = diff;
                    (*preview, *preview_total) =
                        view::tool_preview(dslot.as_deref(), output.as_str());
                    // one-shot finish wave (green/red sweep, then static)
                    *flash = Some(std::time::Instant::now());
                }
                self.touch_segment(i);
            }
            None => {
                self.flush_assistant_preamble_to_commentary();
                let (preview, preview_total) =
                    view::tool_preview(diff.as_deref(), summary.as_str());
                let tool = Segment::Tool {
                    name,
                    args: String::new(),
                    call_id,
                    ok: Some(ok),
                    output: summary,
                    diff,
                    preview,
                    preview_total,
                    expanded: false,
                    // arrived already finished: play the wave from now
                    flash: Some(std::time::Instant::now()),
                };
                let pos = self
                    .segments
                    .iter()
                    .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
                    .unwrap_or(self.segments.len());
                self.insert_segment(pos, tool);
            }
        }
        self.dirty = true;
    }

    /// stream the final answer; close any open reasoning block first so it no
    /// longer receives later (errant) thinking deltas.
    fn handle_text_delta(&mut self, t: String) {
        if self.thinking_open {
            if let Some(i) = self.thinking_idx.take() {
                self.freeze_thinking(i);
                let empty = matches!(
                    self.segments.get(i),
                    Some(Segment::Thinking { text, .. }) if text.is_empty()
                );
                if empty {
                    self.remove_segment(i);
                }
            }
            self.thinking_open = false;
        }
        // queue for the typewriter reveal instead of showing at once
        self.pending_reveal.push_str(&t);
        self.dirty = true;
    }

    /// agent finished with a full outcome (final answer + tool turns)
    fn finish_turn_ok(&mut self, outcome: AgentOutcome) {
        // The agent owns the authoritative conversation. It never contains a
        // system turn: the system block is rebuilt per request and never
        // persisted (the session also refuses one defensively).
        //
        // Compaction and hard-trim drop a message prefix (optionally
        // prepending a summary), so positional attachments from before the
        // replacement no longer address the same turns — rebase them first.
        // Plain appends (the common case) leave every index valid.
        if outcome.messages.len() < self.session.messages.len() {
            self.session
                .rebase_turn_attachments(self.session.messages.len() - outcome.messages.len());
        }
        //
        // The outcome only authorizes a visible answer if it advanced past
        // the plain answer already on screen: a tool-cancelled turn ends Ok
        // with no new assistant message (only tool traffic), and backfilling
        // the session's last — previous turn's — answer would stamp it into
        // this turn's slot, duplicating it. Ordinals, not content: a model
        // legitimately repeating the previous answer word-for-word still
        // advanced the conversation.
        let prev_plain = self
            .session
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant && m.tool_calls.is_empty())
            .count();
        let mut ord = 0;
        let mut last_ord = 0;
        for m in &outcome.messages {
            if m.role == Role::Assistant && m.tool_calls.is_empty() {
                ord += 1;
                last_ord = ord;
            }
        }
        let advanced = last_ord > prev_plain;
        self.session.messages = outcome.messages;
        self.session.summary = outcome.summary;
        // the transcript was replaced wholesale; the token estimate is stale
        self.session.refresh_estimate();
        self.todos = outcome.todos;
        if !outcome.plan_todos.is_empty() {
            self.todos = outcome.plan_todos;
        }
        self.session.checkpoints.extend(outcome.journal);
        self.finish_turn_inner(Ok(()), advanced);
    }

    fn is_subagent_row(segment: &Segment) -> bool {
        matches!(segment, Segment::Subagent { .. })
            || matches!(segment, Segment::Tool { name, .. } if name == "subagent")
    }

    /// True while a tool call is executing right now — the row `ToolStart`
    /// opened and `ToolNotice` has not yet closed. Mutating calls run alone
    /// (§3.1), but same-turn subagent calls overlap, so any open row counts.
    /// This is what Esc uses to decide between a cooperative per-tool cancel
    /// (§3.7) and the hard whole-turn abort: there is nothing "mid-tool" to
    /// cancel while the model is only streaming text.
    fn tool_running(&self) -> bool {
        self.segments
            .iter()
            .rev()
            .any(|s| matches!(s, Segment::Tool { ok: None, .. }))
    }

    fn clear_busy_statuses(&mut self) {
        // the busy notice is a toast now: drop it if it is still up
        if self
            .toast
            .as_ref()
            .is_some_and(|t| t.text == Self::BUSY_STATUS)
        {
            self.toast = None;
            self.dirty = true;
        }
    }

    fn clear_subagent_ui_on_stop(&mut self) {
        self.subagents.clear();
        let removed: Vec<usize> = self
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| Self::is_subagent_row(s))
            .map(|(i, _)| i)
            .collect();
        if !removed.is_empty() {
            self.retain_segments(|s| !Self::is_subagent_row(s));
            // Group ranges index the transcript. Shift them past the removals
            // instead of dropping the folds the user already made.
            for g in &mut self.activity_groups {
                let before = removed.iter().filter(|i| **i < g.seg_start).count();
                let inside = removed
                    .iter()
                    .filter(|i| **i >= g.seg_start && **i < g.seg_end)
                    .count();
                g.seg_start -= before;
                g.seg_end -= before + inside;
            }
            // Keep summaries in sync with rows removed during abort. This is
            // done after all ranges shift so the slice uses current indices.
            // The tally is shared with the group builder: a hand-rolled count
            // here used to disagree with it (inline questions were
            // silently dropped).
            for g in &mut self.activity_groups {
                let run = self
                    .segments
                    .get(g.seg_start..g.seg_end)
                    .unwrap_or_default();
                (g.calls, g.thinking, g.errors) = Self::tally_activity(run);
            }
        }
        self.turn_started = None;
        self.dirty = true;
    }

    /// The contiguous run of `Thinking`/`Tool` segments immediately preceding
    /// the last assistant answer — the working content of one agent turn.
    /// Thinking and tools are interleaved, so the run is exactly "everything
    /// between the previous turn and this turn's answer".
    /// Trailing run of work segments (thinking/tool rows) for the activity
    /// group, as `(start, end)` with `end` exclusive.
    ///
    /// Anchored on the work itself, not on the answer: a turn that fails
    /// before streaming anything has tool rows and no `Assistant` slot (the
    /// empty live slot is dropped at finish), so anchoring on the answer
    /// found either nothing or — worse — a previous turn's answer, and the
    /// new group overlapped the old one. Status notes are skipped over: they
    /// belong to no group and must neither break the run nor join it.
    /// `start` never reaches back past the last finalized group.
    fn trailing_work_run(&self) -> Option<(usize, usize)> {
        let floor = self
            .activity_groups
            .iter()
            .map(|g| g.seg_end)
            .max()
            .unwrap_or(0)
            .min(self.segments.len());
        Self::trailing_work_run_in(&self.segments, floor)
    }

    /// Same scan over any transcript: subagent chats fold with the same
    /// rules as the main one. `floor` is where the previous group ended.
    ///
    /// `Segment::Subagent` is transparent to the scan like `Commentary`: it is
    /// a call row, so it joins the run — but the scan must also *cross* it,
    /// or every row above a delegated call is stranded outside the group
    /// (no header, no fold, no click).
    fn trailing_work_run_in(segs: &[Segment], floor: usize) -> Option<(usize, usize)> {
        let floor = floor.min(segs.len());
        let mut end = segs.len();
        while end > floor && matches!(segs[end - 1], Segment::Status { .. }) {
            end -= 1;
        }
        // an answer closes the run but is not part of it; without one, the
        // run simply extends to the tail
        if end > floor && matches!(segs[end - 1], Segment::Assistant { .. }) {
            end -= 1;
        }
        let mut start = end;
        while start > floor
            && matches!(
                segs[start - 1],
                Segment::Thinking { .. }
                    | Segment::Tool { .. }
                    | Segment::Commentary(_)
                    | Segment::AskUser { .. }
                    | Segment::Subagent { .. }
            )
        {
            start -= 1;
        }
        if start == end {
            // neither reasoning nor tool calls: a bare answer is not activity
            return None;
        }
        // Commentary alone (without tools/thinking/questions) is just prose,
        // not activity — it must not create a `0 calls` group.
        let has_work = segs[start..end].iter().any(|s| {
            matches!(
                s,
                Segment::Thinking { .. }
                    | Segment::Tool { .. }
                    | Segment::AskUser { .. }
                    | Segment::Subagent { .. }
            )
        });
        if !has_work {
            return None;
        }
        Some((start, end))
    }

    /// Summarize one run of working segments into an `ActivityGroup`.
    fn build_activity_group(&self, (seg_start, seg_end): (usize, usize)) -> ActivityGroup {
        // The activity header measures the full turn (including provider and
        // tool latency); individual thinking rows use their own frozen timer.
        let duration_ms = self
            .turn_started
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        Self::build_activity_group_in(
            &self.segments,
            (seg_start, seg_end),
            duration_ms,
            self.turn_user_index,
        )
    }

    /// Count calls/thinking/errors over one run of work segments. Shared by
    /// the group builder and the abort-time recount so the two can never
    /// disagree on what a group holds.
    fn tally_activity(segs: &[Segment]) -> (usize, usize, usize) {
        let (mut calls, mut thinking, mut errors) = (0usize, 0usize, 0usize);
        for seg in segs {
            match seg {
                Segment::Tool {
                    ok: Some(false), ..
                } => {
                    calls += 1;
                    errors += 1;
                }
                Segment::Tool { .. } => calls += 1,
                // a question is a tool call awaiting the user; a delegated
                // child is a tool call awaiting its answer. Both count with
                // the rest of the turn's work.
                Segment::AskUser { .. } | Segment::Subagent { .. } => calls += 1,
                Segment::Thinking { .. } => thinking += 1,
                // Commentary is prose folded into the group for context; it is
                // always visible and never counts as a tool call.
                Segment::Commentary(_) => {}
                _ => {}
            }
        }
        (calls, thinking, errors)
    }

    /// Same summary over any transcript (subagent chats carry no turn user).
    fn build_activity_group_in(
        segs: &[Segment],
        (seg_start, seg_end): (usize, usize),
        duration_ms: u64,
        turn_user: Option<usize>,
    ) -> ActivityGroup {
        let (calls, thinking, errors) = Self::tally_activity(&segs[seg_start..seg_end]);
        ActivityGroup {
            seg_start,
            seg_end,
            calls,
            thinking,
            duration_ms,
            errors,
            rejected: 0,
            turn_user,
        }
    }

    /// Freeze the turn that just finished into a group. Called once the
    /// segments are final, so the stored indices stay valid. The view never
    /// folds anymore — groups survive purely as session-summary data.
    fn finalize_activity_group(&mut self) {
        let Some(run) = self.trailing_work_run() else {
            self.turn_started = None;
            return;
        };
        let group = self.build_activity_group(run);
        self.activity_groups.push(group);
        self.turn_started = None;
    }

    fn finish_turn(&mut self, res: Result<(), String>) {
        self.finish_turn_inner(res, true);
    }

    /// Live transcript persistence for hard-abort survival: the loop owns
    /// the transcript, and a killed task takes it along. Same rebase
    /// discipline as finish_turn_ok — compaction may have replaced the
    /// prefix mid-turn.
    fn persist_transcript(
        &mut self,
        messages: Vec<crate::providers::Message>,
        summary: Option<String>,
    ) {
        if messages.len() < self.session.messages.len() {
            self.session
                .rebase_turn_attachments(self.session.messages.len() - messages.len());
        }
        self.session.messages = messages;
        if summary.is_some() {
            self.session.summary = summary;
        }
        self.session.refresh_estimate();
        self.session.save().ok();
    }

    /// `advanced` tells the Ok path whether the outcome added a new plain
    /// assistant message past the one already on screen (`finish_turn_ok`
    /// computes it against the pre-replacement session). Callers that did
    /// not replace the session pass false, so a turn that died silently can
    /// never backfill the previous answer into the live slot.
    fn finish_turn_inner(&mut self, res: Result<(), String>, advanced: bool) {
        crate::tui::event_log::log(
            "TURN",
            format!(
                "finish streaming={} segments={} groups={} result={:?}",
                self.streaming,
                self.segments.len(),
                self.activity_groups.len(),
                res.as_ref().err()
            ),
        );
        self.clear_busy_statuses();
        // an aborted/errored turn can leave a question with nobody waiting
        // for its answer — freeze it instead of leaving a live ghost.
        if res.is_err() {
            let note = if res.as_ref().is_err_and(|e| e == "aborted") {
                "(no answer — stopped)"
            } else {
                "(no answer — turn failed)"
            };
            self.freeze_active_ask(note);
        } else if self.active_ask_seg().is_some() {
            self.freeze_active_ask("(no answer)");
        }
        if res.as_ref().is_err_and(|error| error == "aborted") {
            self.clear_subagent_ui_on_stop();
        }
        // flush whatever the typewriter has not revealed yet
        self.reveal_chars(usize::MAX);
        let text = std::mem::take(&mut self.assistant_buf);
        self.thinking_open = false;
        self.thinking_idx = None;
        for i in 0..self.segments.len() {
            if matches!(self.segments[i], Segment::Thinking { live: true, .. }) {
                self.freeze_thinking(i);
            }
        }
        // a finished turn cannot have running rows: a lost done-event would
        // spin them forever, so orphaned calls settle as failed here
        self.retire_unfinished_rows();
        // never render "(0 chars)" ghosts
        let empties: Vec<usize> = self
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s, Segment::Thinking { text, .. } if text.is_empty()))
            .map(|(i, _)| i)
            .collect();
        for i in empties.into_iter().rev() {
            self.remove_segment(i);
        }
        // On a successful completion the agent already replaced the session
        // wholesale (finish_turn_ok), so the last assistant message there is
        // authoritative and becomes the visible answer — but only if the
        // outcome actually advanced past the answer on screen (see
        // finish_turn_ok). On an abort/error the session was NOT updated and
        // still holds the *previous* turn's answer — backfilling from it
        // would stamp that old answer into the slot for the turn we just
        // stopped, duplicating it. In that case trust only what was actually
        // streamed this turn (assistant_buf).
        let final_text = if res.is_ok() && advanced {
            self.session
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant && m.tool_calls.is_empty())
                .map(|m| m.content.clone())
        } else {
            None
        };
        if let Some(t) = final_text {
            if let Some(pos) = self
                .segments
                .iter()
                .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
            {
                self.set_segment(
                    pos,
                    Segment::Assistant {
                        text: t.clone(),
                        live: false,
                    },
                );
            }
        } else if !text.is_empty() {
            if let Some(pos) = self
                .segments
                .iter()
                .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
            {
                self.set_segment(
                    pos,
                    Segment::Assistant {
                        text: text.clone(),
                        live: false,
                    },
                );
            }
            // the partial answer that was actually streamed is preserved
            self.session.push(Role::Assistant, text.clone());
        } else {
            // nothing was streamed and we have no authoritative answer: drop the
            // empty live slot instead of leaving a blank assistant line
            if let Some(pos) = self
                .segments
                .iter()
                .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
            {
                self.remove_segment(pos);
            }
        }
        self.streaming = false;
        self.aborted = false;
        self.agent = None;
        self.retry_line = None;
        self.prev_turn_ok = matches!(res, Ok(()));
        // A turn that did not finish cleanly leaves the provider holding a
        // history the host no longer vouches for (§3.3): bootstrap the next
        // request instead of continuing it. An aborted turn additionally
        // arms the resume notice — the next turn must continue the plan,
        // not silently redo settled steps.
        if res.is_err() {
            self.context_bootstrap_pending = true;
        }
        let aborted_turn = matches!(&res, Err(e) if e == "aborted");
        if aborted_turn || res.is_ok() {
            self.session.prev_turn_aborted = aborted_turn;
            self.session.save().ok();
        }
        let turn_note = match &res {
            Ok(()) => None,
            Err(e) if e == "tui closed" => None,
            Err(e) if e == "aborted" => Some(("stopped".to_string(), false)),
            Err(e) => Some((format!("error: {e}"), true)),
        };
        if let Some((note, is_error)) = &turn_note {
            let kind = if *is_error {
                StatusKind::Err
            } else {
                StatusKind::Info
            };
            // The chat segment is the whole signal: it lands as the last row of
            // the transcript, which is where the eye already is, and it is what
            // survives a reload. A toast on top of it only duplicated the word.
            self.push_segment(Segment::Status {
                text: note.clone(),
                kind,
                expanded: false,
                transient: false,
            });
        }
        if res.is_ok() {
            // a retry that recovered leaves no scar: the transient notice
            // pushed on the first failure is retracted now that the answer
            // exists. Terminal failures keep theirs (pushed above or on the
            // retry path) — an unfinished answer must stay explained.
            self.retract_transient_status();
            self.retry_notified = false;
        }
        // Segment indices are stable from here on: the empty-thinking cleanup
        // and the answer backfill above have all run.
        self.finalize_activity_group();
        // Persist the presentation summary only after the group was finalized.
        // Saving earlier lost it across a restart and restored bare tool rows.
        if let Some((text, is_error)) = turn_note
            && let Some(user_index) = self.turn_user_index.take()
        {
            self.session.turn_notes.push(TurnNote {
                user_index,
                text,
                is_error,
            });
        }
        self.session.activity = self
            .activity_groups
            .iter()
            .map(|g| ActivitySummary {
                calls: g.calls,
                thinking: g.thinking,
                duration_ms: g.duration_ms,
                errors: g.errors,
                rejected: g.rejected,
                user_index: g.turn_user,
            })
            .collect();
        self.session.save().ok();
        self.dirty = true;
        // queued follow-ups (typed mid-turn): a natural finish or an abort
        // sends the first one as a new turn right away; a failed turn
        // leaves the queue for a manual resend (Enter on empty input)
        let send_queued = !self.pending_queue.is_empty()
            && self.menu_stack.is_empty()
            && (matches!(&res, Ok(())) || matches!(&res, Err(e) if e == "aborted"));
        if send_queued {
            let next = self.pending_queue.remove(0);
            self.input = Self::fresh_input(next);
            self.submit();
        }
    }

    /// revert the last `n` mutating actions and reopen steps whose evidence was reverted.
    /// AB `/export`: markdown + JSON dump of the session into
    /// `.sqwai/exports/`. Synchronous and local — no model, no network.
    fn export_session(&mut self) {
        let session = self.session.id.to_string();
        let out = crate::agent::export::export_session(
            &self.project_root,
            &session,
            &self.model_cfg.id,
            &self.session.messages,
        );
        let (md, json) = crate::agent::export::export_paths(&self.project_root, &session);
        let write = || -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
            if let Some(parent) = md.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&md, &out.markdown)?;
            std::fs::write(
                &json,
                serde_json::to_string_pretty(&out.json).unwrap_or_default(),
            )?;
            Ok((md, json))
        };
        match write() {
            Ok((md, json)) => self.status(
                &format!("exported: {} + {}", md.display(), json.display()),
                StatusKind::Ok,
            ),
            Err(error) => self.status(&format!("export failed: {error:#}"), StatusKind::Err),
        }
        self.dirty = true;
    }

    /// AB `/why`: answer a why-question from journal evidence, narrated by
    /// the model. Gather is synchronous (no evidence → status, no call);
    /// narration flies in a background task, polled per tick.
    fn start_why(&mut self, question: String) {
        let root = self.project_root.clone();
        let session = self.session.id.to_string();
        let evidence = crate::agent::why::gather(&root, &session, &question);
        if evidence.is_empty() {
            self.status(
                "no evidence for that question in this session",
                StatusKind::Warn,
            );
            return;
        }
        let prompt = crate::agent::why::render_prompt(&evidence, &question);
        let model_id = self.model_cfg.id.clone();
        let provider = self.provider.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let outcome = match crate::agent::why::micro_call(
                &provider,
                &model_id,
                crate::agent::why::NARRATOR_SYSTEM,
                &prompt,
                crate::agent::why::NARRATOR_MAX_TOKENS,
                crate::agent::why::NARRATOR_TIMEOUT_SECS,
            )
            .await
            {
                Ok(answer) => WhyOutcome::Answered { text: answer },
                Err(error) => WhyOutcome::Failed(format!("{error:#}")),
            };
            let _ = tx.send(outcome);
        });
        self.why_rx = Some(rx);
        self.status("answering from session evidence…", StatusKind::Info);
        self.dirty = true;
    }

    /// Collect a finished `/why` answer: one durable row plus status.
    fn poll_why(&mut self) {
        let outcome = match self.why_rx.as_mut() {
            None => return,
            Some(rx) => match rx.try_recv() {
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    self.why_rx = None;
                    self.status("why-answer task died", StatusKind::Err);
                    self.dirty = true;
                    return;
                }
                Ok(outcome) => {
                    self.why_rx = None;
                    outcome
                }
            },
        };
        match outcome {
            WhyOutcome::Answered { text } => {
                self.push_segment(Segment::Assistant { text, live: false });
                self.status("answered from session evidence", StatusKind::Ok);
            }
            WhyOutcome::Failed(error) => {
                self.status(&format!("why-answer failed: {error}"), StatusKind::Err);
            }
        }
        self.dirty = true;
    }

    /// S1 preflight shared by every undo entry: cancel still-registered
    /// children (normally none — they join inside their `task` call),
    /// refuse while background shells are alive (the writer lock stops
    /// dispatch, not an already-running process), and take the restore
    /// lock. `None` means blocked — the caller returns without touching
    /// the tree.
    fn undo_preflight(&mut self) -> Option<crate::agent::undo_guard::RestoreGuard> {
        let cancelled = crate::agent::undo_guard::cancel_running_children();
        if cancelled > 0 {
            self.status(
                &format!("cancelled {cancelled} running subagent(s) before undo"),
                StatusKind::Info,
            );
        }
        let running = crate::agent::tools::bg_running_commands();
        if !running.is_empty() {
            let ids: Vec<String> = running
                .iter()
                .map(|(id, session, cmd)| format!("job {id} ({session}): {cmd}"))
                .collect();
            self.status(
                &format!(
                    "undo refused: {} background job(s) still running — \
                     kill them with bash_kill or wait for completion: {}",
                    running.len(),
                    ids.join("; "),
                ),
                StatusKind::Warn,
            );
            return None;
        }
        Some(crate::agent::undo_guard::hold_restore())
    }

    fn undo(&mut self, n: usize) {
        let n = n.max(1);
        if self.session.checkpoints.is_empty() {
            self.status("nothing to undo", StatusKind::Info);
            return;
        }
        let idx = self.session.checkpoints.len().saturating_sub(n);
        let (sha, label) = self.session.checkpoints[idx].clone();
        let root = self.project_root.clone();
        let git_snapshots = crate::agent::checkpoints::available_in(&root, self.cfg.undo.shadow);
        // Scope the restore to what the host recorded as its own writes across
        // the checkpoints being undone. Without this, undo reverts the whole
        // tree to the snapshot and silently discards anything the user edited
        // in their own editor while the agent was working (§2.5).
        let undone: Vec<String> = self.session.checkpoints[idx..]
            .iter()
            .map(|(sha, _)| sha.clone())
            .collect();
        let recorded =
            crate::agent::journal::Journal::recorded_writes(&root, &undone).unwrap_or_default();
        let scoped = !recorded.is_empty();
        // A window can mix file edits with a `bash` call. Scoping to the
        // journal then silently leaves the shell command's effects in place,
        // so count the checkpoints that contributed nothing and say so.
        let unrecorded = if scoped {
            let with_records =
                crate::agent::journal::Journal::checkpoints_with_writes(&root, &undone)
                    .unwrap_or_default();
            undone
                .iter()
                .filter(|sha| !with_records.contains(*sha))
                .count()
        } else {
            0
        };
        // Layer 1 (§2.5): the pre-images the host stored before each write.
        // No git needed, and it is the only path that works in a project that
        // is not a repository at all.
        let pre_images =
            crate::agent::journal::Journal::recorded_pre_images(&root, &undone).unwrap_or_default();
        let have_blobs = pre_images.iter().any(|item| {
            item.blob_before
                .as_deref()
                .is_some_and(|id| crate::agent::blobs::has(&root, id))
        });
        if !git_snapshots && !have_blobs {
            self.status(
                "nothing to undo from: no shadow snapshot and no stored pre-images",
                StatusKind::Warn,
            );
            return;
        }
        // S1: nothing mutates under the restore below — not dispatch
        // (writer lock), not running children (cancelled), not live
        // shells (refused above while any survive).
        let _restore = match self.undo_preflight() {
            Some(guard) => guard,
            None => return,
        };
        if !git_snapshots || (scoped && have_blobs) {
            // Prefer the blob store when it covers the window: it reverts
            // exactly the recorded writes, needs no repository, and cannot be
            // invalidated by the user's own `git gc`.
            self.undo_from_blobs(&root, idx, &label, &pre_images, unrecorded);
            return;
        }
        let targets: Vec<crate::agent::checkpoints::Target> = if scoped {
            recorded
                .into_iter()
                .map(|(path, agent_hash)| crate::agent::checkpoints::Target { path, agent_hash })
                .collect()
        } else {
            // Nothing recorded — a `bash` mutation, whose effects the host
            // cannot enumerate. Fall back to the snapshot-vs-worktree diff and
            // say so, rather than pretending the scope is known.
            crate::agent::checkpoints::changed_files(&root, self.cfg.undo.shadow, &sha)
                .unwrap_or_default()
                .into_iter()
                .map(|path| crate::agent::checkpoints::Target {
                    path,
                    agent_hash: None,
                })
                .collect()
        };

        match crate::agent::checkpoints::restore_paths_in(
            &root,
            self.cfg.undo.shadow,
            &sha,
            &targets,
        ) {
            Ok(report) => {
                let touched = report.touched();
                let reopened_steps =
                    reopen_undone_steps(&root, &self.session.id.to_string(), &touched, &sha);
                self.session.checkpoints.truncate(idx);
                // The provider's copy of the conversation still contains the
                // work that was just reverted, and a continuation reference
                // would carry the model straight back to it. The next request
                // sends the transcript the host owns instead.
                self.context_bootstrap_pending = true;
                self.session.save().ok();

                let mut note = format!("undo: reverted '{label}' ({} file(s)", touched.len());
                if !report.deleted.is_empty() {
                    note.push_str(&format!(", {} removed", report.deleted.len()));
                }
                note.push(')');
                if !reopened_steps.is_empty() {
                    note.push_str(&format!("; reopened {} step(s)", reopened_steps.len()));
                }
                let kind = if !report.skipped.is_empty() {
                    note.push_str(&format!(
                        "; left alone, changed outside sqwai: {}",
                        report.skipped.join(", ")
                    ));
                    StatusKind::Warn
                } else if !scoped {
                    note.push_str("; scope not narrowed (no file records for this checkpoint)");
                    StatusKind::Warn
                } else if unrecorded > 0 {
                    note.push_str(&format!(
                        "; {unrecorded} checkpoint(s) had no file records, their effects remain"
                    ));
                    StatusKind::Warn
                } else {
                    StatusKind::Ok
                };
                self.status(&note, kind);
                self.dirty = true;
            }
            Err(e) => self.status(&format!("undo failed: {e:#}"), StatusKind::Err),
        }
    }

    /// Revert one plan step (§2.5, `/undo step N`).
    ///
    /// Only layer 1 is involved: the journal knows which `file_diff` records
    /// belong to the step and the blob store holds their pre-images. A file a
    /// later step has since written is refused by name rather than reverted,
    /// because putting the old bytes back would undo that later step too.
    fn undo_step(&mut self, step: &str) {
        let root = self.project_root.clone();
        self.undo_step_in(&root, step);
    }

    /// `undo_step` against an explicit root. Production passes the project
    /// root; tests pass a temp dir — the unscoped fallback scans every
    /// journal under the root, so running this against the repo while an
    /// agent session is active reverts (or deletes!) that session's live
    /// files. Hermetic by construction, never ambient.
    fn undo_step_in(&mut self, root: &std::path::Path, step: &str) {
        // resolve the session's own plan first: step numbers restart per
        // plan, so the revert must be scoped to this plan, not scanned
        // project-wide (no plan → the old unscoped scan, nothing better)
        let plan_id =
            crate::plan::open_active_for_session(root, Some(&self.session.id.to_string()))
                .ok()
                .flatten()
                .map(|plan| plan.id);
        // no plan to scope to → the old unscoped scan (nothing better exists)
        let revert = match &plan_id {
            Some(pid) => crate::agent::journal::Journal::step_pre_images_in(root, Some(pid), step),
            None => crate::agent::journal::Journal::step_pre_images(root, step),
        };
        let revert = match revert {
            Ok(revert) => revert,
            Err(e) => {
                self.status(&format!("undo step {step}: {e:#}"), StatusKind::Err);
                return;
            }
        };
        if revert.files.is_empty() {
            let note = if revert.written_since.is_empty() {
                format!("undo step {step}: this step recorded no file writes")
            } else {
                format!(
                    "undo step {step}: nothing revertible — later steps rewrote {}",
                    revert.written_since.join(", ")
                )
            };
            self.status(&note, StatusKind::Warn);
            return;
        }
        // S1: same preflight as the full undo — the blob restore below is
        // a tree mutation like any other.
        let _restore = match self.undo_preflight() {
            Some(guard) => guard,
            None => return,
        };

        match crate::agent::checkpoints::restore_from_blobs(root, &revert.files) {
            Ok(report) => {
                let touched = report.touched();
                let mut reopened: Vec<String> = Vec::new();
                // the same plan the revert was scoped to: reopening anything
                // else would attach this session's undo to foreign work
                if let Some(pid) = &plan_id
                    && let Ok(mut active) = crate::plan::open(root, pid)
                    && crate::plan::reopen_for_undo(
                        &mut active,
                        step,
                        format!("reopened by undo of step {step}"),
                    )
                    .is_ok()
                {
                    let args = serde_json::json!({
                        "ids": [step],
                        "reason": format!("reopened by undo of step {step}"),
                    });
                    let sid = self.session.id.to_string();
                    if crate::plan::commit(root, &sid, &mut active, "reopen", "host", true, args)
                        .is_ok()
                    {
                        reopened.push(step.to_string());
                    }
                }
                if let Ok(mut journal) =
                    crate::agent::journal::Journal::open(root, &self.session.id.to_string())
                {
                    // host-written, like every other undo record (§2.2.2)
                    journal.set_attribution(None, None, "host");
                    let _ = journal.append_undo_step(step, &touched, &reopened);
                }
                // The provider's copy of the conversation still contains the
                // reverted work (§3.3, #50).
                self.context_bootstrap_pending = true;

                let mut note = format!("undo step {step}: {} file(s)", touched.len());
                if !report.deleted.is_empty() {
                    note.push_str(&format!(", {} removed", report.deleted.len()));
                }
                if !reopened.is_empty() {
                    note.push_str("; step reopened");
                }
                let kind = if !revert.written_since.is_empty() {
                    note.push_str(&format!(
                        "; left alone, rewritten by a later step: {}",
                        revert.written_since.join(", ")
                    ));
                    StatusKind::Warn
                } else if !report.skipped.is_empty() {
                    note.push_str(&format!(
                        "; left alone, changed outside sqwai: {}",
                        report.skipped.join(", ")
                    ));
                    StatusKind::Warn
                } else if !report.no_pre_image.is_empty() {
                    note.push_str(&format!(
                        "; not revertible, no stored pre-image: {}",
                        report.no_pre_image.join(", ")
                    ));
                    StatusKind::Warn
                } else {
                    StatusKind::Ok
                };
                self.status(&note, kind);
                self.dirty = true;
            }
            Err(e) => self.status(&format!("undo step {step} failed: {e:#}"), StatusKind::Err),
        }
    }

    /// Undo through layer 1: write back the pre-images the host stored, remove
    /// what the window created, and leave anything edited outside sqwai alone.
    fn undo_from_blobs(
        &mut self,
        root: &std::path::Path,
        idx: usize,
        label: &str,
        pre_images: &[crate::agent::journal::PreImage],
        unrecorded: usize,
    ) {
        // Called from `undo` after its own preflight; re-checking is cheap
        // and keeps this entry safe on its own.
        let _restore = match self.undo_preflight() {
            Some(guard) => guard,
            None => return,
        };
        match crate::agent::checkpoints::restore_from_blobs(root, pre_images) {
            Ok(report) => {
                let touched = report.touched();
                let sha = self
                    .session
                    .checkpoints
                    .get(idx)
                    .map(|(sha, _)| sha.clone())
                    .unwrap_or_default();
                let reopened_steps =
                    reopen_undone_steps(root, &self.session.id.to_string(), &touched, &sha);
                self.session.checkpoints.truncate(idx);
                self.context_bootstrap_pending = true;
                self.session.save().ok();

                let mut note = format!(
                    "undo: reverted '{label}' from stored pre-images ({} file(s)",
                    touched.len()
                );
                if !report.deleted.is_empty() {
                    note.push_str(&format!(", {} removed", report.deleted.len()));
                }
                note.push(')');
                if !reopened_steps.is_empty() {
                    note.push_str(&format!("; reopened {} step(s)", reopened_steps.len()));
                }
                let kind = if !report.skipped.is_empty() {
                    note.push_str(&format!(
                        "; left alone, changed outside sqwai: {}",
                        report.skipped.join(", ")
                    ));
                    StatusKind::Warn
                } else if !report.no_pre_image.is_empty() {
                    // written by the host but with nothing stored to put back
                    note.push_str(&format!(
                        "; not revertible, no stored pre-image: {}",
                        report.no_pre_image.join(", ")
                    ));
                    StatusKind::Warn
                } else if unrecorded > 0 {
                    note.push_str(&format!(
                        "; {unrecorded} checkpoint(s) had no file records, their effects remain"
                    ));
                    StatusKind::Warn
                } else {
                    StatusKind::Ok
                };
                self.status(&note, kind);
                self.dirty = true;
            }
            Err(e) => self.status(&format!("undo failed: {e:#}"), StatusKind::Err),
        }
    }

    fn page(&mut self, dir: i32) {
        self.scroll(-dir * 20);
    }

    fn scroll(&mut self, delta: i32) {
        let had_sel = self.sel.is_some();
        self.sel = None;
        let h = self.last_chat.height.max(1) as usize;
        let max = self.cache_lines.len().saturating_sub(h);
        // while following, the viewport sits at the bottom; every tick moves the
        // absolute top by exactly delta lines, so scrolling past the edges can
        // never build up "dead" distance to unwind later
        let cur = if self.follow {
            max
        } else {
            self.view_top.min(max)
        };
        let next = (cur as isize - delta as isize).clamp(0, max as isize) as usize;
        let old = (self.follow, self.view_top);
        if next >= max {
            self.follow = true;
        } else {
            self.follow = false;
            self.view_top = next;
        }
        // Scrolling into a clamped edge changes nothing on screen: skip the
        // frame instead of presenting an empty diff (each present costs a
        // terminal roundtrip). A cleared selection still needs its repaint.
        if (self.follow, self.view_top) != old || had_sel {
            self.dirty = true;
        }
    }
}

/// Pick the summary for a restored group: anchored by the turn's user message
/// when available, legacy sequential order otherwise. Never loses a summary —
/// the final fallback takes the first remaining entry.
fn take_summary(
    remaining: &mut Vec<ActivitySummary>,
    anchored_mode: bool,
    turn_user: Option<usize>,
) -> Option<ActivitySummary> {
    if remaining.is_empty() {
        return None;
    }
    if !anchored_mode {
        return Some(remaining.remove(0));
    }
    let pos = turn_user
        .and_then(|user| remaining.iter().position(|a| a.user_index == Some(user)))
        .or_else(|| remaining.iter().position(|a| a.user_index.is_none()))
        .unwrap_or(0);
    Some(remaining.remove(pos))
}

fn fmt_bytes(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{} KB", n / 1024)
    } else {
        format!("{n} B")
    }
}

fn fmt_k(n: u64) -> String {
    if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        format!("{n}")
    }
}

fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else if n == 0 {
        String::new()
    } else {
        // reserve one slot for the ellipsis so the result is never wider than
        // `n` — otherwise a "…" pushed the boxed tool output one column past its
        // border and wrap_tagged split the overflow char (and the right │) onto
        // a new row, breaking the frame on every long line of a large file.
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

fn short_id(s: &Session) -> String {
    s.id.to_string()[..8].to_string()
}

fn fmt_date(t: chrono::DateTime<chrono::Utc>) -> String {
    t.with_timezone(&chrono::Local)
        .format("%d.%m %H:%M")
        .to_string()
}

/// time of day for grouped session rows (the section header carries
/// the date, so the column keeps HH:MM only)
fn fmt_time(t: chrono::DateTime<chrono::Utc>) -> String {
    t.with_timezone(&chrono::Local).format("%H:%M").to_string()
}

fn on_off(v: bool) -> String {
    if v { "on" } else { "off" }.to_string()
}
