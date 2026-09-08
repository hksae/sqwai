use anyhow::Result;
use ratatui::backend::CrosstermBackend;
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

fn reopened_step_ids(
    active: &plan::Plan,
    records: &[crate::agent::journal::Record],
    session_id: &str,
    files: &[String],
) -> Vec<String> {
    let file_set: std::collections::HashSet<&str> = files.iter().map(String::as_str).collect();
    active
        .steps
        .iter()
        .filter(|step| step.status == plan::StepStatus::Done)
        .filter(|step| {
            let evidence_paths: Vec<&str> = records
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
                .collect();
            !evidence_paths.is_empty() && evidence_paths.iter().all(|path| file_set.contains(path))
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
    let Some(mut active) = plan::open_active(root).ok().flatten() else {
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
        let _ = plan::store(root, &active);
    }
    if let Ok(mut journal) = crate::agent::journal::Journal::open(root, session_id) {
        journal.set_attribution(None, Some(active.id.clone()), "host");
        let _ = journal.append_undo(checkpoint, files, &reopened);
    }
    reopened
}

pub type Terminal = ratatui::Terminal<CrosstermBackend<std::io::Stdout>>;

mod events;
mod forms;
mod menus;
#[cfg(test)]
mod tests;
mod view;

use forms::FormField;
use menus::{Menu, MenuAction};
use view::{ActivityGroup, AskRow, CellPos, ProposalRow, Segment, Selection};

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
    /// true while showing the no-session startup screen
    startup: bool,
    pub(super) startup_data: Option<StartupData>,
    pub(super) last_ctrl_c: Option<Instant>,
    /// last request error, shown in the status bar until the next action
    bar_error: Option<String>,
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
    /// last time draw_menu rebuilt menu_rows (throttled background refresh)
    menu_built_at: Option<std::time::Instant>,
    quit: bool,

    dirty: bool,
    cache_w: u16,
    cache_lines: Vec<Line<'static>>,
    cache_rowseg: Vec<Option<usize>>,
    last_chat: Rect,
    last_input: Rect,
    /// per-segment render cache: (content key at render time, width used,
    /// content lines). Segments nested in an activity group render narrower,
    /// so the width has to be part of the check.
    #[allow(clippy::type_complexity)]
    // tuple structure matches render pipeline; aliasing adds indirection
    seg_cache: Vec<Option<(usize, u16, Vec<(Line<'static>, Option<usize>)>)>>,
    /// stable order/identity of segments used to invalidate positional caches
    seg_layout: Vec<u64>,

    /// Clipboard text inserted by Ctrl+V, used to consume the terminal's
    /// replay of the same payload before it reaches normal key handling.
    pasted_clipboard: Option<String>,
    /// Suppresses the synthetic Enter some terminals emit after Ctrl+V.
    paste_enter_guard: bool,
    /// Classifies ordinary Windows Enter events as submit vs pasted newlines.
    enter_gate: events::EnterGate,
    /// Pending events queued during burst detection.
    pending_events: std::collections::VecDeque<crossterm::event::Event>,

    /// Deadline for the single transient busy notice.
    busy_until: Option<Instant>,

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
    hover: Option<usize>,
    popup_dismiss: bool,
    popup_scroll: usize,
    popup_rows: Vec<(u16, usize)>,

    // providers/models menu (Ctrl+P)
    menu_stack: Vec<Menu>,
    menu_sel: usize,
    /// scroll offset for long list menus
    menu_scroll: usize,
    /// fixed hint line under a list menu (not part of the scrolled rows)
    menu_footer_text: Option<String>,
    /// transient status shown inside the open menu instead of the chat
    menu_status: Option<(String, StatusKind)>,
    menu_rows: Vec<(Line<'static>, MenuAction)>,
    menu_rect: Rect,
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
        if let Some(plan) = crate::prompts::plan_block(&root) {
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
        self.plan_step_label = plan::open_active(&self.project_root)
            .ok()
            .flatten()
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
            });
        let resolved = cfg.resolve_provider(&model_cfg)?;
        let provider = providers::create(&resolved)?;

        let startup_data = if startup {
            Some(Self::collect_startup_data(&cfg, &model_cfg, read_only))
        } else {
            None
        };

        let project_root = std::env::current_dir().unwrap_or_default();
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
            last_ctrl_c: None,
            read_only,
            bar_error: None,
            prev_turn_ok: false,
            retry_notified: true, // no toast for the very first turn
            retry_line: None,
            last_checkpoint: None,
            turn_user_index: None,
            lsp_diagnostics: 0,
            follow: true,
            view_top: 0,
            spinner_tick: 0,
            menu_built_at: None,
            quit: false,
            dirty: true,
            cache_w: 0,
            cache_lines: Vec::new(),
            cache_rowseg: Vec::new(),
            last_chat: Rect::default(),
            last_input: Rect::default(),
            seg_cache: Vec::new(),
            seg_layout: Vec::new(),
            hover: None,
            popup_dismiss: false,
            popup_scroll: 0,
            popup_rows: Vec::new(),
            menu_stack: Vec::new(),
            menu_sel: 0,
            menu_scroll: 0,
            menu_footer_text: None,
            menu_status: None,
            menu_rows: Vec::new(),
            menu_rect: Rect::default(),
            form_fields: Vec::new(),
            form_focus: 0,
            sessions: Vec::new(),
            sessions_filter: String::new(),
            ef_click: None,
            provider_checks: std::collections::HashMap::new(),
            provider_check_rx: None,
            maintain_rx: None,
            effort_observed_ignored: None,
            agents_click: None,
            status_y: 0,
            press: None,
            dragging: false,
            sel: None,
            input_dragging: false,
            pasted_clipboard: None,
            paste_enter_guard: false,
            enter_gate: events::EnterGate::default(),
            pending_events: std::collections::VecDeque::new(),
            busy_until: None,
            activity_groups: Vec::new(),
            turn_started: None,
            live_group_collapsed: false,
        };
        if !startup {
            app.load_history_segments();
        }
        if app.session.plan_id.is_none() {
            app.session.plan_id = crate::plan::open_active(&app.project_root)
                .ok()
                .flatten()
                .map(|plan| plan.id);
        }
        app.refresh_plan_label();
        app.stable_prefix = app.stable_prefix();
        app.rebuild_session_environment();
        app.context_bootstrap_pending = true;
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
                    self.segments.push(Segment::User(m.content.clone()));
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
                    self.segments.push(Segment::Assistant {
                        text: m.content.clone(),
                        live: false,
                    });
                }
                Role::Assistant => {
                    work_start.get_or_insert(self.segments.len());
                    for call in &m.tool_calls {
                        let idx = self.segments.len();
                        self.segments.push(Segment::Tool {
                            name: call.name.clone(),
                            args: crate::agent::tools::call_summary(&call.name, &call.args),
                            ok: None,
                            output: String::new(),
                            diff: None,
                            expanded: false,
                        });
                        pending_tools.insert(call.id.clone(), idx);
                    }
                }
                Role::Tool => {
                    if let Some(call_id) = m.tool_call_id.as_ref()
                        && let Some(idx) = pending_tools.remove(call_id)
                        && let Some(Segment::Tool { ok, output, .. }) = self.segments.get_mut(idx)
                    {
                        *ok = Some(!m.is_error);
                        *output = m.content.clone();
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
            calls: saved.calls,
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
            self.segments.push(Segment::Status {
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
        input.set_cursor_line_style(Style::new().bg(Theme::SURFACE()));
        input.set_cursor_style(Style::new().bg(Theme::ACCENT_SOFT()).fg(Theme::BG()));
        input.set_selection_style(Style::new().bg(Theme::ACCENT()).fg(Theme::BG()));
        input
    }

    fn input_block() -> Block<'static> {
        Block::default()
    }

    pub async fn run(mut self, mut terminal: Terminal) -> Result<()> {
        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<crossterm::event::Event>();
        std::thread::spawn(move || {
            while let Ok(ev) = crossterm::event::read() {
                crate::tui::event_log::log("READ", crate::tui::event_log::describe(&ev));
                if ev_tx.send(ev).is_err() {
                    break;
                }
            }
        });

        let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
        while !self.quit {
            tick.tick().await;
            if self.enter_gate.flush(Instant::now()) {
                self.submit();
            }
            self.poll_input(&ev_rx)?;
            self.poll_agent();
            self.poll_provider_check();
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
            if self
                .busy_until
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.clear_busy_statuses();
                self.busy_until = None;
            }
            self.spinner_tick = self.spinner_tick.wrapping_add(1);
            self.dirty |= self.streaming;
            terminal.draw(|f| self.draw(f))?;
            self.dirty = false;
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
        self.segments.insert(pos, seg);
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
        self.segments.insert(pos, seg);
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
        self.active_proposal_id = None;
        self.proposal_hover = None;
        self.dirty = true;
    }

    fn popup_visible(&self) -> bool {
        let t = self.input_text();
        !self.popup_dismiss && t.starts_with('/') && !t.contains(' ')
    }

    fn popup_items(&self) -> Vec<usize> {
        let t = self.input_text();
        COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, cmd)| cmd.starts_with(&t))
            .map(|(i, _)| i)
            .collect()
    }

    pub(super) fn popup_scroll_by(&mut self, delta: i32) {
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
        self.popup_scroll = next.min(max_scroll);
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
        self.bar_error = None;
        self.retry_notified = false;
        self.retry_line = None;
        self.last_checkpoint = None;
        let mut text = self.input_text().trim().to_string();
        if text.is_empty() {
            if self.startup {
                let root = std::env::current_dir().unwrap_or_default();
                if let Ok(Some(active)) = crate::plan::open_active(&root) {
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
                    | "providers" | "models" | "sessions" | "exit" => true,
                    "plan" => {
                        let sub = parts.next().unwrap_or("show");
                        matches!(sub, "show")
                    }
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
            let cmd_name = rest.split_whitespace().next().unwrap_or("");
            if cmd_name != "new" {
                self.startup = false;
            }
            self.command(rest);
            return;
        }
        self.startup = false;
        // pick up any provider/key change made since the last turn
        self.rebuild_provider();
        self.segments.push(Segment::User(text.clone()));
        self.session.push(Role::User, &text);
        self.turn_user_index = Some(self.session.messages.len().saturating_sub(1));

        // The system block is assembled per request and travels separately
        // from the transcript: nothing here is ever written to the session.
        let system = self.system_block();
        let msgs: Vec<PMessage> = self.session.messages.clone();
        let root = std::env::current_dir().unwrap_or_default();
        let input = crate::agent::loop_task::AgentInput {
            provider: self.provider.clone(),
            model_id: self.model_cfg.id.clone(),
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
        };
        self.context_bootstrap_pending = false;
        self.agent = Some(spawn_agent(input));
        self.streaming = true;
        self.aborted = false;
        self.assistant_buf.clear();
        // the activity header shows how long the turn took; measure from here
        self.turn_started = Some(Instant::now());
        self.live_group_collapsed = false;
        // show the thinking placeholder right away so the indicator is visible
        // from turn start even before any reasoning deltas arrive
        let tpos = self.segments.len();
        self.segments.push(Segment::Thinking {
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
        self.segments.push(Segment::Assistant {
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
        };
        self.agent = Some(spawn_agent(input));
        self.streaming = true;
        self.aborted = false;
        self.turn_user_index = None;
        self.assistant_buf.clear();
        self.status("compacting context…", StatusKind::Info);
    }

    fn apply_session(&mut self, mut s: Session) {
        self.startup = false;
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
        if self.session.plan_id.is_none() {
            self.session.plan_id =
                crate::plan::open_active(&std::env::current_dir().unwrap_or_default())
                    .ok()
                    .flatten()
                    .map(|plan| plan.id);
        }
        // defensive: never let a legacy system turn back into the transcript
        self.session.strip_system_messages();
        self.segments.clear();
        self.seg_cache.clear();
        // the transcript is replaced: old group ranges point nowhere
        self.activity_groups.clear();
        self.active_subagent = None;
        self.subagents.clear();
        self.subagent_chats.clear();
        self.todos.clear();
        self.turn_user_index = None;
        self.active_ask_id = None;
        self.ask_hover = None;
        self.active_proposal_id = None;
        self.proposal_hover = None;
        self.ask_custom_focus = None;
        self.rebuild_session_environment();
        self.load_history_segments();
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
        self.startup = false;
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
        self.session.plan_id =
            crate::plan::open_active(&std::env::current_dir().unwrap_or_default())
                .ok()
                .flatten()
                .map(|plan| plan.id);
        self.context_bootstrap_pending = true;
        self.segments.clear();
        self.seg_cache.clear();
        self.activity_groups.clear();
        self.active_subagent = None;
        self.subagents.clear();
        self.subagent_chats.clear();
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

    fn command(&mut self, rest: &str) {
        let name = format!("/{rest}")
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        match name.as_str() {
            "/settings" => self.open_menu(Menu::Settings),
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

            "/theme" => self.open_menu(Menu::Themes),
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
                            self.bar_error = Some(format!("init: {e}"));
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
            "/fork" => {
                if self.session.messages.is_empty() {
                    self.status("nothing to fork yet", StatusKind::Warn);
                } else if self.streaming {
                    self.show_busy_status();
                } else {
                    self.open_menu(Menu::ForkPoint);
                }
            }
            "/providers" => self.open_menu(Menu::Providers),
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

    fn plan_command(&mut self, rest: &str) {
        let root = std::env::current_dir().unwrap_or_default();
        let args: Vec<&str> = rest.split_whitespace().skip(1).collect();
        let result = match args.first().copied() {
            None | Some("show") => {
                self.open_menu(Menu::Plan);
                return;
            }
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
            Some("complete") => match plan::open_active(&root) {
                Ok(Some(mut active)) => {
                    match plan::apply(&mut active, plan::Op::Complete, &plan::Limits::default()) {
                        Ok(plan::Applied::Completed) => match plan::store(&root, &active) {
                            Ok(()) => "plan completed".to_string(),
                            Err(e) => format!("plan write failed: {e:#}"),
                        },
                        Ok(_) => "plan complete did not change its status".to_string(),
                        Err(e) => format!("plan complete rejected [{}]: {}", e.code, e.reason),
                    }
                }
                Ok(None) => "no active plan".to_string(),
                Err(e) => format!("plan load failed: {e:#}"),
            },
            Some("abandon") => match plan::open_active(&root) {
                Ok(Some(mut active)) => {
                    active.status = plan::PlanStatus::Abandoned;
                    active.revision += 1;
                    match plan::store(&root, &active) {
                        Ok(()) => "plan abandoned".to_string(),
                        Err(e) => format!("plan write failed: {e:#}"),
                    }
                }
                Ok(None) => "no active plan".to_string(),
                Err(e) => format!("plan load failed: {e:#}"),
            },
            Some("waive") => {
                let index = args.get(1).and_then(|s| s.parse::<usize>().ok());
                let reason = args.get(2..).map(|v| v.join(" ")).unwrap_or_default();
                match (index, reason.trim()) {
                    (Some(index), reason) if !reason.is_empty() => match plan::open_active(&root) {
                        Ok(Some(mut active)) => match plan::waive(&mut active, index, reason) {
                            Ok(()) => match plan::store(&root, &active) {
                                Ok(()) => format!("acceptance {index} waived"),
                                Err(e) => format!("plan write failed: {e:#}"),
                            },
                            Err(e) => format!("plan waive rejected [{}]: {}", e.code, e.reason),
                        },
                        Ok(None) => "no active plan".to_string(),
                        Err(e) => format!("plan load failed: {e:#}"),
                    },
                    _ => "usage: /plan waive <acceptance-index> <reason>".to_string(),
                }
            }
            Some(other) => format!("unknown /plan action '{other}'"),
        };
        self.status(&result, StatusKind::Info);
    }

    fn goal_command(&mut self, rest: &str) {
        let root = std::env::current_dir().unwrap_or_default();
        let text = rest
            .split_once(' ')
            .map(|(_, value)| value.trim())
            .unwrap_or_default();
        if text.is_empty() {
            self.status("usage: /goal <text>", StatusKind::Warn);
            return;
        }
        match plan::open_active(&root) {
            Ok(Some(mut active)) => {
                plan::set_goal(
                    &mut active,
                    text.to_string(),
                    "user",
                    Some("user: /goal".to_string()),
                );
                match plan::store(&root, &active) {
                    Ok(()) => self.status("goal updated; pending steps are stale", StatusKind::Ok),
                    Err(e) => self.status(&format!("goal update failed: {e:#}"), StatusKind::Err),
                }
            }
            Ok(None) => self.status("no active plan", StatusKind::Warn),
            Err(e) => self.status(&format!("plan load failed: {e:#}"), StatusKind::Err),
        }
    }

    fn constraints_command(&mut self, rest: &str) {
        let root = std::env::current_dir().unwrap_or_default();
        let mut parts = rest.splitn(3, ' ');
        let _ = parts.next();
        let action = parts.next().unwrap_or_default();
        let text = parts.next().unwrap_or_default().trim();
        if text.is_empty() || !matches!(action, "add" | "remove") {
            self.status("usage: /constraints add|remove <text>", StatusKind::Warn);
            return;
        }
        match plan::open_active(&root) {
            Ok(Some(mut active)) => {
                if action == "add" {
                    active.constraints.push(text.to_string());
                } else if let Some(index) = active.constraints.iter().position(|c| c == text) {
                    active.constraints.remove(index);
                }
                active.revision += 1;
                match plan::store(&root, &active) {
                    Ok(()) => self.status("constraints updated", StatusKind::Ok),
                    Err(e) => self.status(
                        &format!("constraints update failed: {e:#}"),
                        StatusKind::Err,
                    ),
                }
            }
            Ok(None) => self.status("no active plan", StatusKind::Warn),
            Err(e) => self.status(&format!("plan load failed: {e:#}"), StatusKind::Err),
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
        self.segments.retain(
            |segment| !matches!(segment, Segment::Status { text, .. } if text == Self::BUSY_STATUS),
        );
        self.status(Self::BUSY_STATUS, StatusKind::Warn);
        self.busy_until = Some(Instant::now() + Duration::from_secs(2));
    }

    fn status(&mut self, text: &str, kind: StatusKind) {
        if text == Self::BUSY_STATUS {
            self.segments.retain(|segment| {
                !matches!(segment, Segment::Status { text: existing, .. } if text == existing)
            });
        }
        if kind == StatusKind::Err {
            self.bar_error = Some(text.to_string());
        }
        if self.menu_stack.is_empty() {
            // with no menu open the chat carries the message
            self.segments.push(Segment::Status {
                text: text.to_string(),
                kind,
                expanded: false,
            });
        } else {
            // never pollute the chat from inside a menu: show it in the menu
            self.menu_status = Some((text.to_string(), kind));
        }
        self.dirty = true;
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
                    self.segments.push(Segment::Subagent {
                        id,
                        task,
                        status: "running".into(),
                        output: String::new(),
                        expanded: false,
                    });
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
                    self.dirty = true;
                }
                AgentEvent::SubagentToolStart { id, name, summary } => {
                    if let Some(chat) = self.subagent_chats.get_mut(&id) {
                        let pos = chat
                            .iter()
                            .rposition(|segment| {
                                matches!(segment, Segment::Assistant { live: true, .. })
                            })
                            .unwrap_or(chat.len());
                        chat.insert(
                            pos,
                            Segment::Tool {
                                name,
                                args: summary,
                                ok: None,
                                output: String::new(),
                                diff: None,
                                expanded: false,
                            },
                        );
                    }
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
                        && let Some(Segment::Tool { ok: state, output, diff: current_diff, .. }) = chat.iter_mut().rev().find(|segment| matches!(segment, Segment::Tool { name: current, ok: None, .. } if current == &name))
                    {
                        *state = Some(ok);
                        *output = summary;
                        *current_diff = diff;
                    }
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
                    if let Some(Segment::Subagent {
                        status,
                        output: current,
                        ..
                    }) = self
                        .segments
                        .iter_mut()
                        .rev()
                        .find(|s| matches!(s, Segment::Subagent { id: sid, .. } if *sid == id))
                    {
                        *status = if ok {
                            "completed".into()
                        } else {
                            "failed".into()
                        };
                        *current = output;
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
                        self.segments.push(Segment::Status {
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
    }

    /// append reasoning text, opening a fresh thinking row when none is open.
    /// This keeps multiple reasoning blocks separate and interleaved with the
    /// tool calls that follow them (think -> tool -> think -> tool -> answer).
    fn handle_thinking_delta(&mut self, t: String) {
        if !self.thinking_open {
            self.thinking_open = true;
            // reasoning precedes the answer: insert before the live assistant
            let pos = self
                .segments
                .iter()
                .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
                .unwrap_or(self.segments.len());
            self.segments.insert(
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
        }
        self.dirty = true;
    }

    /// close any open reasoning block, then insert a running tool row above the
    /// live answer (tool -> result -> answer). A tool call ends the current
    /// reasoning block, so the next ThinkingDelta opens its own row instead of
    /// piling onto the previous one.
    fn handle_tool_start(&mut self, name: String, summary: String) {
        // ask_user has its own inline Q&A segment (AgentEvent::AskUser); a
        // parallel Tool row would duplicate it and its expansion used to be
        // empty because `args` here is only a one-line summary, not the JSON.
        // propose_plan is the same: the PlanProposal segment is the surface.
        if name == "ask_user" || name == "propose_plan" {
            return;
        }
        if self.thinking_open {
            if let Some(i) = self.thinking_idx.take() {
                self.freeze_thinking(i);
                let empty = matches!(
                    self.segments.get(i),
                    Some(Segment::Thinking { text, .. }) if text.is_empty()
                );
                if empty {
                    self.segments.remove(i);
                }
            }
            self.thinking_open = false;
        }
        let tool = Segment::Tool {
            name,
            args: summary,
            ok: None,
            output: String::new(),
            diff: None,
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
        self.segments.insert(pos, tool);
        self.dirty = true;
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
                    ..
                }) = self.segments.get_mut(i)
                {
                    *slot = Some(ok);
                    *output = summary;
                    *dslot = diff;
                }
            }
            None => {
                let tool = Segment::Tool {
                    name,
                    args: String::new(),
                    ok: Some(ok),
                    output: summary,
                    diff,
                    expanded: false,
                };
                let pos = self
                    .segments
                    .iter()
                    .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
                    .unwrap_or(self.segments.len());
                self.segments.insert(pos, tool);
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
                    self.segments.remove(i);
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
        self.segments.retain(
            |segment| !matches!(segment, Segment::Status { text, .. } if text == Self::BUSY_STATUS),
        );
        self.busy_until = None;
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
            self.segments.retain(|s| !Self::is_subagent_row(s));
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
            self.segments.remove(i);
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
                self.segments[pos] = Segment::Assistant {
                    text: t.clone(),
                    live: false,
                };
            }
        } else if !text.is_empty() {
            if let Some(pos) = self
                .segments
                .iter()
                .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
            {
                self.segments[pos] = Segment::Assistant {
                    text: text.clone(),
                    live: false,
                };
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
                self.segments.remove(pos);
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
            if *is_error {
                self.bar_error = Some(note.clone());
            }
            self.status(
                note,
                if *is_error {
                    StatusKind::Err
                } else {
                    StatusKind::Info
                },
            );
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

    /// switch the palette and repaint everything that caches colors
    fn apply_theme(&mut self, idx: usize) {
        let applied = crate::tui::theme::set_theme(idx);
        self.cfg.ui.theme = applied;
        self.cfg.save().ok();
        // rendered lines are cached by text length only — drop them so every
        // message repaints in the new palette (otherwise old accents linger)
        self.seg_cache.clear();
        self.cache_lines.clear();
        self.cache_rowseg.clear();
        // textareas capture their styles at creation time
        let restyle = |ta: &mut TextArea<'static>| {
            ta.set_style(Theme::base());
            ta.set_cursor_line_style(Style::new().bg(Theme::SURFACE()));
            ta.set_cursor_style(Style::new().bg(Theme::ACCENT()).fg(Theme::BG()));
            ta.set_selection_style(Style::new().bg(Theme::ACCENT()).fg(Theme::BG()));
        };
        restyle(&mut self.input);
        for f in self.form_fields.iter_mut() {
            if let FormField::Text { ta, .. } = f {
                restyle(ta);
            }
        }
        // no status note on theme switch — the live repaint is the feedback
        self.build_menu_rows();
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
                if let Ok(Some(mut active)) = crate::plan::open_active(&root)
                    && crate::plan::reopen_for_undo(
                        &mut active,
                        step,
                        format!("reopened by undo of step {step}"),
                    )
                    .is_ok()
                {
                    if crate::plan::store(&root, &active).is_ok() {
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
        if next >= max {
            self.follow = true;
        } else {
            self.follow = false;
            self.view_top = next;
        }
        self.dirty = true;
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
