use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders};
use std::time::{Duration, Instant};
use tui_textarea::TextArea;

use crate::agent::loop_task::{
    AgentEvent, AgentHandle, AgentOutcome, ApprovalDecision, ControlMsg, spawn_agent,
};
use crate::config::{Config, ModelConfig, ThinkingLevel};
use crate::plan;
use crate::providers::{self, Message as PMessage, Role, SharedProvider};
use crate::session::Session;
use crate::tui::markdown::Highlighter;
use crate::tui::theme::Theme;

pub type Terminal = ratatui::Terminal<CrosstermBackend<std::io::Stdout>>;

mod events;
mod forms;
mod menus;
#[cfg(test)]
mod tests;
mod view;

use forms::FormField;
use menus::{Menu, MenuAction};
use view::{CellPos, Segment, Selection};

use menus::COMMANDS;

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
    /// Whether the current prompt still needs the full tool-oriented context.
    context_bootstrap_pending: bool,
    active_skills: Vec<crate::prompts::skills::Skill>,

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
    /// checked options in the current multi-select ask_user
    ask_picked: Vec<bool>,
    assistant_buf: String,
    /// arrived text not yet revealed to the screen (typewriter effect)
    pending_reveal: String,
    thinking_open: bool,
    thinking_idx: Option<usize>,
    mode: Mode,
    /// true while showing the no-session startup screen
    startup: bool,
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

    /// latest diagnostic count reported by the LSP manager
    lsp_diagnostics: usize,
    follow: bool,
    /// absolute top line of the viewport when not following (None-equivalent: follow == true)
    view_top: usize,
    spinner_tick: usize,
    quit: bool,

    dirty: bool,
    cache_w: u16,
    cache_lines: Vec<Line<'static>>,
    cache_rowseg: Vec<Option<usize>>,
    last_chat: Rect,
    last_input: Rect,
    /// per-segment render cache: (content key at render time, content lines)
    seg_cache: Vec<Option<(usize, Vec<(Line<'static>, Option<usize>)>)>>,
    /// stable order/identity of segments used to invalidate positional caches
    seg_layout: Vec<u64>,

    /// Ignore one Enter that can follow a terminal-generated Ctrl+V event.
    /// Windows terminals may enqueue the key release/accept sequence after
    /// bracketed/clipboard paste; it must not submit the newly pasted prompt.
    paste_enter_guard: bool,

    /// Deadline for the single transient busy notice.
    busy_until: Option<Instant>,

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
    /// cached session list for the sessions menus
    sessions: Vec<Session>,
    /// live filter typed inside the sessions menu
    sessions_filter: String,
    th_click: Option<(u16, u16)>,
    agents_click: Option<(u16, u16)>,
    status_y: u16,

    // mouse selection
    press: Option<CellPos>,
    dragging: bool,
    sel: Option<Selection>,
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

    /// Assemble the system block for one request.
    ///
    /// Order matters: the stable prefix comes first, the durable plan next
    /// (it only changes when the agent rewrites it), and everything that moves
    /// while the agent works goes last so it cannot invalidate the prefix.
    fn system_block(&self, with_tools: bool) -> Vec<crate::providers::SystemPart> {
        use crate::providers::SystemPart;
        if !with_tools {
            return vec![SystemPart::volatile(crate::prompts::concise_prompt())];
        }
        let mut parts = vec![SystemPart::cached(self.stable_prefix.clone())];
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
        // re-read once per submitted turn, never cached
        parts.push(SystemPart::volatile(crate::prompts::runtime_context()));
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
                thinking: ThinkingLevel::Off,
                price_in: None,
                price_out: None,
            });
        let resolved = cfg.resolve_provider(&model_cfg)?;
        let provider = providers::create(&resolved)?;

        let mut app = Self {
            input: Self::fresh_input(String::new()),
            model_cfg,
            provider,
            session,
            hl: Highlighter::new(),
            stable_prefix: String::new(),
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
            assistant_buf: String::new(),
            pending_reveal: String::new(),
            thinking_open: false,
            thinking_idx: None,
            mode: Mode::Act,
            startup,
            read_only,
            bar_error: None,
            prev_turn_ok: false,
            retry_notified: true, // no toast for the very first turn
            retry_line: None,
            last_checkpoint: None,
            lsp_diagnostics: 0,
            follow: true,
            view_top: 0,
            spinner_tick: 0,
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
            th_click: None,
            agents_click: None,
            status_y: 0,
            press: None,
            dragging: false,
            sel: None,
            paste_enter_guard: false,
            busy_until: None,
        };
        if !startup {
            app.load_history_segments();
        }
        app.stable_prefix = app.stable_prefix();
        app.context_bootstrap_pending = true;
        Ok(app)
    }

    /// render persisted messages as chat segments (used on start and on resume)
    fn load_history_segments(&mut self) {
        for m in &self.session.messages {
            match m.role {
                Role::User => self.segments.push(Segment::User(m.content.clone())),
                Role::Assistant if m.tool_calls.is_empty() => {
                    self.segments.push(Segment::Assistant {
                        text: m.content.clone(),
                        live: false,
                    });
                }
                // assistant tool-call turns and tool results are housekeeping
                Role::Assistant | Role::System | Role::Tool => {}
            }
        }
        // The panel is derived from the active structured plan; legacy session
        // to-do state is intentionally not loaded.
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
        input.set_cursor_style(Style::new().bg(Theme::ACCENT()).fg(Theme::BG()));
        input
    }

    fn input_block() -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Theme::border_focused())
    }

    pub async fn run(mut self, mut terminal: Terminal) -> Result<()> {
        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<crossterm::event::Event>();
        std::thread::spawn(move || {
            while let Ok(ev) = crossterm::event::read() {
                if ev_tx.send(ev).is_err() {
                    break;
                }
            }
        });

        let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
        while !self.quit {
            tick.tick().await;
            self.poll_input(&ev_rx)?;
            self.poll_agent();
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
            crate::tui::theme::set_anim_tick(self.spinner_tick as u64);
            // keep the /themes live swatches animating while the menu is open
            if matches!(self.cur_menu(), Some(Menu::Themes)) {
                self.build_menu_rows();
            }
            // an animated theme shifts the chat's baked frame/border colors
            // every frame, so force a re-assemble (and thus re-render) of the
            // transcript while one is active
            if crate::tui::theme::anim_theme_index().is_some() {
                self.dirty = true;
            }
            self.dirty |= self.streaming;
            terminal.draw(|f| self.draw(f))?;
            self.dirty = false;
        }
        // Persist a final diary entry before saving the session. The model
        // writer is bounded; host-only content remains available if it fails.
        if self.session_has_messages() && !self.read_only {
            let root = std::env::current_dir().unwrap_or_default();
            let _ = crate::agent::diary::write_entry(
                &root,
                crate::agent::diary::today(),
                &self.session.id.to_string(),
                "session_end",
                Some(&self.provider),
                &self.model_cfg.id,
                crate::plan::open_active(&root)
                    .ok()
                    .flatten()
                    .map(|plan| crate::plan::render(&plan))
                    .as_deref(),
                None,
                self.session
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == crate::providers::Role::User)
                    .map(|message| message.content.as_str()),
                Some(self.cfg.diary.token_budget),
                Some(Duration::from_secs(self.cfg.diary.timeout_secs)),
            )
            .await;
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

    /// Rebuild the provider client from the current config so changes made
    /// while sqwai is running — above all the API key — take effect on the next
    /// turn without restarting the app. The agent clones this handle per turn,
    /// so a key edited mid-turn is picked up when the next turn starts (after a
    /// normal Esc stop, or simply the next message).
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
        self.bar_error = None;
        self.retry_notified = false;
        self.retry_line = None;
        self.last_checkpoint = None;
        let text = self.input_text();
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.streaming {
            self.show_busy_status();
            return;
        }
        self.input = Self::fresh_input(String::new());
        self.popup_dismiss = false;
        self.hover = None;
        if let Some(rest) = text.strip_prefix('/') {
            self.command(rest);
            return;
        }
        if let Some(pc) = self.cfg.providers.get(&self.model_cfg.provider)
            && pc.effective_api_key(&self.model_cfg.provider).is_none()
        {
            self.status(
                &format!("provider '{}' has no api key", self.model_cfg.provider),
                StatusKind::Err,
            );
            return;
        }
        // pick up any provider/key change made since the last turn
        self.rebuild_provider();
        self.segments.push(Segment::User(text.clone()));
        self.session.push(Role::User, &text);

        let with_tools = self.context_bootstrap_pending || !Self::is_trivial_request(&text);
        // The system block is assembled per request and travels separately
        // from the transcript: nothing here is ever written to the session.
        let system = self.system_block(with_tools);
        let msgs: Vec<PMessage> = self.session.messages.clone();
        let root = std::env::current_dir().unwrap_or_default();
        let input = crate::agent::loop_task::AgentInput {
            provider: self.provider.clone(),
            model_id: self.model_cfg.id.clone(),
            thinking: if self.model_cfg.thinking == ThinkingLevel::Off {
                None
            } else {
                Some(self.model_cfg.thinking)
            },
            max_tokens: None,
            system,
            messages: msgs,
            root,
            session_id: self.session.id.to_string(),
            blocked_patterns: self.cfg.safety.blocked_patterns.clone(),
            plan_mode: self.mode == Mode::Plan,
            context_limit: self.session.context_limit,
            enable_tools: with_tools,
            read_only: self.read_only,
            mcp: self.cfg.mcp.clone(),
            lsp: self.cfg.lsp.clone(),
            // A continuation reference only travels with the model that
            // produced it, and only for providers that document the field.
            previous_response_id: if self.context_bootstrap_pending {
                None
            } else {
                self.session.response_id_for(&self.session.model_key)
            },
            summary: self.session.summary.clone(),
            compact_only: false,
            diary: self.cfg.diary.clone(),
            memory: self.cfg.memory.clone(),
            subagent_depth: 0,
        };
        self.context_bootstrap_pending = false;
        self.agent = Some(spawn_agent(input));
        self.streaming = true;
        self.aborted = false;
        self.assistant_buf.clear();
        // show the thinking placeholder right away so the indicator is visible
        // from turn start even before any reasoning deltas arrive
        let tpos = self.segments.len();
        self.segments.push(Segment::Thinking {
            text: String::new(),
            expanded: false,
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
            thinking: None,
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
            subagent_depth: 0,
        };
        self.agent = Some(spawn_agent(input));
        self.streaming = true;
        self.aborted = false;
        self.assistant_buf.clear();
        self.status("compacting context…", StatusKind::Info);
    }

    fn is_trivial_request(text: &str) -> bool {
        let t = text.trim().to_lowercase();
        matches!(
            t.as_str(),
            "привет" | "привет!" | "hello" | "hi" | "2 + 2" | "2+2"
        )
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
        // defensive: never let a legacy system turn back into the transcript
        self.session.strip_system_messages();
        self.segments.clear();
        self.seg_cache.clear();
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
        self.startup = false;
        if self.streaming {
            self.show_busy_status();
            return false;
        }
        let ctx = self.session.context_limit;
        self.session = Session::new(self.cfg.default_model.clone(), ctx);
        self.context_bootstrap_pending = true;
        self.segments.clear();
        self.seg_cache.clear();
        self.follow = true;
        self.view_top = 0;
        self.menu_home();
        self.dirty = true;
        self.status(
            &format!("session {} started", short_id(&self.session)),
            StatusKind::Ok,
        );
        true
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
            .and_then(|rp| providers::create(&rp).map(|p| p))
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

            "/themes" | "/theme" => self.open_menu(Menu::Themes),
            "/graph-rebuild" => {
                if self.streaming {
                    self.show_busy_status();
                } else {
                    let root = std::env::current_dir().unwrap_or_default();
                    match crate::agent::graph::CozoGraphStore::open(&root).and_then(|mut store| {
                        crate::agent::graph_index::index_project(&mut store, &root)
                    }) {
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
            "/exit" | "/quit" | "/q" => self.quit = true,
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
                    let n = rest
                        .split_whitespace()
                        .nth(1)
                        .and_then(|x| x.parse::<usize>().ok())
                        .unwrap_or(1);
                    self.undo(n);
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

    fn plan_command(&mut self, rest: &str) {
        let root = std::env::current_dir().unwrap_or_default();
        let args: Vec<&str> = rest.split_whitespace().skip(1).collect();
        let result = match args.first().copied() {
            None | Some("show") => plan::open_active(&root)
                .ok()
                .flatten()
                .map(|p| plan::render(&p))
                .unwrap_or_else(|| "no active plan".to_string()),
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
                "plan limit is configured through the plan settings; runtime override pending"
                    .to_string()
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
        {
            if let Some(Segment::Assistant { text, .. }) = self.segments.get_mut(pos) {
                text.push_str(&chunk);
            }
        }
        !chunk.is_empty()
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
                        // agent died without a final Completed event
                        self.agent = None;
                        self.finish_turn(Ok(()));
                        return;
                    }
                },
                None => return,
            };
            match ev {
                AgentEvent::TextDelta(t) => {
                    // drop the reasoning placeholder once the answer starts and
                    // it never received any thought text
                    if self.thinking_open {
                        let is_empty = self
                            .thinking_idx
                            .and_then(|i| match self.segments.get(i) {
                                Some(Segment::Thinking { text, .. }) => Some(text.is_empty()),
                                _ => None,
                            })
                            .unwrap_or(false);
                        if is_empty {
                            if let Some(i) = self.thinking_idx.take() {
                                self.segments.remove(i);
                                self.thinking_open = false;
                            }
                        }
                    }
                    // queue for the typewriter reveal instead of showing at once
                    self.pending_reveal.push_str(&t);
                    self.dirty = true;
                }
                AgentEvent::ThinkingDelta(t) => {
                    if !self.thinking_open {
                        self.thinking_open = true;
                        // thinking precedes the answer: insert before the live assistant
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
                                live: true,
                            },
                        );
                        self.thinking_idx = Some(pos);
                    }
                    if let Some(i) = self.thinking_idx {
                        if let Some(Segment::Thinking { text, .. }) = self.segments.get_mut(i) {
                            text.push_str(&t);
                        }
                    }
                    self.dirty = true;
                }
                AgentEvent::Usage(u) => {
                    self.session.add_usage(&u);
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
                } => {
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
                    let tool = Segment::Tool {
                        name,
                        args: summary,
                        ok: None,
                        output: String::new(),
                        diff: None,
                        expanded: false,
                    };
                    // The model may stream a short preamble before emitting
                    // its tool call. Keep tool activity above the live answer
                    // so the chat reads in execution order: tool -> result -> answer.
                    let pos = self
                        .segments
                        .iter()
                        .rposition(|s| matches!(s, Segment::Assistant { live: true, .. }))
                        .unwrap_or(self.segments.len());
                    self.segments.insert(pos, tool);
                    self.dirty = true;
                }
                AgentEvent::ToolNotice {
                    name,
                    summary,
                    ok,
                    diff,
                } => {
                    // close the row opened by ToolStart; fall back to a new one
                    let hit = self.segments.iter().rposition(
                        |s| matches!(s, Segment::Tool { name: n, ok: None, .. } if *n == name),
                    );
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
                AgentEvent::Checkpoint { label } => {
                    self.last_checkpoint = Some(label);
                    self.dirty = true;
                }
                AgentEvent::Todos(items) => {
                    self.todos = items;
                    self.dirty = true;
                }
                AgentEvent::AskUser {
                    id,
                    question,
                    options,
                    multiple,
                    allow_free,
                } => {
                    let opt = options
                        .into_iter()
                        .map(|o| (o.label, o.description))
                        .collect();
                    self.open_menu(Menu::AskUser {
                        id,
                        question,
                        options: opt,
                        multiple,
                        allow_free,
                    });
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
                        // while here it stays readable and copyable (click to copy)
                        self.segments.push(Segment::Status {
                            text: format!("request failed — retrying with backoff: {error}"),
                            kind: StatusKind::Err,
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
                    match res {
                        Ok(outcome) => self.finish_turn_ok(outcome),
                        Err(e) => self.finish_turn(Err(e)),
                    }
                    return;
                }
            }
        }
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
        self.session.checkpoints.extend(outcome.journal);
        self.finish_turn(Ok(()));
    }

    fn clear_busy_statuses(&mut self) {
        self.segments.retain(
            |segment| !matches!(segment, Segment::Status { text, .. } if text == Self::BUSY_STATUS),
        );
        self.busy_until = None;
    }

    fn clear_subagent_ui_on_stop(&mut self) {
        self.subagents.clear();
        self.segments.retain(|segment| {
            !matches!(segment, Segment::Subagent { .. })
                && !matches!(segment, Segment::Tool { name, .. } if name == "subagent")
        });
        if matches!(self.cur_menu(), Some(Menu::Subagents)) {
            self.menu_home();
        }
        self.agents_click = None;
        self.dirty = true;
    }

    fn finish_turn(&mut self, res: Result<(), String>) {
        self.clear_busy_statuses();
        if res.as_ref().is_err_and(|error| error == "aborted") {
            self.clear_subagent_ui_on_stop();
        }
        // flush whatever the typewriter has not revealed yet
        self.reveal_chars(usize::MAX);
        let text = std::mem::take(&mut self.assistant_buf);
        self.thinking_open = false;
        self.thinking_idx = None;
        for s in &mut self.segments {
            if let Segment::Thinking { live, .. } = s {
                *live = false;
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
        self.agent = None;
        self.retry_line = None;
        self.session.save().ok();
        self.prev_turn_ok = matches!(res, Ok(()));
        match res {
            Ok(()) => {}
            Err(e) if e == "aborted" => self.status("stopped", StatusKind::Info),
            Err(e) if e == "tui closed" => {}
            Err(e) => {
                self.bar_error = Some(format!("error: {e}"));
                self.status(&format!("error: {e}"), StatusKind::Err)
            }
        }
        self.dirty = true;
    }

    /// switch the palette and repaint everything that caches colors
    fn apply_theme(&mut self, idx: usize) {
        let applied = crate::tui::theme::set_theme(idx);
        crate::tui::theme::set_anim_theme_off();
        self.cfg.ui.theme = applied;
        self.cfg.ui.anim_theme = None;
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

    /// switch to an animated (time-driven) theme and repaint
    fn apply_anim_theme(&mut self, idx: usize) {
        let applied = crate::tui::theme::set_anim_theme(idx);
        self.cfg.ui.anim_theme = Some(applied);
        self.cfg.save().ok();
        // same cache/textarea refresh as a static theme switch
        self.seg_cache.clear();
        self.cache_lines.clear();
        self.cache_rowseg.clear();
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
        self.build_menu_rows();
        self.dirty = true;
    }

    /// revert the last `n` mutating actions via git checkpoints (design §6)
    fn undo(&mut self, n: usize) {
        let n = n.max(1);
        if self.session.checkpoints.is_empty() {
            self.status("nothing to undo", StatusKind::Info);
            return;
        }
        let idx = self.session.checkpoints.len().saturating_sub(n);
        let (sha, label) = self.session.checkpoints[idx].clone();
        let root = std::env::current_dir().unwrap_or_default();
        if !crate::agent::checkpoints::available(&root) {
            self.status("not a git repo — undo unavailable", StatusKind::Warn);
            return;
        }
        match crate::agent::checkpoints::restore(&root, &sha) {
            Ok(()) => {
                self.session.checkpoints.truncate(idx);
                self.session.save().ok();
                self.status(&format!("undo: reverted '{label}'"), StatusKind::Ok);
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
