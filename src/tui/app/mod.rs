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
use crate::config::{Config, EffortLevel, ModelConfig};
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

mod events;
mod forms;
mod menus;
mod perf;
#[cfg(test)]
mod tests;
mod view;

use forms::FormField;
use menus::{Menu, MenuAction};
use view::{ActivityGroup, AskRow, CellPos, ProposalRow, SegMeta, Segment, Selection, StoredView};

use menus::COMMANDS;

#[derive(Debug, Clone)]
pub(super) struct StartupData {
    pub version: &'static str,
    pub project_path: String,
    pub git_branch: Option<String>,
    pub git_modified: Option<usize>,
    pub model: String,
    pub active_plan: Option<ActivePlanInfo>,
    pub last_session: Option<RecentSessionInfo>,
    pub memory: MemoryInfo,
    pub recent: Vec<RecentSessionInfo>,
    pub warnings: Vec<String>,
    pub has_sqwai_dir: bool,
}

#[derive(Debug, Clone)]
pub(super) struct ActivePlanInfo {
    pub title: String,
    pub current_step: usize,
    pub total_steps: usize,
    pub status_text: String,
}

#[derive(Debug, Clone)]
pub(super) struct RecentSessionInfo {
    pub date: String,
    pub title: String,
    pub outcome: String,
}

#[derive(Debug, Clone, Default)]
pub(super) struct MemoryInfo {
    pub has_memory_md: bool,
    pub latest_diary: Option<String>,
    pub graph_ready: bool,
}

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
}

const WORKING_SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

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
    /// `step N/M` for the active plan, or empty when there is none. Refreshed
    /// when the plan can have changed (see `refresh_plan_label`) rather than
    /// re-read and re-parsed from disk on every redraw.
    pub(super) plan_step_label: String,

    input: TextArea<'static>,
    segments: Vec<Segment>,

    streaming: bool,
    aborted: bool,
    agent: Option<AgentHandle>,
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
    /// id of the plan proposal awaiting accept/decline, if any
    active_proposal_id: Option<u64>,
    /// mouse hover target inside the active proposal, for highlight
    proposal_hover: Option<ProposalRow>,
    assistant_buf: String,
    /// arrived text not yet revealed to the screen (typewriter effect)
    pending_reveal: String,
    thinking_open: bool,
    thinking_idx: Option<usize>,
    mode: Mode,
    /// true while the current session has no user turns yet: the startup
    /// screen is shown for every such session, not only at launch
    startup: bool,
    pub(super) startup_data: Option<StartupData>,
    /// background collector for startup_data; Some while a collection is
    /// still running, so /new and session switches never block the UI
    startup_data_rx: Option<std::sync::mpsc::Receiver<StartupData>>,
    pub(super) last_ctrl_c: Option<Instant>,
    /// transient bottom-bar notice (3s): every status/error lands here, the
    /// chat stays clean. A new notice replaces the current one.
    toast: Option<Toast>,
    /// previous turn ended successfully — gates retry notifications
    prev_turn_ok: bool,
    /// already toasted for the current retry cycle
    retry_notified: bool,
    /// live retry indicator rendered in the status bar (single updating line)
    retry_line: Option<String>,
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
    /// The running turn's group is expanded by default; the user can fold it
    /// manually before the turn ends.
    live_group_collapsed: bool,

    // command popup
    hover: Option<String>,
    popup_dismiss: bool,
    popup_scroll: usize,
    popup_rows: Vec<(u16, String)>,

    // providers/models menu (Ctrl+P)
    menu_stack: Vec<Menu>,
    menu_sel: usize,
    /// scroll offset for long list menus
    menu_scroll: usize,
    /// list-window height from the last draw: wheel/nav clamp against it
    menu_visible_rows: usize,
    /// fixed hint line under a list menu (not part of the scrolled rows)
    menu_footer_text: Option<String>,
    menu_rows: Vec<(Line<'static>, MenuAction)>,
    menu_rect: Rect,
    /// pinned-section width the Sessions rows were built for (see
    /// `sessions_frame_w`): draw_menu rebuilds once when the real card
    /// disagrees, so a stale rect never desyncs header from content
    sessions_frame_built_w: u16,
    /// click/hover targets of the effort slider (rect → SELECTABLE index),
    /// rebuilt on every slider draw; empty when another menu is open
    effort_hits: Vec<(Rect, usize)>,
    form_fields: Vec<FormField>,
    form_focus: usize,
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
    agents_click: Option<(u16, u16)>,
    status_y: u16,

    // mouse selection
    press: Option<CellPos>,
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
        let root = std::env::current_dir().unwrap_or_default();
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
        let root = std::env::current_dir().unwrap_or_default();
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
    fn system_block(&self) -> Vec<crate::providers::SystemPart> {
        use crate::providers::SystemPart;
        let mut parts = vec![SystemPart::cached(self.stable_prefix.clone())];
        if !self.session_environment.is_empty() {
            parts.push(SystemPart::cached(self.session_environment.clone()));
        }
        let root = std::env::current_dir().unwrap_or_default();
        if let Some(plan) = crate::prompts::plan_block(&root, Some(&self.session.id.to_string())) {
            parts.push(SystemPart::cached(plan));
        }
        // The anchor is host-generated from the plan and this session's
        // journal. It is rebuilt every turn so resume/compaction never relies
        // on a model-written summary.
        parts.push(SystemPart::volatile(crate::agent::context::anchor(
            &root,
            &self.session.id.to_string(),
        )));
        if !self.session.messages.is_empty()
            && let Some(notice) =
                crate::agent::context::resume_notice(&root, &self.session.id.to_string())
        {
            parts.push(SystemPart::volatile(notice));
        }
        let runtime = crate::prompts::runtime_context();
        if !runtime.is_empty() {
            parts.push(SystemPart::volatile(runtime));
        }
        parts
    }

    /// Recompute the plan label from disk.
    ///
    /// Called where the active plan can have changed — construction, a plan
    /// operation reported by the agent, the end of a turn, `/undo`, and any
    /// slash command — instead of on every frame from inside the renderer.
    pub(super) fn refresh_plan_label(&mut self) {
        self.plan_step_label = self
            .session_plan()
            .and_then(|plan| {
                let current = plan
                    .steps
                    .iter()
                    .position(|step| step.status == plan::StepStatus::InProgress)?;
                Some(format!("step {}/{}", current + 1, plan.steps.len()))
            })
            .unwrap_or_default();
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
                price_in: None,
                price_out: None,
                fallback: None,
            });
        let resolved = cfg.resolve_provider(&model_cfg)?;
        let provider = providers::create(&resolved)?;

        let startup_data = if startup {
            Some(Self::collect_startup_data(&cfg, &model_cfg, read_only))
        } else {
            None
        };

        let project_root = std::env::current_dir().unwrap_or_default();
        if !read_only {
            // Heal a crash between a journal intent and its plan store
            // (§2.1.4, §3.7) before any plan read. No-op on a clean tree.
            let _ = crate::plan::replay(&project_root);
        }
        let cwd_label = project_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();

        let mut app = Self {
            project_root,
            cwd_label,
            plan_step_label: String::new(),
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
            active_proposal_id: None,
            proposal_hover: None,
            assistant_buf: String::new(),
            pending_reveal: String::new(),
            thinking_open: false,
            thinking_idx: None,
            mode: Mode::Act,
            startup,
            startup_data,
            startup_data_rx: None,
            last_ctrl_c: None,
            read_only,
            toast: None,
            prev_turn_ok: false,
            retry_notified: true, // no toast for the very first turn
            retry_line: None,
            last_checkpoint: None,
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
            menu_scroll: 0,
            menu_visible_rows: 0,
            menu_footer_text: None,
            menu_rows: Vec::new(),
            menu_rect: Rect::default(),
            sessions_frame_built_w: 0,
            effort_hits: Vec::new(),
            form_fields: Vec::new(),
            form_focus: 0,
            sessions: Vec::new(),
            sessions_filter: String::new(),
            ef_click: None,
            provider_checks: std::collections::HashMap::new(),
            provider_check_rx: None,
            builtin_update_rx: None,
            maintain_rx: None,
            effort_observed_ignored: None,
            agents_click: None,
            status_y: 0,
            press: None,
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
            live_group_collapsed: false,
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
        app.refresh_plan_label();
        app.stable_prefix = app.stable_prefix();
        app.rebuild_session_environment();
        app.context_bootstrap_pending = true;
        app.start_builtin_update(false);
        Ok(app)
    }

    /// render persisted messages as chat segments (used on start and on resume)
    fn load_history_segments(&mut self) {
        // Tool calls and their results are part of the durable provider
        // transcript. Keep the rendered row keyed by call id so batched calls
        // and providers that return results out of order are restored safely.
        let mut pending_tools = std::collections::HashMap::<String, usize>::new();
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
                            true,
                        );
                    }
                    previous_user = Some(message_index);
                    turn_user = Some(message_index);
                    self.push_segment(Segment::User(m.content.clone()));
                }
                Role::Assistant if m.tool_calls.is_empty() => {
                    if let Some(seg_start) = work_start.take() {
                        // A saved session is never streaming: historical work
                        // must begin folded, even if it ended with an error.
                        self.close_restored_group(
                            seg_start,
                            turn_user,
                            &mut remaining_summaries,
                            anchored_mode,
                            false,
                        );
                    }
                    self.push_segment(Segment::Assistant {
                        text: m.content.clone(),
                        live: false,
                    });
                }
                Role::Assistant => {
                    work_start.get_or_insert(self.segments.len());
                    let trimmed = m.content.trim();
                    if !trimmed.is_empty() {
                        self.push_segment(Segment::Commentary(trimmed.to_string()));
                    }
                    for call in &m.tool_calls {
                        let idx = self.push_segment(Segment::Tool {
                            name: call.name.clone(),
                            args: crate::agent::tools::call_summary(&call.name, &call.args),
                            ok: None,
                            output: String::new(),
                            diff: None,
                            preview: Vec::new(),
                            preview_total: 0,
                            expanded: false,
                        });
                        pending_tools.insert(call.id.clone(), idx);
                    }
                }
                Role::Tool => {
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
                true,
            );
        }
        // The panel is derived from the active structured plan; legacy session
        // to-do state is intentionally not loaded.
    }

    /// Close a restored work run `[seg_start..]` into an `ActivityGroup`,
    /// preferring the summary anchored to this turn's user message.
    /// `failed` groups (stopped turns) restore expanded, like the live UI.
    fn close_restored_group(
        &mut self,
        seg_start: usize,
        turn_user: Option<usize>,
        remaining: &mut Vec<ActivitySummary>,
        anchored_mode: bool,
        failed: bool,
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
            expanded: failed || saved.errors > 0,
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
        input.set_cursor_style(Style::new().bg(ratatui::style::Color::White).fg(ratatui::style::Color::Black));
        input.set_selection_style(Style::new().bg(ratatui::style::Color::Cyan).fg(ratatui::style::Color::Black));
        input
    }

    fn input_block() -> Block<'static> {
        Block::default()
    }

    /// Width of the `› ` input marker gutter (0 on degenerate widths).
    /// The marker occupies the gutter on the top input row only; the
    /// textarea itself is shifted right by this on every row.
    pub(super) fn input_marker_w(&self) -> u16 {
        if self.last_input.width >= 6 {
            2
        } else {
            0
        }
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
        let mut last_build =
            Instant::now() - crate::tui::presenter::MIN_PRESENT_INTERVAL;
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
            let presenter_gone =
                !presenter_alive.load(std::sync::atomic::Ordering::Relaxed);
            if presenter_gone && !self.renderer_dead_reported {
                self.renderer_dead_reported = true;
                self.status("renderer thread died - restart sqwai", StatusKind::Err);
            }
            if self.enter_gate.flush(Instant::now()) {
                self.submit();
            }
            self.poll_input(&ev_rx)?;
            self.poll_startup_data();
            self.poll_agent();
            self.poll_provider_check();
            self.poll_builtin_update();
            self.poll_maintain();
            // typewriter: reveal queued answer text gradually, catching up when
            // the queue grows faster than the reveal speed
            if !self.pending_reveal.is_empty() {
                let step = if self.cfg.ui.typewriter {
                    let queued = self.pending_reveal.chars().count();
                    // Reveal at a steady pace while the provider streams. The
                    // old queue-based catch-up could dump the whole answer in
                    // one tick when a chunk arrived faster than the TUI.
                    if queued > 96 { 12 } else { 2 }
                } else {
                    usize::MAX
                };
                self.dirty |= self.reveal_chars(step);
            }
            if self.toast.as_ref().is_some_and(|t| Instant::now() >= t.until) {
                self.toast = None;
                self.dirty = true;
            }
            let animating = self.streaming
                || self.tool_running()
                || self.toast.is_some()
                || matches!(self.cur_menu(), Some(Menu::TestAnims));
            if animating {
                // fixed 20 FPS animation rate from the wall clock, not per
                // loop iteration: bursts would otherwise fast-forward it
                self.spinner_tick =
                    (self.tick_origin.elapsed().as_millis() / 50) as usize;
                self.dirty = true;
            }
            if self.dirty
                && last_build.elapsed() >= crate::tui::presenter::MIN_PRESENT_INTERVAL
            {
                let area = ratatui::layout::Rect::new(
                    0,
                    0,
                    self.term_size.width,
                    self.term_size.height,
                );
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
                    let mut buf = ratatui::buffer::Buffer::empty(area);
                    self.render_into(&mut buf, area);
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
                        buf,
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
                    Some(prev) => rep
                        .presented_at
                        .saturating_duration_since(prev)
                        .as_micros(),
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
                let wraps = crate::tui::markdown::WRAP_TAGGED_CALLS
                    .load(std::sync::atomic::Ordering::Relaxed);
                self.perf.frame(stat, renders, wraps);
            }
        }
        // Shutdown must never wait for a provider request. A final diary
        // entry is host-only here; model-written diary prose belongs to the
        // explicit diary/compaction paths, not the exit path.
        if self.session_has_messages() && !self.read_only {
            let root = std::env::current_dir().unwrap_or_default();
            let _ = crate::agent::diary::append_entry(
                &root,
                crate::agent::diary::today(),
                &self.session.id.to_string(),
                "session_end",
                None,
            );
            self.session.save().ok();
        } else if self.session_has_messages() {
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
        if let Some(Segment::AskUser { answered, .. }) = self.segments.get_mut(seg) {
            *answered = Some(text.clone());
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
        if let Some(Segment::AskUser { answered, .. }) = self.segments.get_mut(seg) {
            *answered = Some(note.to_string());
        }
        self.touch_segment(seg);
        self.active_ask_id = None;
        self.ask_hover = None;
        self.ask_custom_focus = None;
        self.dirty = true;
    }

    /// the live plan proposal awaiting accept/decline, if it still exists.
    /// Resolved by tool-call id: rows inserted later shift indices.
    pub(super) fn active_proposal_seg(&self) -> Option<usize> {
        let id = self.active_proposal_id?;
        self.segments.iter().position(|s| {
            matches!(
                s,
                Segment::PlanProposal {
                    id: qid,
                    decided: None,
                    ..
                } if *qid == id
            )
        })
    }

    /// insert a proposal segment in execution order — before the live answer,
    /// like tool rows — so the finished turn folds it into its activity group
    /// instead of leaving it rendered below the answer.
    pub(super) fn push_proposal_segment(&mut self, id: u64, draft: crate::plan::Plan) {
        self.flush_assistant_preamble_to_commentary();
        self.freeze_active_proposal();
        let seg = Segment::PlanProposal {
            id,
            draft,
            decided: None,
        };
        let pos = self
            .segments
            .iter()
            .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
            .unwrap_or(self.segments.len());
        self.insert_segment(pos, seg);
        self.active_proposal_id = Some(id);
        self.proposal_hover = None;
        self.follow = true;
        self.dirty = true;
    }

    /// open the draft preview popup (same view as /plan, read-only)
    pub(super) fn open_proposal_preview(&mut self) {
        let Some(seg) = self.active_proposal_seg() else {
            return;
        };
        let draft = match self.segments.get(seg) {
            Some(Segment::PlanProposal { draft, .. }) => draft.clone(),
            _ => return,
        };
        self.open_menu(Menu::PlanPreview { draft });
    }

    /// freeze the active proposal and deliver the verdict to the agent
    pub(super) fn proposal_answer(&mut self, accept: bool) {
        let Some(seg) = self.active_proposal_seg() else {
            return;
        };
        let id = match self.segments.get(seg) {
            Some(Segment::PlanProposal { id, .. }) => *id,
            _ => return,
        };
        if let Some(Segment::PlanProposal { decided, .. }) = self.segments.get_mut(seg) {
            *decided = Some(accept);
        }
        self.touch_segment(seg);
        if let Some(agent) = &self.agent {
            let _ = agent
                .control
                .try_send(ControlMsg::PlanAnswer { id, accept });
        }
        self.active_proposal_id = None;
        self.proposal_hover = None;
        self.follow = true;
        self.dirty = true;
    }

    /// the agent turn ended while a proposal was open: never leave a ghost
    /// behind. A dangling proposal always freezes as declined — answering
    /// for the user is not something the host may do (§10).
    fn freeze_active_proposal(&mut self) {
        let Some(seg) = self.active_proposal_seg() else {
            self.active_proposal_id = None;
            return;
        };
        if let Some(Segment::PlanProposal { decided, .. }) = self.segments.get_mut(seg) {
            *decided = Some(false);
        }
        self.touch_segment(seg);
        self.active_proposal_id = None;
        self.proposal_hover = None;
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
        let verb = if summarized { "summarized" } else { "trimmed" };
        self.status(
            &format!(
                "context compacted ({verb}): {} → {} tok",
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
                if let Some(active) = self.session_plan() {
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
            } else {
                return;
            }
        }
        if self.streaming {
            // Whitelist of commands that are safe to run while the agent is
            // streaming: read-only UI, menus, and viewing the plan. Mutating
            // commands (graph rebuild, plan edits, undo, etc.) stay blocked and
            // show the busy notice. User messages (non-commands) are always
            // blocked while streaming.
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
        self.push_segment(Segment::User(text.clone()));
        self.session.push(Role::User, &text);
        self.turn_user_index = Some(self.session.messages.len().saturating_sub(1));

        // The system block is assembled per request and travels separately
        // from the transcript: nothing here is ever written to the session.
        let system = self.system_block();
        let msgs: Vec<PMessage> = self.session.messages.clone();
        let root = std::env::current_dir().unwrap_or_default();
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
            plan_mode: self.mode == Mode::Plan,
            context_limit: self.session.context_limit,
            enable_tools: true,
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
            diary: self.cfg.diary.clone(),
            memory: self.cfg.memory.clone(),
            compaction: self.cfg.compaction.clone(),
            plan_limits: self.cfg.plan,
            shadow_store: self.cfg.undo.shadow,
            subagent_depth: 0,
            parent_step: None,
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
        self.live_group_collapsed = false;
        // show the thinking placeholder right away so the indicator is visible
        // from turn start even before any reasoning deltas arrive
        let tpos = self.push_segment(Segment::Thinking {
            text: String::new(),
            expanded: false,
            // Do not start the thinking stopwatch at request start: connection
            // latency before the first reasoning delta is not model thinking.
            started: None,
            duration_ms: 0,
            live: true,
        });
        self.thinking_idx = Some(tpos);
        self.thinking_open = true;
        self.push_segment(Segment::Assistant {
            text: String::new(),
            live: true,
        });
        self.jump_to_bottom_on_typing();
        self.dirty = true;
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
            root: std::env::current_dir().unwrap_or_default(),
            session_id: self.session.id.to_string(),
            blocked_patterns: Vec::new(),
            plan_mode: false,
            context_limit: self.session.context_limit,
            enable_tools: false,
            read_only: self.read_only,
            mcp: Default::default(),
            lsp: Default::default(),
            previous_response_id: None,
            summary: self.session.summary.clone(),
            compact_only: true,
            diary: self.cfg.diary.clone(),
            memory: self.cfg.memory.clone(),
            compaction: self.cfg.compaction.clone(),
            plan_limits: self.cfg.plan,
            shadow_store: self.cfg.undo.shadow,
            subagent_depth: 0,
            parent_step: None,
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
        // persist the session we are leaving — but skip a brand-new empty one
        // (e.g. the startup stub), otherwise opening an existing session from
        // the startup screen would litter the list with an extra empty file
        if self.session_has_messages() {
            self.session.save().ok();
        }
        // resolve the session's model against the current config
        if !self.cfg.models.contains_key(&s.model_key) {
            s.model_key = self.cfg.default_model.clone();
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
        self.active_ask_id = None;
        self.ask_hover = None;
        self.active_proposal_id = None;
        self.proposal_hover = None;
        self.ask_custom_focus = None;
        self.rebuild_session_environment();
        self.load_history_segments();
        // an empty session shows the startup screen like a fresh launch;
        // data is collected in the background — switching must not block
        self.startup = self.session.messages.is_empty();
        if self.startup {
            self.refresh_startup_data();
        }
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
        self.session = Session::new(self.cfg.default_model.clone(), ctx);
        self.session.plan_id = crate::plan::open_active_for_session(
            &self.project_root,
            Some(&self.session.id.to_string()),
        )
        .ok()
        .flatten()
        .map(|plan| plan.id);
        self.context_bootstrap_pending = true;
        // a fresh empty session shows the startup screen again; the data
        // refresh runs in the background so /new returns instantly
        self.startup = true;
        self.refresh_startup_data();
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
        self.active_ask_id = None;
        self.ask_hover = None;
        self.active_proposal_id = None;
        self.proposal_hover = None;
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
        let root = std::env::current_dir().unwrap_or_default();
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
                self.cfg.default_model = key.to_string();
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
                } else if rest.split_whitespace().nth(1) == Some("animations") {
                    self.open_menu(Menu::TestAnims);
                } else {
                    self.status("/test takes: animations", StatusKind::Warn);
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
                let root = std::env::current_dir().unwrap_or_default();
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
            "/graph-rebuild" => {
                if self.streaming {
                    self.show_busy_status();
                } else {
                    let root = std::env::current_dir().unwrap_or_default();
                    match crate::agent::graph_index::rebuild_project(&root) {
                        Ok(report) => self.status(
                            &format!(
                                "graph rebuilt: {} indexed, {} removed, {} skipped",
                                report.indexed_files, report.removed_files, report.skipped_files
                            ),
                            StatusKind::Ok,
                        ),
                        Err(error) => self
                            .status(&format!("graph rebuild failed: {error:#}"), StatusKind::Err),
                    }
                }
            }
            "/init" => {
                if std::path::Path::new("AGENTS.md").exists() {
                    self.status("AGENTS.md already exists", StatusKind::Warn);
                } else {
                    match std::fs::write("AGENTS.md", crate::prompts::AGENTS_TEMPLATE) {
                        Ok(()) => self.status(
                            "AGENTS.md created — it is sent to the model with every request",
                            StatusKind::Ok,
                        ),
                        Err(e) => {
                            self.status(&format!("init: {e}"), StatusKind::Err)
                        }
                    }
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
            "/diary" => {
                if self.streaming {
                    self.show_busy_status();
                } else if self.read_only {
                    self.status(
                        "project is read-only; diary writes are disabled",
                        StatusKind::Warn,
                    );
                } else {
                    let root = std::env::current_dir().unwrap_or_default();
                    match crate::agent::diary::append_entry(
                        &root,
                        crate::agent::diary::today(),
                        &self.session.id.to_string(),
                        "manual",
                        None,
                    ) {
                        Ok(()) => self.status("diary entry written", StatusKind::Ok),
                        Err(error) => {
                            self.status(&format!("diary write failed: {error:#}"), StatusKind::Err)
                        }
                    }
                }
            }
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
            other if COMMANDS.contains(&other) => {
                self.status(&format!("{other}: not implemented yet"), StatusKind::Warn)
            }
            "" => {}
            other => self.status(&format!("unknown command {other}"), StatusKind::Warn),
        }
        // /plan, /goal, /constraints, /init, /undo and /new can all change the
        // active plan; refreshing once per command is cheaper than the
        // per-frame read this replaces.
        self.refresh_plan_label();
        self.dirty = true;
    }

    /// The plan this session works on: its linked plan while the file is
    /// still readable on disk, otherwise the session-scoped active plan
    /// (which falls back to the most recent one, preserving single-plan
    /// behavior when nothing is linked). Every TUI read or mutation of
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
            Some("abandon") => match self.workable_plan() {
                Ok(mut active) => {
                    active.status = plan::PlanStatus::Abandoned;
                    active.revision += 1;
                    let sid = self.session.id.to_string();
                    let args = serde_json::json!({"id": active.id.clone()});
                    match plan::commit(&root, &sid, &mut active, "cancel", "user", true, args) {
                        Ok(_) => "plan abandoned".to_string(),
                        Err(e) => format!("plan write failed: {e:#}"),
                    }
                }
                Err(message) => message,
            },
            Some("waive") => {
                let index = args.get(1).and_then(|s| s.parse::<usize>().ok());
                let reason = args.get(2..).map(|v| v.join(" ")).unwrap_or_default();
                match (index, reason.trim()) {
                    (Some(index), reason) if !reason.is_empty() => match self.workable_plan() {
                        Ok(mut active) => match plan::waive(&mut active, index, reason) {
                            Ok(()) => {
                                let sid = self.session.id.to_string();
                                let args = serde_json::json!({"index": index, "reason": reason});
                                match plan::commit(
                                    &root,
                                    &sid,
                                    &mut active,
                                    "waive",
                                    "user",
                                    true,
                                    args,
                                ) {
                                    Ok(_) => format!("acceptance {index} waived"),
                                    Err(e) => format!("plan write failed: {e:#}"),
                                }
                            }
                            Err(e) => format!("plan waive rejected [{}]: {}", e.code, e.reason),
                        },
                        Err(message) => message,
                    },
                    _ => "usage: /plan waive <acceptance-index> <reason>".to_string(),
                }
            }
            Some("confirm") => {
                let index = args.get(1).and_then(|s| s.parse::<usize>().ok());
                let reason = args.get(2..).map(|v| v.join(" ")).unwrap_or_default();
                match (index, reason.trim()) {
                    (Some(index), reason) if !reason.is_empty() => match self.workable_plan() {
                        Ok(mut active) => {
                            let sid = self.session.id.to_string();
                            match plan::confirm(&root, &sid, &mut active, index, reason) {
                                Ok(_) => {
                                    let receipt = active
                                        .acceptance
                                        .get(index)
                                        .and_then(|item| item.validation.receipts.last())
                                        .and_then(|r| serde_json::to_value(r).ok());
                                    let mut cargs =
                                        serde_json::json!({"index": index, "reason": reason});
                                    if let Some(receipt) = receipt {
                                        cargs["receipt"] = receipt;
                                    }
                                    match plan::commit(
                                        &root,
                                        &sid,
                                        &mut active,
                                        "confirm",
                                        "user",
                                        true,
                                        cargs,
                                    ) {
                                        Ok(_) => format!("acceptance {index} confirmed"),
                                        Err(e) => format!("plan write failed: {e:#}"),
                                    }
                                }
                                Err(e) => {
                                    format!("plan confirm rejected [{}]: {}", e.code, e.reason)
                                }
                            }
                        }
                        Err(message) => message,
                    },
                    _ => "usage: /plan confirm <acceptance-index> <reason>".to_string(),
                }
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
                self.mode = Mode::Plan;
                self.status("mode: PLAN", StatusKind::Info);
            }
            Some("act") => {
                self.mode = Mode::Act;
                self.status("mode: ACT", StatusKind::Info);
            }
            _ => self.status("usage: /mode plan|act", StatusKind::Warn),
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
        self.toast
            .as_ref()
            .map(|t| (t.text.clone(), t.kind))
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
                    if let Ok(mc) = self.cfg.default_model_config().cloned()
                        && self.model_cfg.id == mc.id
                    {
                        self.model_cfg = mc;
                    }
                    self.build_menu_rows();
                    self.dirty = true;
                    if manual {
                        self.status(
                            &format!(
                                "built-in providers updated ({} models)",
                                new_catalog.models.len()
                            ),
                            StatusKind::Ok,
                        );
                    } else {
                        crate::tui::event_log::log(
                            "PROVIDERS",
                            format!(
                                "built-in providers updated in background ({} models)",
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
                    if manual {
                        self.status(&format!("update failed: {e}"), StatusKind::Err);
                    } else {
                        crate::tui::event_log::log(
                            "PROVIDERS",
                            format!("background update check failed: {e}"),
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
                        let result = if self.aborted {
                            Err("aborted".to_string())
                        } else {
                            Ok(())
                        };
                        self.agent = None;
                        self.finish_turn(result);
                        return;
                    }
                },
                None => return,
            };
            match ev {
                AgentEvent::TextDelta(t) => {
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
                AgentEvent::RequestBreakdown(b) => {
                    crate::providers::log_http(&format!(
                        "request breakdown: system={}B history={}B user={}B tools={}B total={}B",
                        b.system_bytes,
                        b.history_bytes,
                        b.user_bytes,
                        b.tool_schema_bytes,
                        b.total_bytes,
                    ));
                }
                AgentEvent::SubagentStart { id, task } => {
                    self.handle_subagent_start(id, task);
                    if matches!(self.cur_menu(), Some(Menu::Subagents)) {
                        self.build_menu_rows();
                    }
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
                            ok: None,
                            output: String::new(),
                            diff: None,
                            preview: Vec::new(),
                            preview_total: 0,
                            expanded: false,
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
                            ..
                        }) = chat.iter_mut().rev().find(|segment| matches!(segment, Segment::Tool { name: current, ok: None, .. } if current == &name))
                    {
                        *state = Some(ok);
                        *output = summary;
                        *current_diff = diff;
                        (*preview, *preview_total) =
                            view::tool_preview(current_diff.as_deref(), output.as_str());
                    }
                    self.sub_touch_all(id);
                    self.dirty = true;
                }
                AgentEvent::SubagentDone { id, ok, output } => {
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
                    self.sub_touch_all(id);
                    if matches!(self.cur_menu(), Some(Menu::Subagents)) {
                        self.build_menu_rows();
                    }
                    self.dirty = true;
                }
                AgentEvent::ToolStart { name, summary } => {
                    self.handle_tool_start(name, summary);
                }
                AgentEvent::ToolNotice {
                    name,
                    summary,
                    ok,
                    diff,
                } => {
                    self.handle_tool_notice(name, summary, ok, diff);
                }
                AgentEvent::Checkpoint { label } => {
                    self.last_checkpoint = Some(label);
                    self.dirty = true;
                }
                AgentEvent::Todos(items) => {
                    self.todos = items;
                    self.refresh_plan_label();
                    self.dirty = true;
                }
                AgentEvent::AskUser { id, questions } => {
                    // Inline in chat as an ordinary message: no overlay, no
                    // modal menu, so a small window never covers the history
                    // and the mouse target is the chat row itself.
                    self.push_ask_segment(id, questions);
                }
                AgentEvent::PlanProposal { id, draft } => {
                    // Same treatment as a question: inline, in execution
                    // order, folded into the turn's activity afterwards.
                    self.push_proposal_segment(id, draft);
                }
                AgentEvent::PlanAccepted { id } => {
                    // the loop stored the accepted draft: re-link the session
                    // so fork copies the new plan instead of the abandoned one
                    self.session.plan_id = Some(id);
                    self.refresh_plan_label();
                    self.dirty = true;
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
                    });
                    self.session.model_key = to.clone();
                    if let Some(mc) = self.cfg.models.get(&to) {
                        self.model_cfg = mc.clone();
                        self.session.context_limit = mc.context;
                    }
                    self.retry_line = None;
                    self.retry_notified = false;
                    self.status(&format!("switched to fallback model: {to}"), StatusKind::Warn);
                    self.dirty = true;
                }
                AgentEvent::Completed(res) => {
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
    fn handle_tool_start(&mut self, name: String, summary: String) {
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
        // propose_plan is the same: the PlanProposal segment is the surface.
        if name == "ask_user" || name == "propose_plan" {
            return;
        }
        self.perf.event(&format!("tool_start {name}"));
        let tool = Segment::Tool {
            name,
            args: summary,
            ok: None,
            output: String::new(),
            diff: None,
            preview: Vec::new(),
            preview_total: 0,
            expanded: false,
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
    ) {
        // answered inline above; no Tool row exists for it by design
        if name == "ask_user" || name == "propose_plan" {
            return;
        }
        self.perf.event(&format!("tool_done {name} ok={ok}"));
        // close the row opened by ToolStart; fall back to a new one
        let hit = self
            .segments
            .iter()
            .rposition(|s| matches!(s, Segment::Tool { name: n, ok: None, .. } if *n == name));
        match hit {
            Some(i) => {
                if let Some(Segment::Tool {
                    ok: slot,
                    output,
                    diff: dslot,
                    preview,
                    preview_total,
                    ..
                }) = self.segments.get_mut(i)
                {
                    *slot = Some(ok);
                    *output = summary;
                    *dslot = diff;
                    (*preview, *preview_total) =
                        view::tool_preview(dslot.as_deref(), output.as_str());
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
                    ok: Some(ok),
                    output: summary,
                    diff,
                    preview,
                    preview_total,
                    expanded: false,
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
        self.session.messages = outcome.messages;
        self.session.summary = outcome.summary;
        // the transcript was replaced wholesale; the token estimate is stale
        self.session.refresh_estimate();
        self.todos = outcome.todos;
        if !outcome.plan_todos.is_empty() {
            self.todos = outcome.plan_todos;
        }
        self.refresh_plan_label();
        self.session.checkpoints.extend(outcome.journal);
        self.finish_turn(Ok(()));
    }

    fn is_subagent_row(segment: &Segment) -> bool {
        matches!(segment, Segment::Subagent { .. })
            || matches!(segment, Segment::Tool { name, .. } if name == "subagent")
    }

    /// True while a tool call is executing right now — the row `ToolStart`
    /// opened and `ToolNotice` has not yet closed. Only one tool runs at a
    /// time (mutating calls run alone; §3.1), so the most recent segment is
    /// enough to check. This is what Esc uses to decide between a cooperative
    /// per-tool cancel (§3.7) and the hard whole-turn abort: there is nothing
    /// "mid-tool" to cancel while the model is only streaming text.
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
            for g in &mut self.activity_groups {
                g.calls = self.segments[g.seg_start..g.seg_end]
                    .iter()
                    .filter(|s| matches!(s, Segment::Tool { .. }))
                    .count();
                g.thinking = self.segments[g.seg_start..g.seg_end]
                    .iter()
                    .filter(|s| matches!(s, Segment::Thinking { .. }))
                    .count();
                g.errors = self.segments[g.seg_start..g.seg_end]
                    .iter()
                    .filter(|s| {
                        matches!(
                            s,
                            Segment::Tool {
                                ok: Some(false),
                                ..
                            }
                        )
                    })
                    .count();
            }
        }
        self.turn_started = None;
        self.live_group_collapsed = false;
        if matches!(self.cur_menu(), Some(Menu::Subagents)) {
            self.menu_home();
        }
        self.agents_click = None;
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
        let segs = &self.segments;
        let floor = self
            .activity_groups
            .iter()
            .map(|g| g.seg_end)
            .max()
            .unwrap_or(0)
            .min(segs.len());
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
                    | Segment::PlanProposal { .. }
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
                    | Segment::PlanProposal { .. }
            )
        });
        if !has_work {
            return None;
        }
        Some((start, end))
    }

    /// Summarize one run of working segments into an `ActivityGroup`.
    fn build_activity_group(&self, (seg_start, seg_end): (usize, usize)) -> ActivityGroup {
        let mut calls = 0usize;
        let mut thinking = 0usize;
        let mut errors = 0usize;
        for seg in &self.segments[seg_start..seg_end] {
            match seg {
                Segment::Tool {
                    ok: Some(false), ..
                } => {
                    calls += 1;
                    errors += 1;
                }
                Segment::Tool { .. } => calls += 1,
                // a question is a tool call awaiting the user; it folds with
                // the rest of the turn's work
                Segment::AskUser { .. } => calls += 1,
                // same for a plan proposal awaiting accept/decline
                Segment::PlanProposal { .. } => calls += 1,
                Segment::Thinking { .. } => thinking += 1,
                // Commentary is prose folded into the group for context; it is
                // always visible and never counts as a tool call.
                Segment::Commentary(_) => {}
                _ => {}
            }
        }
        // The activity header measures the full turn (including provider and
        // tool latency); individual thinking rows use their own frozen timer.
        let duration_ms = self
            .turn_started
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        ActivityGroup {
            seg_start,
            seg_end,
            calls,
            thinking,
            duration_ms,
            errors,
            rejected: 0,
            expanded: true,
            turn_user: self.turn_user_index,
        }
    }

    /// Freeze the turn that just finished into a group. Called once the
    /// segments are final, so the stored indices stay valid.
    fn finalize_activity_group(&mut self, turn_failed: bool) {
        let Some(run) = self.trailing_work_run() else {
            self.turn_started = None;
            self.live_group_collapsed = false;
            return;
        };
        let mut group = self.build_activity_group(run);
        // A turn that ended badly stays open: the user must see the failure
        // instead of a collapsed summary line.
        group.expanded = turn_failed || group.errors > 0;
        self.activity_groups.push(group);
        self.turn_started = None;
        self.live_group_collapsed = false;
    }

    fn finish_turn(&mut self, res: Result<(), String>) {
        self.clear_busy_statuses();
        // an aborted/errored turn can leave a question with nobody waiting
        // for its answer — freeze it instead of leaving a live ghost.
        // Same for a dangling plan proposal (always declined: the host must
        // not answer for the user).
        if res.is_err() {
            let note = if res.as_ref().is_err_and(|e| e == "aborted") {
                "(no answer — stopped)"
            } else {
                "(no answer — turn failed)"
            };
            self.freeze_active_ask(note);
            self.freeze_active_proposal();
        } else {
            if self.active_ask_seg().is_some() {
                self.freeze_active_ask("(no answer)");
            }
            self.freeze_active_proposal();
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
        // authoritative and becomes the visible answer. On an abort/error the
        // session was NOT updated and still holds the *previous* turn's answer —
        // backfilling from it would stamp that old answer into the slot for the
        // turn we just stopped, duplicating it. In that case trust only what was
        // actually streamed this turn (assistant_buf).
        let final_text = if res.is_ok() {
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
        let turn_failed = res.is_err();
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
            self.status(note, kind);
            // durable turn notes stay in the chat as segments (they are
            // history, restored on reload) — the toast alone is not enough
            self.push_segment(Segment::Status {
                text: note.clone(),
                kind,
                expanded: false,
            });
        }
        // Segment indices are stable from here on: the empty-thinking cleanup
        // and the answer backfill above have all run.
        self.finalize_activity_group(turn_failed);
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
    }

    /// revert the last `n` mutating actions and reopen steps whose evidence was reverted.
    fn undo(&mut self, n: usize) {
        let n = n.max(1);
        if self.session.checkpoints.is_empty() {
            self.status("nothing to undo", StatusKind::Info);
            return;
        }
        let idx = self.session.checkpoints.len().saturating_sub(n);
        let (sha, label) = self.session.checkpoints[idx].clone();
        let root = std::env::current_dir().unwrap_or_default();
        let git_snapshots = crate::agent::checkpoints::available(&root);
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
            crate::agent::checkpoints::changed_files(&root, &sha)
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
        let root = std::env::current_dir().unwrap_or_default();
        let revert = match crate::agent::journal::Journal::step_pre_images(&root, step) {
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

        match crate::agent::checkpoints::restore_from_blobs(&root, &revert.files) {
            Ok(report) => {
                let touched = report.touched();
                let mut reopened: Vec<String> = Vec::new();
                if let Ok(Some(mut active)) =
                    crate::plan::open_active_for_session(&root, Some(&self.session.id.to_string()))
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
                    if crate::plan::commit(&root, &sid, &mut active, "reopen", "host", true, args)
                        .is_ok()
                    {
                        reopened.push(step.to_string());
                    }
                    self.refresh_plan_label();
                }
                if let Ok(mut journal) =
                    crate::agent::journal::Journal::open(&root, &self.session.id.to_string())
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

    /// Start collecting startup screen data on a background thread. The
    /// heavy parts (git subprocesses, session index, plan file) would stall
    /// the UI for seconds when run synchronously inside /new.
    /// Until the result arrives the previous data (if any) keeps rendering.
    fn refresh_startup_data(&mut self) {
        if self.startup_data_rx.is_some() {
            return; // a collection is already running
        }
        let cfg = self.cfg.clone();
        let model_cfg = self.model_cfg.clone();
        let read_only = self.read_only;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let data = Self::collect_startup_data(&cfg, &model_cfg, read_only);
            let _ = tx.send(data);
        });
        self.startup_data_rx = Some(rx);
    }

    /// Pick up a finished background startup-data collection, if any.
    fn poll_startup_data(&mut self) {
        if self.startup_data_rx.is_none() {
            return;
        }
        if let Some(rx) = self.startup_data_rx.take() {
            match rx.try_recv() {
                Ok(data) => {
                    self.startup_data = Some(data);
                    self.dirty = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => self.startup_data_rx = Some(rx),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
            }
        }
    }

    pub(super) fn collect_startup_data(
        cfg: &Config,
        model_cfg: &ModelConfig,
        read_only: bool,
    ) -> StartupData {
        let root = std::env::current_dir().unwrap_or_default();
        let version = env!("CARGO_PKG_VERSION");
        let project_path = shorten_path(&root);
        let (git_branch, git_modified) = collect_git_info(&root);
        let model = model_cfg.id.clone();

        let active_plan_raw = crate::plan::open_active(&root).ok().flatten();
        let has_sqwai_dir = root.join(".sqwai").exists();

        // Memory info
        let has_memory_md = root.join("MEMORY.md").exists();
        let diary_dir = root.join(".sqwai").join("memory");
        let latest_diary = if diary_dir.exists() {
            let mut dates = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&diary_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.ends_with(".md") && name != "MEMORY.md" {
                        let date_str = name.trim_end_matches(".md");
                        if chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d").is_ok() {
                            dates.push(date_str.to_string());
                        }
                    }
                }
            }
            dates.sort();
            dates.pop()
        } else {
            None
        };
        let graph_ready = root.join(".sqwai").join("graph").join("graph.db").exists();
        let memory = MemoryInfo {
            has_memory_md,
            latest_diary,
            graph_ready,
        };

        // Saved sessions
        let sessions = Session::list_visible_headers(10).unwrap_or_default();

        let (active_plan, last_session, recent) = if let Some(plan) = active_plan_raw {
            let in_prog = plan
                .steps
                .iter()
                .position(|s| s.status == crate::plan::StepStatus::InProgress);
            let (curr, st) = if let Some(i) = in_prog {
                (i + 1, "in progress".to_string())
            } else if let Some(i) = plan
                .steps
                .iter()
                .position(|s| s.status == crate::plan::StepStatus::Pending)
            {
                (i + 1, "pending".to_string())
            } else if !plan.steps.is_empty()
                && plan
                    .steps
                    .iter()
                    .all(|s| s.status == crate::plan::StepStatus::Done)
            {
                (plan.steps.len(), "all completed".to_string())
            } else {
                (1, "in progress".to_string())
            };

            let plan_info = ActivePlanInfo {
                title: plan.goal.text.clone(),
                current_step: curr,
                total_steps: plan.steps.len(),
                status_text: st,
            };

            let done = plan
                .steps
                .iter()
                .filter(|s| s.status == crate::plan::StepStatus::Done)
                .count();
            let blocked = plan
                .steps
                .iter()
                .filter(|s| s.status == crate::plan::StepStatus::Blocked)
                .count();
            let mut stats = Vec::new();
            if done > 0 {
                stats.push(format!("{done} steps done"));
            }
            if blocked > 0 {
                stats.push(format!("{blocked} blocked"));
            }
            let stats_str = if stats.is_empty() {
                format!("{} steps", plan.steps.len())
            } else {
                stats.join(" · ")
            };

            let last_sess_obj = sessions
                .iter()
                .find(|s| {
                    plan.sessions.contains(&s.id.to_string())
                        || s.plan_id.as_deref() == Some(&plan.id)
                })
                .or_else(|| sessions.first());

            let last_session_info = last_sess_obj.map(|s| RecentSessionInfo {
                date: fmt_relative_time(s.last_activity()),
                title: s.title.clone(),
                outcome: stats_str,
            });

            let outside_sessions: Vec<RecentSessionInfo> = sessions
                .iter()
                .filter(|s| {
                    !plan.sessions.contains(&s.id.to_string())
                        && s.plan_id.as_deref() != Some(&plan.id)
                })
                .take(3)
                .map(|s| {
                    let outcome = if s.calls > 0 || s.errors > 0 {
                        let mut parts = Vec::new();
                        if s.calls > 0 {
                            parts.push(format!("{} done", s.calls));
                        }
                        if s.errors > 0 {
                            parts.push(format!("{} blocked", s.errors));
                        }
                        parts.join(" · ")
                    } else {
                        "complete".to_string()
                    };
                    RecentSessionInfo {
                        date: fmt_relative_time(s.last_activity()),
                        title: s.title.clone(),
                        outcome,
                    }
                })
                .collect();

            (Some(plan_info), last_session_info, outside_sessions)
        } else {
            let last_session_info = sessions.first().map(|s| RecentSessionInfo {
                date: fmt_relative_time(s.last_activity()),
                title: s.title.clone(),
                outcome: String::new(),
            });

            let recent_sessions: Vec<RecentSessionInfo> = sessions
                .iter()
                .take(3)
                .map(|s| {
                    let outcome = if s.calls > 0 || s.errors > 0 {
                        let mut parts = Vec::new();
                        if s.calls > 0 {
                            parts.push(format!("{} done", s.calls));
                        }
                        if s.errors > 0 {
                            parts.push(format!("{} blocked", s.errors));
                        }
                        parts.join(" · ")
                    } else {
                        "complete".to_string()
                    };
                    RecentSessionInfo {
                        date: fmt_relative_time(s.last_activity()),
                        title: s.title.clone(),
                        outcome,
                    }
                })
                .collect();

            (None, last_session_info, recent_sessions)
        };

        let mut warnings = Vec::new();
        if let Some(pc) = cfg.providers.get(&model_cfg.provider)
            && pc.effective_api_key(&model_cfg.provider).is_none()
        {
            let env_name = pc
                .key_env_name(&model_cfg.provider)
                .unwrap_or_else(|| "API_KEY".into());
            warnings.push(format!("no API key: set {env_name} or run /settings"));
        }
        if git_branch.is_none() {
            warnings.push("git not found: undo for shell commands disabled".to_string());
        }
        if read_only {
            warnings.push("another sqwai instance holds this project — read-only".to_string());
        }
        warnings.truncate(2);

        StartupData {
            version,
            project_path,
            git_branch,
            git_modified,
            model,
            active_plan,
            last_session,
            memory,
            recent,
            warnings,
            has_sqwai_dir,
        }
    }
}

pub(super) fn shorten_path(path: &std::path::Path) -> String {
    let path_str = path.to_string_lossy().replace('\\', "/");
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        let home_str = home.replace('\\', "/");
        if path_str.starts_with(&home_str) {
            let rest = &path_str[home_str.len()..];
            if rest.is_empty() {
                return "~".to_string();
            }
            if rest.starts_with('/') {
                return format!("~{rest}");
            }
            return format!("~/{rest}");
        }
    }
    path_str
}

/// Branch and dirty-file count for the status bar, read through the `git`
/// binary rather than libgit2 (§5.10).
///
/// This is the user's own repository, so it is read and never written: two
/// plumbing commands, no index, no locks. `--porcelain` output is stable
/// across git versions, which is the point of asking for it.
pub(super) fn collect_git_info(root: &std::path::Path) -> (Option<String>, Option<usize>) {
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    // `--abbrev-ref HEAD` prints `HEAD` on a detached head, which is what the
    // libgit2 version reported too
    let Some(branch) = git(&["rev-parse", "--abbrev-ref", "HEAD"]) else {
        // not a repository, or no git binary: the bar simply shows no branch
        return (None, None);
    };
    let branch = branch.trim().to_string();
    if branch.is_empty() {
        return (None, None);
    }
    let modified = git(&["status", "--porcelain", "--untracked-files=all"])
        .map(|out| out.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0);
    (Some(branch), Some(modified))
}

pub(super) fn fmt_relative_time(dt: chrono::DateTime<chrono::Utc>) -> String {
    let local = dt.with_timezone(&chrono::Local);
    let now = chrono::Local::now();
    let duration = now.signed_duration_since(local);
    if duration.num_minutes() < 1 {
        "just now".to_string()
    } else if duration.num_hours() < 24 && now.date_naive() == local.date_naive() {
        local.format("%H:%M").to_string()
    } else if (now.date_naive() - local.date_naive()).num_days() == 1 {
        format!("yesterday {}", local.format("%H:%M"))
    } else if duration.num_days() < 7 {
        let days = duration.num_days().max(2);
        format!("{days} days ago")
    } else {
        local.format("%d.%m %H:%M").to_string()
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

fn on_off(v: bool) -> String {
    if v { "on" } else { "off" }.to_string()
}
