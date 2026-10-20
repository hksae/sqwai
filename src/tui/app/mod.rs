use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Block;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tui_textarea::TextArea;

use crate::agent::loop_task::{AgentEvent, AgentHandle, ApprovalDecision, ControlMsg};
use crate::config::{Config, EffortLevel, ModelConfig, ModelStatus};
use crate::plan;
use crate::providers::{self, Role, SharedProvider};
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
    // Conservative rule: ANY reverted part of a done step's result
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

mod attachments;
mod commands;
mod events;
mod forms;
mod menu_actions;
mod menu_rows;
mod menus;
mod perf;
mod prompt;
mod segments;
mod session_ops;
#[cfg(test)]
mod tests;
mod turn_events;
mod undo;
mod view;
mod view_doc;
mod view_surfaces;

use forms::FormField;
use menus::{Menu, MenuAction};
use view::{ActivityGroup, AskRow, AskState, CellPos, SegMeta, Segment, Selection, StoredView};

use menus::COMMANDS;

/// The clipboard seam. Every copy and paste in the app goes through here
/// rather than through `arboard` directly, because the unit tests exercise
/// those paths (drag-select + Ctrl+C, unfolding an error copies its text)
/// and a real clipboard write is permanent, visible state: Windows parks
/// every value ever set in the Win+V history, so a test run rewrites the
/// developer's clipboard. Under `cfg(test)` both directions land in a
/// process-local string instead.
#[cfg(not(test))]
fn clip_set(text: &str) -> Result<(), arboard::Error> {
    arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text))
}

#[cfg(not(test))]
fn clip_get() -> Result<String, arboard::Error> {
    arboard::Clipboard::new().and_then(|mut cb| cb.get_text())
}

/// Clipboard bitmap, when the platform provides one. Text paste is tried only
/// when this returns `None`, so a screenshot copied from a browser (where the
/// clipboard often also carries a URL) still pastes as an image.
#[cfg(not(test))]
fn clip_get_image() -> Option<(usize, usize, Vec<u8>)> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let image = clipboard.get_image().ok()?;
    Some((image.width, image.height, image.bytes.into_owned()))
}

#[cfg(test)]
static CLIP_TEST: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

#[cfg(test)]
fn clip_set(text: &str) -> Result<(), arboard::Error> {
    *CLIP_TEST.lock().unwrap() = text.to_string();
    Ok(())
}

#[cfg(test)]
fn clip_get() -> Result<String, arboard::Error> {
    Ok(CLIP_TEST.lock().unwrap().clone())
}

#[cfg(test)]
fn clip_get_image() -> Option<(usize, usize, Vec<u8>)> {
    None
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
    /// Stable prefix as its cacheable layers (rules, environment, memory,
    /// skills), captured once per session and reused byte-for-byte so a
    /// provider-side prefix cache can hit on every request. Kept apart rather
    /// than joined so `/context` can bill memory and skills on their own.
    stable_parts: Vec<crate::providers::SystemPart>,
    /// This instance is read-only because another sqwai process owns the project lock.
    read_only: bool,
    /// Session-scoped environment facts, rebuilt only at startup and compaction.
    session_environment: String,
    /// Background builder for `session_environment` so TUI startup and `/new`
    /// never block on initial toolchain/manifest probes.
    session_env_rx: Option<std::sync::mpsc::Receiver<String>>,
    /// Whether the current prompt still needs the full tool-oriented context.
    context_bootstrap_pending: bool,
    discovered_skills: Vec<crate::prompts::skills::Skill>,
    active_skills: Vec<crate::prompts::skills::Skill>,
    skill_search: TextArea<'static>,
    skill_focus: bool,

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
    pending_queue: Vec<attachments::QueuedPrompt>,
    /// images drafted in the composer, each tied to an `[image N]` marker
    pending_images: Vec<crate::providers::ImageAttachment>,
    /// last marker number handed out in the current composition
    image_marker: u32,
    /// bumped whenever the composer is cleared; late worker results from an
    /// older composition are discarded instead of attaching to a new one
    image_epoch: u64,
    /// image decodes still in flight, for cap accounting and submit gating
    pending_image_jobs: Vec<attachments::PendingImageJob>,
    image_jobs_tx: std::sync::mpsc::Sender<attachments::ImageJobResult>,
    image_jobs_rx: std::sync::mpsc::Receiver<attachments::ImageJobResult>,
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
    /// label of the last shadow checkpoint
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
    /// The open menu is a bottom surface (effort, /context) rather than a
    /// popup. It is the tail of the history, so vertical scrolling belongs to
    /// the transcript: the wheel and PageUp/PageDown move the view, a press
    /// outside the surface selects history instead of dismissing it, and the
    /// surface scrolls away with the end of the history.
    surface_open: bool,
    /// Rows the scrolling surface (the /context card) contributes to the
    /// document, so the view can scroll through them. Zero for a pinned picker
    /// and for no surface at all.
    surf_rows: usize,
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
    /// which animation condition (or bare `dirty`) drove the frame; bit layout
    /// in [`perf`]. Computed every pass — it reuses the terms `animating`
    /// already evaluates, so an off log costs one u32 store.
    perf_why: u32,
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
    /// The list cursor, as a position on an endless walk over the commands: one
    /// step per arrow press, never a wrap-around jump, so the list rotates. The
    /// item under it is `walk.rem_euclid(len)`. `None` until something moves it,
    /// so plain Enter keeps submitting the draft as typed.
    popup_walk: Option<i64>,
    /// Absolute index of the first command inside the list window. The window
    /// follows the cursor by exactly one row per step, in both directions.
    popup_top: i64,
    /// Screen row → menu row index, for a list that paints as a surface. The
    /// block scrolls with the history, so hover and click can no longer be
    /// computed from `menu_rect.y + 2` the way a fixed popup could.
    surf_hits: Vec<(u16, usize)>,
    /// First body row inside a surface's window. Its own counter, not
    /// `menu_scroll`: the popup's follow-the-selection drags that over the
    /// pinned leading rows too, which would scroll a list the cursor is not on.
    surf_top: usize,
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
    /// card width the open /context menu was built for: a resize rebuilds it
    ctx_built_w: u16,
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
    /// unsaved draft values for a brand-new model (`Menu::EditModel { key: None }`)
    /// until both `key` and `request id` have been filled in
    model_draft: menus::ModelDraft,
    /// unsaved draft values for a brand-new provider (`Menu::EditProvider { name: None }`)
    /// until both `name` and `base url` have been filled in
    provider_draft: menus::ProviderDraft,
    /// cached session list for the sessions menus (headers only — see
    /// `Session::list_visible_headers`)
    sessions: Vec<SessionHeader>,
    /// The sessions surface's search field and caret focus.
    session_search: TextArea<'static>,
    session_focus: bool,
    /// The `/config` surface's search field and caret focus.
    config_search: TextArea<'static>,
    config_focus: bool,
    /// In-memory sample plan created by `/test plan` for previewing the Plan surface.
    test_plan: Option<crate::plan::Plan>,
    /// The providers surface's search field: a real input with its own caret,
    /// focused while that surface is open, filtering the list as it is typed.
    prov_search: TextArea<'static>,
    /// Whether that field holds the caret. It is a click away, not a default:
    /// a standing block cursor in a field nobody chose is the loudest thing on
    /// the surface.
    prov_focus: bool,
    /// The models surface's own field and caret. Each surface keeps a separate
    /// query: the provider name you filtered to is not a model filter, and
    /// opening the list under it would hide every model you came to look at.
    model_search: TextArea<'static>,
    model_focus: bool,
    ef_click: Option<(u16, u16)>,
    /// Click range of the context readout in the status bar: opens the same
    /// menu `/context` opens.
    ctx_click: Option<(u16, u16)>,
    /// Click range of the model name in the status bar: the same target Ctrl+G
    /// binds to.
    md_click: Option<(u16, u16)>,
    /// Connection-check results per provider, shown on the provider's menu
    /// rows: green on success, red with the reason on failure.
    provider_checks: std::collections::HashMap<String, ProviderCheck>,
    /// In-flight check and the channel its worker thread reports back on,
    /// polled on the UI tick so the check never blocks rendering.
    provider_check_rx: Option<(String, std::sync::mpsc::Receiver<Result<String, String>>)>,
    /// Background check/download of builtin providers: (manual, rx)
    builtin_update_rx: Option<BuiltinUpdateRx>,
    /// Last time a background catalog check was started, so a long-running TUI
    /// session re-checks every 3 hours automatically.
    last_builtin_check: Instant,
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

    pub fn new(cfg: Config, session: Session, startup: bool, read_only: bool) -> Result<Self> {
        let (image_jobs_tx, image_jobs_rx) = std::sync::mpsc::channel();
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
                vision: None,
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
            // before any plan read. No-op on a clean tree.
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
            stable_parts: Vec::new(),
            session_environment: String::new(),
            session_env_rx: None,
            context_bootstrap_pending: true,
            discovered_skills: Vec::new(),
            active_skills: Vec::new(),
            skill_search: Self::fresh_input(String::new()),
            skill_focus: false,
            cfg,
            segments: Vec::new(),
            streaming: false,
            aborted: false,
            agent: None,
            test_churn: None,
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
            pending_images: Vec::new(),
            image_marker: 0,
            image_epoch: 0,
            pending_image_jobs: Vec::new(),
            image_jobs_tx,
            image_jobs_rx,
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
            surface_open: false,
            surf_rows: 0,
            seg_cache: std::collections::HashMap::new(),
            live_md: crate::tui::markdown::LiveRender::new(),
            seg_meta: Vec::new(),
            next_seg_id: 1,
            theme_rev: 0,
            last_fp: 0,
            perf: perf::PerfLog::new(),
            perf_why: 0,
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
            popup_walk: None,
            popup_top: 0,
            surf_hits: Vec::new(),
            surf_top: 0,
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
            ctx_built_w: 0,
            effort_hits: Vec::new(),
            effort_blend: None,
            effort_blend_sel: None,
            form_fields: Vec::new(),
            form_focus: 0,
            form_draft: Vec::new(),
            model_draft: menus::ModelDraft::default(),
            provider_draft: menus::ProviderDraft::default(),
            sessions: Vec::new(),
            session_search: Self::fresh_input(String::new()),
            session_focus: false,
            config_search: Self::fresh_input(String::new()),
            config_focus: false,
            test_plan: None,
            prov_search: Self::fresh_input(String::new()),
            prov_focus: false,
            model_search: Self::fresh_input(String::new()),
            model_focus: false,
            ef_click: None,
            ctx_click: None,
            md_click: None,
            provider_checks: std::collections::HashMap::new(),
            provider_check_rx: None,
            builtin_update_rx: None,
            last_builtin_check: Instant::now(),
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
        app.refresh_skills();
        app.stable_parts = app.stable_parts();
        app.rebuild_session_environment();
        app.context_bootstrap_pending = true;
        crate::providers::set_conversation_id(&app.session.id.to_string());
        app.start_builtin_update(false);
        Ok(app)
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
        // The selection is the house band, not tui-textarea's ANSI cyan: a
        // selection must never mean anything other than "this row is picked".
        input.set_cursor_style(Theme::cursor());
        input.set_selection_style(Theme::selection());
        input
    }

    fn input_block() -> Block<'static> {
        Block::default()
    }

    /// Copy a textarea's selection and return the text, **without ending the
    /// selection**.
    ///
    /// `TextArea::copy()` reads through `take_selection_positions()`, which
    /// cancels the selection it just read — so every copy erased the highlight
    /// the user was looking at, on mouse release as much as on Ctrl+C. The yank
    /// buffer is still filled (Ctrl+Y must paste what was copied), and the
    /// selection is put back over the same range afterwards.
    pub(super) fn copy_textarea(ta: &mut tui_textarea::TextArea) -> String {
        let range = ta.selection_range();
        ta.copy();
        let text = ta.yank_text();
        if let Some((start, end)) = range {
            ta.move_cursor(tui_textarea::CursorMove::Jump(
                start.0 as u16,
                start.1 as u16,
            ));
            ta.start_selection();
            ta.move_cursor(tui_textarea::CursorMove::Jump(end.0 as u16, end.1 as u16));
        }
        text
    }

    /// Width of the `› ` input marker gutter (0 on degenerate widths).
    /// The marker occupies the gutter on the top input row only; the
    /// textarea itself is shifted right by this on every row.
    pub(super) fn input_marker_w(&self) -> u16 {
        if self.last_input.width >= 6 { 2 } else { 0 }
    }

    /// Compute the earliest wall-clock `Instant` the main loop needs to wake
    /// up for when no input or agent event arrives first. Returns `None` when
    /// the UI is completely idle (zero timers, zero animations, nothing dirty),
    /// allowing the event loop to sleep indefinitely with 0% CPU wakeups.
    fn next_wake_deadline(&self, last_build: Instant) -> Option<Instant> {
        let now = Instant::now();
        let mut earliest: Option<Instant> = None;
        let mut consider = |t: Instant| {
            earliest = Some(match earliest {
                Some(cur) => cur.min(t),
                None => t,
            });
        };

        // 1. Frame build pending: either waiting out a 150ms resize drag settle
        //    or held back by the 8.33ms presenter pacing gate.
        if self.dirty {
            if self.defer_rebuild
                && let Some(t_res) = self.last_resize
            {
                consider(t_res + Duration::from_millis(150));
            } else {
                consider(last_build + crate::tui::presenter::MIN_PRESENT_INTERVAL);
            }
        }

        // 2. Enter burst gate (30ms submit disambiguation).
        if let Some(dl) = self.enter_gate.deadline() {
            consider(dl);
        }

        // 3. Typewriter reveal queue (90ms flush window).
        if !self.pending_reveal.is_empty() {
            let dl = match self.last_reveal {
                Some(t0) => t0 + REVEAL_FLUSH_INTERVAL,
                None => now,
            };
            consider(dl);
        }

        // 4. Short 250ms color sweeps (mode chip / effort slider): step at
        //    ~60 FPS (16ms) for smooth gradient interpolation.
        let smooth_blend = self
            .mode_blend
            .is_some_and(|(_, t0)| t0.elapsed().as_millis() < MODE_BLEND_MS as u128)
            || self
                .effort_blend
                .is_some_and(|(_, t0)| t0.elapsed().as_millis() < MODE_BLEND_MS as u128);
        if smooth_blend {
            consider(now + Duration::from_millis(16));
        }

        // 5. 20 FPS animations (streaming, running tools, finish wave, gallery).
        let (animating, _) = self.animating_terms();
        if animating {
            let elapsed_ms = self.tick_origin.elapsed().as_millis() as u64;
            let next_tick_ms = ((elapsed_ms / 50) + 1) * 50;
            consider(self.tick_origin + Duration::from_millis(next_tick_ms));
        }

        // 6. Toast expiration deadline (exact 3s wakeup, no polling in between).
        if let Some(toast) = &self.toast {
            consider(toast.until);
        }

        // 7. Background std::sync::mpsc tasks or queued paste batch remainder.
        if !self.pending_events.is_empty() {
            consider(now + Duration::from_millis(8));
        } else if self.provider_check_rx.is_some()
            || self.builtin_update_rx.is_some()
            || self.maintain_rx.is_some()
            || self.session_env_rx.is_some()
        {
            consider(now + Duration::from_millis(50));
        }

        earliest
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
            // Tickless, event-driven wakeup:
            // - Crossterm input wakes instantly via `input_notify`.
            // - Agent events (`TextDelta`, `ToolStart`, `ToolNotice`, etc.)
            //   wake instantly via cancel-safe `agent.rx.recv()`.
            // - Deadlines (8.33ms frame gate, 16ms color blend, 30ms enter
            //   gate, 50ms spinner tick, 90ms typewriter flush, 3s toast TTL)
            //   sleep until the exact target instant; when completely idle,
            //   no timer runs at all (0% CPU wakeups).
            let deadline = self.next_wake_deadline(last_build);
            let has_agent = self.agent.is_some();
            let mut woke_agent_ev: Option<AgentEvent> = None;
            tokio::select! {
                _ = input_notify.notified() => {},
                maybe_ev = async {
                    match self.agent.as_mut() {
                        Some(handle) => handle.rx.recv().await,
                        None => std::future::pending().await,
                    }
                }, if has_agent => {
                    woke_agent_ev = maybe_ev;
                },
                _ = async {
                    match deadline {
                        Some(dl) => tokio::time::sleep_until(tokio::time::Instant::from_std(dl)).await,
                        None => std::future::pending().await,
                    }
                } => {},
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
            if let Some(ev) = woke_agent_ev {
                if !self.handle_agent_event(ev) {
                    self.poll_agent();
                }
            } else {
                self.poll_agent();
            }
            self.poll_provider_check();
            self.poll_builtin_update();
            self.poll_maintain();
            self.poll_session_env();
            self.poll_image_jobs();
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
            let (animating, why) = self.animating_terms();
            self.perf_why = why;
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
                    cached: self.seg_cache.len(),
                    why: self.perf_why,
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
        // entry that used to be appended here is gone with the diary.
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

    /// the live `ask_user` row awaiting an answer, if it still exists.
    /// Resolved by the loop's ask id: rows inserted later shift indices.
    pub(super) fn active_ask_seg(&self) -> Option<usize> {
        let id = self.active_ask_id?;
        self.segments.iter().position(|s| {
            matches!(
                s,
                Segment::Tool {
                    ask: Some(a),
                    ..
                } if a.id == id
            )
        })
    }

    /// hang the questionnaire on the tool row its call already opened. The row
    /// is in execution order because `ToolStart` precedes this event, so the
    /// ask inherits the place every other tool call has — no second surface is
    /// inserted beside it, and the answered record is that same row.
    pub(super) fn open_ask(
        &mut self,
        call_id: &str,
        id: u64,
        questions: Vec<crate::agent::loop_task::AskQuestion>,
    ) {
        // a previous unanswered ask (e.g. after abort) is closed first so at
        // most one row stays interactive
        self.freeze_active_ask("(no answer — superseded)");
        let ask = Box::new(AskState {
            id,
            picked: questions
                .iter()
                .map(|q| vec![false; q.options.len()])
                .collect(),
            custom: vec![String::new(); questions.len()],
            focus: 0,
            cursor: vec![0; questions.len()],
            questions,
        });
        let found = self.segments.iter().position(|s| {
            matches!(s, Segment::Tool { call_id: cid, ask: None, .. }
                if cid.as_deref() == Some(call_id))
        });
        let seg = match found {
            Some(i) => i,
            None => {
                // `ToolStart` is ordered ahead of this event on the same
                // channel, so this is unreachable in a live run. Answering is
                // impossible without a row, and the loop blocks on the answer
                // forever, so the row is opened here rather than dropped.
                let tool = Segment::Tool {
                    name: "ask_user".into(),
                    args: String::new(),
                    call_id: Some(call_id.to_string()),
                    ok: None,
                    output: String::new(),
                    diff: None,
                    preview: Vec::new(),
                    preview_total: 0,
                    expanded: false,
                    flash: None,
                    ask: None,
                };
                let pos = self
                    .segments
                    .iter()
                    .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
                    .unwrap_or(self.segments.len());
                self.insert_segment(pos, tool);
                pos
            }
        };
        if let Some(Segment::Tool {
            ask: slot,
            expanded,
            ..
        }) = self.segments.get_mut(seg)
        {
            *slot = Some(ask);
            // live questions open unfolded so they can be answered at once
            *expanded = true;
        }
        self.touch_segment(seg);
        self.active_ask_id = Some(id);
        self.ask_hover = None;
        self.ask_custom_focus = None;
        self.follow = true;
        self.dirty = true;
    }

    /// build the answer string for the live ask, same shape as
    /// the legacy menu confirm (`"label1, label2; custom"` per question,
    /// `"H: answer"` joined with `" | "`, `"(no answer)"` when empty)
    pub(super) fn inline_ask_text(&self, seg_idx: usize) -> String {
        let Some(Segment::Tool { ask: Some(ask), .. }) = self.segments.get(seg_idx) else {
            return String::new();
        };
        let questions = &ask.questions;
        let picked = &ask.picked;
        let custom = &ask.custom;
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
        if let Some(Segment::Tool { ask: Some(ask), .. }) = self.segments.get_mut(seg) {
            if !ask.can_reach(q) {
                return;
            }
            let Some(opts) = ask.picked.get_mut(q) else {
                return;
            };
            if opt >= opts.len() {
                return;
            }
            for (i, v) in opts.iter_mut().enumerate() {
                *v = i == opt;
            }
            if let Some(c) = ask.cursor.get_mut(q) {
                *c = opt;
            }
            ask.focus = q;
            // one radio: choosing an option retires the free text, so a
            // single-choice question never shows two filled markers
            if !ask.questions.get(q).is_some_and(|qq| qq.multiple)
                && let Some(free) = ask.custom.get_mut(q)
            {
                free.clear();
            }
        }
        self.touch_segment(seg);
        self.ask_custom_focus = None;
        self.dirty = true;
    }

    /// multi-choice: toggle one option in question q
    pub(super) fn inline_ask_toggle(&mut self, q: usize, opt: usize) {
        let Some(seg) = self.active_ask_seg() else {
            return;
        };
        if let Some(Segment::Tool { ask: Some(ask), .. }) = self.segments.get_mut(seg) {
            if !ask.can_reach(q) {
                return;
            }
            let Some(v) = ask.picked.get_mut(q).and_then(|v| v.get_mut(opt)) else {
                return;
            };
            *v = !*v;
            if let Some(c) = ask.cursor.get_mut(q) {
                *c = opt;
            }
            ask.focus = q;
        }
        self.touch_segment(seg);
        self.ask_custom_focus = None;
        self.dirty = true;
    }

    /// switch the focused question (Tab / Shift+Tab / click)
    pub(super) fn inline_ask_focus(&mut self, q: usize) {
        let Some(seg) = self.active_ask_seg() else {
            return;
        };
        if let Some(Segment::Tool { ask: Some(ask), .. }) = self.segments.get_mut(seg) {
            if !ask.can_reach(q) {
                return;
            }
            ask.focus = q;
        }
        self.touch_segment(seg);
        self.ask_custom_focus = None;
        self.dirty = true;
    }

    /// focus the free-text editor of question q. On a single-choice question
    /// this is the user choosing "something else", so the option markers clear —
    /// the same radio `inline_ask_select` enforces from the other side.
    pub(super) fn inline_ask_open_custom(&mut self, q: usize) {
        let Some(seg) = self.active_ask_seg() else {
            return;
        };
        if let Some(Segment::Tool { ask: Some(ask), .. }) = self.segments.get(seg)
            && !ask.can_reach(q)
        {
            return;
        }
        self.inline_ask_focus(q);
        if let Some(seg) = self.active_ask_seg()
            && let Some(Segment::Tool { ask: Some(ask), .. }) = self.segments.get_mut(seg)
        {
            if let Some(qq) = ask.questions.get(q) {
                let n = qq.options.len();
                let multiple = qq.multiple;
                if let Some(c) = ask.cursor.get_mut(q) {
                    *c = n;
                }
                if !multiple
                    && let Some(opts) = ask.picked.get_mut(q)
                {
                    for v in opts.iter_mut() {
                        *v = false;
                    }
                }
            }
            self.touch_segment(seg);
        }
        self.ask_custom_focus = Some(q);
        self.dirty = true;
    }

    /// Move the option cursor (`Up` / `Down`) strictly inside the currently
    /// focused question (`ask.focus`).
    pub(super) fn inline_ask_move(&mut self, delta: isize) {
        let Some(seg) = self.active_ask_seg() else {
            return;
        };
        let (q, next_cursor, n_opts, multiple, allow_free) = match self.segments.get(seg) {
            Some(Segment::Tool { ask: Some(ask), .. }) => {
                let q = ask.focus;
                let Some(qq) = ask.questions.get(q) else {
                    return;
                };
                let n_opts = qq.options.len();
                let total = n_opts + usize::from(qq.allow_free);
                if total == 0 {
                    return;
                }
                let cur = ask.cursor.get(q).copied().unwrap_or(0).min(total - 1);
                let next = (cur as isize + delta).rem_euclid(total as isize) as usize;
                (q, next, n_opts, qq.multiple, qq.allow_free)
            }
            _ => return,
        };
        self.ask_hover = None;
        if next_cursor < n_opts {
            if multiple {
                if let Some(Segment::Tool { ask: Some(ask), .. }) = self.segments.get_mut(seg)
                    && let Some(c) = ask.cursor.get_mut(q)
                {
                    *c = next_cursor;
                }
                self.ask_custom_focus = None;
                self.touch_segment(seg);
                self.dirty = true;
            } else {
                self.inline_ask_select(q, next_cursor);
            }
        } else if allow_free {
            self.inline_ask_open_custom(q);
        }
    }

    /// confirm the current question: if it is answered and more questions
    /// follow, advance focus to the next one; on the last question, submit the
    /// whole questionnaire to the agent. Unanswered questions cannot confirm.
    pub(super) fn inline_ask_confirm(&mut self) {
        let Some(seg) = self.active_ask_seg() else {
            return;
        };
        if let Some(Segment::Tool { ask: Some(ask), .. }) = self.segments.get_mut(seg) {
            let cur = ask.focus;
            if !ask.is_answered(cur) {
                return;
            }
            if cur + 1 < ask.questions.len() {
                ask.focus = cur + 1;
                self.ask_custom_focus = None;
                self.touch_segment(seg);
                self.dirty = true;
                return;
            }
        }
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

    /// freeze the active ask and deliver the text to the agent
    fn inline_ask_answer(&mut self, text: String) {
        let Some(seg) = self.active_ask_seg() else {
            return;
        };
        let (id, call_id) = match self.segments.get(seg) {
            Some(Segment::Tool {
                ask: Some(ask),
                call_id,
                ..
            }) => (ask.id, call_id.clone()),
            _ => return,
        };
        self.close_ask_segment(seg, Some(text.clone()));
        if call_id.as_deref().is_some_and(|c| c.starts_with("test-ask-")) {
            let ok = text != "(no answer)";
            self.handle_tool_notice("ask_user".into(), text, ok, None, call_id);
        } else if let Some(agent) = &self.agent {
            let _ = agent.control.try_send(ControlMsg::AskAnswer { id, text });
        }
        self.active_ask_id = None;
        self.ask_hover = None;
        self.ask_custom_focus = None;
        self.follow = true;
        self.dirty = true;
    }

    /// the agent turn ended (or was aborted) while a question was open:
    /// never leave a ghost interactive row behind. The note becomes the row's
    /// output; `retire_unfinished_rows` settles the marker if no notice follows.
    fn freeze_active_ask(&mut self, note: &str) {
        let Some(seg) = self.active_ask_seg() else {
            self.active_ask_id = None;
            return;
        };
        self.close_ask_segment(seg, Some(note.to_string()));
        self.active_ask_id = None;
        self.ask_hover = None;
        self.ask_custom_focus = None;
        self.dirty = true;
    }

    /// take the questionnaire away and fold the row back to one line: this is
    /// the moment `ask_user` stops being anything other than a tool row.
    /// `output` is written only when the call will not report one itself.
    fn close_ask_segment(&mut self, seg: usize, output: Option<String>) {
        if let Some(Segment::Tool {
            ask,
            expanded,
            output: out,
            preview,
            preview_total,
            ..
        }) = self.segments.get_mut(seg)
        {
            *ask = None;
            *expanded = false;
            if let Some(text) = output {
                *out = text;
                let (p, t) = view::tool_preview(None, out.as_str());
                *preview = p;
                *preview_total = t;
            }
        }
        self.touch_segment(seg);
    }

    pub(super) fn refresh_skills(&mut self) {
        self.discovered_skills =
            crate::prompts::skills::discover(&self.cfg.skills, &self.project_root);
    }

    /// The discovered skill catalog, borrowed. Skills carry their full
    /// `SKILL.md` body, so this must never clone: the popup and the menu rows
    /// are rebuilt on every keystroke and every frame.
    pub(super) fn cached_skills(&self) -> &[crate::prompts::skills::Skill] {
        &self.discovered_skills
    }

    /// Second-level completion target: `<cmd> <tail>` filters that
    /// command's subcommand list from [`menus::SUBCOMMANDS`].
    /// Gated commands (e.g. `/test` without the experimental flag)
    /// complete nothing.
    fn popup_level2_cmd(&self) -> Option<&'static str> {
        let t = self.input_text();
        if t.starts_with("/skill ") {
            let has_skills = !self.active_skills.is_empty() || !self.cached_skills().is_empty();
            if has_skills {
                return Some("/skill");
            }
        }
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
        if let Some(cmd) = self.popup_level2_cmd() {
            let tail = t.strip_prefix(cmd).unwrap_or("").trim_start_matches(' ');
            let word = tail.split_whitespace().next().unwrap_or("");
            if cmd == "/skill" {
                let word_lower = word.to_ascii_lowercase();
                let mut subs: Vec<String> = self
                    .cached_skills()
                    .iter()
                    .map(|s| s.name.clone())
                    .collect();
                subs.push("clear".to_string());
                return subs
                    .into_iter()
                    .filter(|sub| sub.to_ascii_lowercase().starts_with(&word_lower))
                    .map(|sub| format!("{cmd} {sub}"))
                    .collect();
            }
            if let Some(subs) = menus::subcommands_of(cmd) {
                return subs
                    .iter()
                    .filter(|sub| sub.starts_with(word))
                    .map(|sub| format!("{cmd} {sub}"))
                    .collect();
            }
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

    /// Step the command-list cursor. The walk is unbounded and the window
    /// follows it one row at a time, so running off the last command continues
    /// into the first from below — and the other way round — with no snap.
    pub(super) fn popup_move(&mut self, dir: i32) {
        let items = self.popup_items();
        let n = items.len() as i64;
        if n == 0 {
            return;
        }
        let step = if dir < 0 { -1 } else { 1 };
        // The first press lands where the arrow points: Down on the first
        // command, Up on the last one.
        let next = match self.popup_walk {
            Some(w) => w + step,
            None => {
                if dir < 0 {
                    n - 1
                } else {
                    0
                }
            }
        };
        self.popup_walk = Some(next);
        self.hover = Some(items[next.rem_euclid(n) as usize].clone());
        self.dirty = true;
    }

    /// The item under the cursor: Enter accepts it instead of submitting the
    /// draft, the same action as clicking it.
    pub(super) fn popup_accepted(&self) -> Option<String> {
        let walk = self.popup_walk?;
        let items = self.popup_items();
        let n = items.len() as i64;
        self.popup_visible()
            .then(|| items.get(walk.rem_euclid(n).max(0) as usize).cloned())
            .flatten()
    }

    /// Rebuild the provider client from the current config so changes made
    /// while sqwai is running — above all the API key — take effect on the next
    /// turn without restarting the app. The agent clones this handle per turn,
    /// so a key edited mid-turn is picked up when the next turn starts (after a
    /// normal Esc stop, or simply the next message).
    /// The host rewrote the transcript. A continuation reference points at the
    /// provider's copy of the history that was *not* rewritten, so continuing
    /// from it would hand the model the very context compaction removed
    /// while the host accounts for the compacted one.
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
    /// is what and assume: the host owns the context.
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

    fn page(&mut self, dir: i32) {
        self.scroll(-dir * 20);
    }

    fn scroll(&mut self, delta: i32) {
        let had_sel = self.sel.is_some();
        self.sel = None;
        let h = self.last_chat.height.max(1) as usize;
        let max = (self.cache_lines.len() + self.surf_rows).saturating_sub(h);
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

/// Clip `s` to `n` terminal COLUMNS. Counting chars was the old measure, and a
/// CJK or emoji title is one to two columns per char, so `truncate_chars(t, 30)`
/// handed back a 60-column row — the invariant every caller here exists to keep.
fn truncate_chars(s: &str, n: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if unicode_width::UnicodeWidthStr::width(s) <= n {
        return s.to_string();
    }
    if n == 0 {
        return String::new();
    }
    // reserve one slot for the ellipsis so the result is never wider than
    // `n` — otherwise a "…" pushed the boxed tool output one column past its
    // border and wrap_tagged split the overflow char (and the right │) onto
    // a new row, breaking the frame on every long line of a large file.
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > n - 1 {
            break;
        }
        used += w;
        out.push(ch);
    }
    out.push('…');
    out
}

fn short_id(s: &Session) -> String {
    s.id.to_string()[..8].to_string()
}

/// time of day for grouped session rows (the section header carries
/// the date, so the column keeps HH:MM only)
fn fmt_time(t: chrono::DateTime<chrono::Utc>) -> String {
    t.with_timezone(&chrono::Local).format("%H:%M").to_string()
}

fn on_off(v: bool) -> String {
    if v { "on" } else { "off" }.to_string()
}
