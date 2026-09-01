use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders};
use tui_textarea::TextArea;

use crate::agent::loop_task::{
    AgentEvent, AgentHandle, AgentOutcome, ControlMsg, ApprovalDecision, spawn_agent,
};
use crate::config::{Config, ModelConfig, ThinkingLevel};
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

    input: TextArea<'static>,
    segments: Vec<Segment>,

    streaming: bool,
    aborted: bool,
    agent: Option<AgentHandle>,
    /// to-do list written by the agent's todowrite tool
    todos: Vec<String>,
    /// checked options in the current multi-select ask_user
    ask_picked: Vec<bool>,
    assistant_buf: String,
    /// arrived text not yet revealed to the screen (typewriter effect)
    pending_reveal: String,
    thinking_open: bool,
    thinking_idx: Option<usize>,
    mode: Mode,
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
    /// per-segment render cache: (content key at render time, content lines)
    seg_cache: Vec<Option<(usize, Vec<(Line<'static>, Option<usize>)>)>>,
    /// stable order/identity of segments used to invalidate positional caches
    seg_layout: Vec<u64>,

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
    status_y: u16,

    // mouse selection
    press: Option<CellPos>,
    dragging: bool,
    sel: Option<Selection>,
}

impl App {
    /// system prompt: built-in markdown, overridable by config_dir/system.md
    fn system_prompt(&self) -> String {
        crate::prompts::system_prompt()
    }

    pub fn new(cfg: Config, session: Session) -> Result<Self> {
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
            cfg,
            segments: Vec::new(),
            streaming: false,
            aborted: false,
            agent: None,
            todos: Vec::new(),
            ask_picked: Vec::new(),
            assistant_buf: String::new(),
            pending_reveal: String::new(),
            thinking_open: false,
            thinking_idx: None,
            mode: Mode::Act,
            bar_error: None,
            prev_turn_ok: false,
            retry_notified: true, // no toast for the very first turn
            retry_line: None,
            last_checkpoint: None,
            follow: true,
            view_top: 0,
            spinner_tick: 0,
            quit: false,
            dirty: true,
            cache_w: 0,
            cache_lines: Vec::new(),
            cache_rowseg: Vec::new(),
            last_chat: Rect::default(),
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
            status_y: 0,
            press: None,
            dragging: false,
            sel: None,
        };
        app.load_history_segments();
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
        // the to-do list travels with the session, not the live agent turn
        self.todos = self.session.todos.clone();
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
            self.spinner_tick = self.spinner_tick.wrapping_add(1);
            self.dirty |= self.streaming;
            terminal.draw(|f| self.draw(f))?;
            self.dirty = false;
        }
        self.session.save().ok();
        Ok(())
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
            .filter(|(_, (cmd, _))| cmd.starts_with(&t))
            .map(|(i, _)| i)
            .collect()
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
        self.input = Self::fresh_input(String::new());
        self.popup_dismiss = false;
        self.hover = None;
        if let Some(rest) = text.strip_prefix('/') {
            self.command(rest);
            return;
        }
        if self.streaming {
            self.status("busy: esc stops the current generation", StatusKind::Warn);
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
        self.segments.push(Segment::User(text.clone()));
        self.session.push(Role::User, text);

        let mut msgs: Vec<PMessage> = vec![PMessage::new(Role::System, self.system_prompt())];
        msgs.extend(self.session.messages.iter().cloned());
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
            messages: msgs,
            root,
            blocked_patterns: self.cfg.safety.blocked_patterns.clone(),
            plan_mode: self.mode == Mode::Plan,
        };
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

    // ---------- providers / models menu ----------

    fn apply_session(&mut self, mut s: Session) {
        // persist the session we are leaving
        self.session.save().ok();
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
        self.session = s;
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
        if self.streaming {
            self.status(
                "busy: esc stops the current generation first",
                StatusKind::Warn,
            );
            return false;
        }
        let ctx = self.session.context_limit;
        self.session = Session::new(self.cfg.default_model.clone(), ctx);
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
            self.status(
                "busy: esc stops the current generation first",
                StatusKind::Warn,
            );
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
            "/help" => self.open_menu(Menu::Help),
            "/debug" => self.open_menu(Menu::Debug),
            "/themes" | "/theme" => self.open_menu(Menu::Themes),
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
            "/plan" => {
                self.mode = Mode::Plan;
                self.status("mode: PLAN", StatusKind::Info);
            }
            "/act" => {
                self.mode = Mode::Act;
                self.status("mode: ACT", StatusKind::Info);
            }
            "/new" => {
                self.start_new_session();
            }
            "/sessions" => self.open_menu(Menu::Sessions),
            "/fork" => {
                if self.session.messages.is_empty() {
                    self.status("nothing to fork yet", StatusKind::Warn);
                } else if self.streaming {
                    self.status(
                        "busy: esc stops the current generation first",
                        StatusKind::Warn,
                    );
                } else {
                    self.open_menu(Menu::ForkPoint);
                }
            }
            "/providers" => self.open_menu(Menu::Providers),
            "/models" => self.open_menu(Menu::Models {
                provider: self.model_cfg.provider.clone(),
            }),
            "/exit" | "/quit" | "/q" => self.quit = true,
            "/undo" => {
                if self.streaming {
                    self.status(
                        "busy: esc stops the current generation first",
                        StatusKind::Warn,
                    );
                } else {
                    let n = rest
                        .split_whitespace()
                        .nth(1)
                        .and_then(|x| x.parse::<usize>().ok())
                        .unwrap_or(1);
                    self.undo(n);
                }
            }
            other if COMMANDS.iter().any(|(c, _)| *c == other) => {
                self.status(&format!("{other}: not implemented yet"), StatusKind::Warn)
            }
            "" => {}
            other => self.status(
                &format!("unknown command {other} — try /help"),
                StatusKind::Warn,
            ),
        }
        self.dirty = true;
    }

    fn status(&mut self, text: &str, kind: StatusKind) {
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
                        },
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
                AgentEvent::Approval { id, command, reason } => {
                    self.open_menu(Menu::Approval { id, command, reason });
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
        // the agent owns the authoritative conversation; persist it whole
        self.session.messages = outcome.messages;
        self.todos = outcome.todos;
        // carry the to-do list onto the session so it survives the next save
        self.session.todos = self.todos.clone();
        self.session.checkpoints.extend(outcome.journal);
        self.finish_turn(Ok(()));
    }

    fn finish_turn(&mut self, res: Result<(), String>) {
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
        // the final visible assistant message is the last assistant message
        // without tool calls in the (now updated) session
        let final_text = self
            .session
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && m.tool_calls.is_empty())
            .map(|m| m.content.clone());
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
            if !t.is_empty() {
                // ensure the final answer is persisted even if the live buffer
                // had nothing (e.g. answer arrived with no deltas)
                let _ = text;
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
            self.session.push(Role::Assistant, text.clone());
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
        };
        restyle(&mut self.input);
        for f in self.form_fields.iter_mut() {
            if let FormField::Text { ta, .. } = f {
                restyle(ta);
            }
        }
        // stay inside the menu so the user can browse palettes live
        self.status(
            &format!("theme: {}", crate::tui::theme::THEMES[applied].name),
            StatusKind::Ok,
        );
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
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
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
