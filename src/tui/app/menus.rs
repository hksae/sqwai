#![allow(unused_imports)]
use super::*;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tui_textarea::TextArea;

use crate::agent::loop_task::AgentEvent;
use crate::config::{Config, ModelConfig, ThinkingLevel, WireFormat};
use crate::providers::{self, ChatRequest, Message as PMessage, Role, SharedProvider};
use crate::session::Session;
use crate::tui::markdown::{Highlighter, render, wrap_tagged};
use crate::tui::theme::Theme;

pub(super) const COMMANDS: &[&str] = &[
    "/new",
    "/sessions",
    "/fork",
    "/debug",
    "/providers",
    "/models",
    "/plan",
    "/goal",
    "/constraints",
    "/mode",
    "/compact",
    "/diary",
    "/undo",
    "/init",
    "/graph-rebuild",
    "/themes",
    "/settings",
    "/mcp",
    "/lsp",
    "/skills",
    "/skill",
    "/exit",
];

pub(super) const POPUP_MAX_ROWS: usize = 14;

#[derive(Clone)]
pub(super) enum Menu {
    /// /settings: top-level settings hub
    Settings,
    /// /settings -> Appearance: organizer for visual settings
    Appearance,
    Mcp,
    Lsp,
    Skills,
    Providers,
    Models {
        provider: String,
    },
    PickModel {
        provider: String,
    },
    Sessions,
    DeleteSessions,
    /// pick a message to copy history up to
    ForkPoint,
    /// /debug: runtime toggles and diagnostics
    Debug,
    /// /themes: palette browser
    Themes,
    /// single-field form to rename a session
    EditSessionTitle {
        id: String,
    },
    EditProvider {
        name: Option<String>,
    },
    EditModel {
        provider: String,
        key: Option<String>,
    },
    ConfirmDelete {
        label: String,
        action: MenuAction,
    },
    DeleteModelList {
        provider: String,
    },
    Thinking,
    /// the model asked the user a structured question (ask_user)
    AskUser {
        id: u64,
        question: String,
        options: Vec<(String, Option<String>)>,
        multiple: bool,
        allow_free: bool,
    },
    /// a dangerous command needs explicit approval
    Approval {
        id: u64,
        command: String,
        reason: String,
    },
    /// free-text answer for an open ask_user (single-field form)
    AskFree {
        id: u64,
    },
    /// the active plan's visible steps, opened with Ctrl+T
    Todo,
    /// all delegated child agents, opened with Ctrl+A
    Subagents,
}

#[derive(Clone)]
pub(super) enum MenuAction {
    None,
    Back,
    OpenModels(String),
    OpenAppearance,
    OpenThemes,
    OpenProviders,
    OpenMcp,
    OpenLsp,
    OpenSkills,
    AddProvider,
    EditProvider(String),
    DeleteProvider(String),
    AddModel(String),
    EditModel(String, String),
    DeleteModel(String, String),
    PickModelList(String),
    DeleteModelList(String),
    UseModel(String),
    OpenSession(String),
    NewSession,
    RenameSession(String),
    PinSession(String),
    ForkSessionList,
    /// kept for future use; deletion now goes through the `d` key
    #[allow(dead_code)]
    DeleteSessionList,
    DeleteSession(String),
    /// fork the current session, copying history up to this message index
    ForkAt(usize),
    ToggleTypewriter,
    ToggleHttpLog,
    ToggleShowCost,
    CycleModelThinking,
    CycleDefaultThinking,
    ToggleMode,
    OpenSessions,
    SetTheme(usize),
    SetAnimTheme(usize),
    Confirm(Box<MenuAction>),
    SetThinking(ThinkingLevel),
    OpenSubagent(u64),
    /// ask_user: submit one chosen option's label
    AskSelect(String),
    /// ask_user multi: toggle an option by index
    AskToggle(usize),
    /// ask_user multi: confirm the toggled selection
    AskConfirm,
    /// ask_user: open the free-text form
    AskFree,
}

impl App {
    pub(super) fn cur_menu(&self) -> Option<&Menu> {
        self.menu_stack.last()
    }

    pub(super) fn open_menu(&mut self, menu: Menu) {
        self.menu_stack.push(menu);
        self.menu_sel = 0;
        self.form_fields.clear();
        self.form_focus = 0;
        if let Some(Menu::AskUser { options, .. }) = self.cur_menu() {
            self.ask_picked = vec![false; options.len()];
        }
        // confirmation prompts open with the confirm action highlighted
        if matches!(self.cur_menu(), Some(Menu::ConfirmDelete { .. })) && self.menu_sel == 0 {
            self.menu_sel = 1;
        }
        // unit tests inject the cache directly and never hit the disk
        #[cfg(not(test))]
        if matches!(self.cur_menu(), Some(Menu::Sessions | Menu::DeleteSessions)) {
            // The menu needs metadata only; defer loading full message histories
            // until the user actually opens a session.
            self.sessions = Session::list_visible(40).unwrap_or_default();
        }
        if matches!(self.cur_menu(), Some(Menu::Sessions)) {
            self.sessions_filter.clear();
        }
        self.prefill_form();
        // rows are ready immediately, not on the next frame
        self.build_menu_rows();
        self.dirty = true;
    }

    /// replace the top of the stack (used after save/confirm flows)
    pub(super) fn open_menu_replace(&mut self, menu: Menu) {
        self.menu_stack.pop();
        self.open_menu(menu);
    }

    pub(super) fn menu_back(&mut self) {
        // interactions: esc cancels/blanks an ask, denies an approval
        match self.cur_menu() {
            Some(Menu::AskUser { .. }) => {
                self.ask_answer(String::new());
                return;
            }
            Some(Menu::Approval { .. }) => {
                self.approval_decide(ApprovalDecision::Deny);
                return;
            }
            _ => {}
        }
        self.menu_stack.pop();
        self.menu_sel = 0;
        self.form_fields.clear();
        self.form_focus = 0;
        // a form below the popped one needs its fields back
        self.prefill_form();
        self.build_menu_rows();
        self.dirty = true;
    }

    pub(super) fn menu_home(&mut self) {
        self.menu_stack.clear();
        self.menu_sel = 0;
        self.form_fields.clear();
        self.form_focus = 0;
        self.menu_rows.clear();
        self.dirty = true;
    }

    pub(super) fn menu_nav(&mut self, dir: i32) {
        let is_form = self.is_form_menu();
        if is_form {
            let n = self.form_fields.len();
            if n > 0 {
                self.form_focus = if dir < 0 {
                    (self.form_focus + n - 1) % n
                } else {
                    (self.form_focus + 1) % n
                };
                // start at the sensible end of the newly focused field
                if dir < 0 {
                    self.form_to_start();
                } else {
                    self.form_to_end();
                }
            }
        } else if dir.abs() > 1 {
            // page jump
            let n = self.menu_rows.len();
            if n > 0 {
                let step = (self.menu_rows.len() as i32 / 2).clamp(1, 10) as usize;
                self.menu_sel = if dir < 0 {
                    self.menu_sel.saturating_sub(step)
                } else {
                    (self.menu_sel + step).min(n - 1)
                };
            }
        } else {
            let n = self.menu_rows.len();
            if n > 0 {
                self.menu_sel = if dir < 0 {
                    (self.menu_sel + n - 1) % n
                } else {
                    (self.menu_sel + 1) % n
                };
            }
        }
        self.dirty = true;
    }

    pub(super) fn menu_jump(&mut self, to_end: bool) {
        if self.is_form_menu() {
            self.menu_nav(if to_end { 1 } else { -1 });
            return;
        }
        if !self.menu_rows.is_empty() {
            self.menu_sel = if to_end { self.menu_rows.len() - 1 } else { 0 };
        }
        self.dirty = true;
    }

    pub(super) fn is_form_menu(&self) -> bool {
        matches!(
            self.cur_menu(),
            Some(
                Menu::EditProvider { .. }
                    | Menu::EditModel { .. }
                    | Menu::EditSessionTitle { .. }
                    | Menu::AskFree { .. }
            )
        )
    }

    pub(super) fn menu_hover(&mut self, row: u16) {
        if self.is_form_menu() {
            return;
        }
        if self.menu_rect.height > 0 && row >= self.menu_rect.y && row < self.menu_rect.bottom() {
            let rel = row.saturating_sub(self.menu_rect.y + 1) as usize; // skip border
            let abs = self.menu_scroll + rel;
            if abs < self.menu_rows.len() && abs != self.menu_sel {
                self.menu_sel = abs;
                self.dirty = true;
            }
        }
    }

    /// id of the session row currently highlighted in the sessions menu
    pub(super) fn selected_session_id(&self) -> Option<String> {
        match &self.menu_rows.get(self.menu_sel)?.1 {
            MenuAction::OpenSession(id) => Some(id.clone()),
            _ => None,
        }
    }

    pub(super) fn in_menu_rect(&self, row: u16, col: u16) -> bool {
        let r = self.menu_rect;
        r.width > 0 && col >= r.x && col < r.right() && row >= r.y && row < r.bottom()
    }

    pub(super) fn menu_click(&mut self, row: u16) {
        self.menu_hover(row);
        // confirmation prompts: honor the exact row clicked (label/cancel/confirm)
        if let Some(Menu::ConfirmDelete { .. }) = self.cur_menu() {
            let Some((_, action)) = self.menu_rows.get(self.menu_sel) else {
                return;
            };
            self.run_action(action.clone());
            return;
        }
        self.menu_activate();
    }

    /// run the confirm action of the open delete-confirmation prompt
    pub(super) fn run_confirm_action(&mut self) {
        let act = self
            .menu_rows
            .iter()
            .find(|(_, a)| matches!(a, MenuAction::Confirm(_)))
            .map(|(_, a)| a.clone());
        if let Some(a) = act {
            self.run_action(a);
        }
    }

    pub(super) fn menu_activate(&mut self) {
        if self.is_form_menu() {
            self.form_save();
            return;
        }
        // confirmation prompts: enter always confirms (esc cancels)
        if let Some(Menu::ConfirmDelete { .. }) = self.cur_menu() {
            self.run_confirm_action();
            return;
        }
        // ask_user: enter submits the selected option (or free text)
        if let Some(Menu::AskUser { .. }) = self.cur_menu() {
            if let Some((_, action)) = self.menu_rows.get(self.menu_sel) {
                self.run_action(action.clone());
            }
            return;
        }
        // approval: enter = run once
        if let Some(Menu::Approval { .. }) = self.cur_menu() {
            self.approval_decide(ApprovalDecision::RunOnce);
            return;
        }
        let Some((_, action)) = self.menu_rows.get(self.menu_sel) else {
            return;
        };
        self.run_action(action.clone());
    }

    /// ask_user: send the chosen answer back to the agent and close
    pub(super) fn ask_answer(&mut self, text: String) {
        let id = match self.cur_menu() {
            Some(Menu::AskUser { id, .. }) | Some(Menu::AskFree { id }) => *id,
            _ => return,
        };
        if let Some(agent) = &self.agent {
            let _ = agent.control.try_send(ControlMsg::AskAnswer { id, text });
        }
        self.ask_picked.clear();
        self.close_interaction();
    }

    /// approval: send the user's decision to the agent and close
    pub(super) fn approval_decide(&mut self, decision: ApprovalDecision) {
        let id = match self.cur_menu() {
            Some(Menu::Approval { id, .. }) => *id,
            _ => return,
        };
        if let Some(agent) = &self.agent {
            let _ = agent
                .control
                .try_send(ControlMsg::ApprovalAnswer { id, decision });
        }
        self.close_interaction();
    }

    /// pop an ask/approval interaction without re-triggering its esc handler
    fn close_interaction(&mut self) {
        self.menu_stack.pop();
        self.menu_sel = 0;
        self.form_fields.clear();
        self.form_focus = 0;
        self.prefill_form();
        self.build_menu_rows();
        self.dirty = true;
    }

    pub(super) fn run_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::None => {}
            MenuAction::Back => self.menu_back(),
            MenuAction::OpenAppearance => self.open_menu(Menu::Appearance),
            MenuAction::OpenThemes => self.open_menu(Menu::Themes),
            MenuAction::OpenProviders => self.open_menu(Menu::Providers),
            MenuAction::OpenMcp => self.open_menu(Menu::Mcp),
            MenuAction::OpenLsp => self.open_menu(Menu::Lsp),
            MenuAction::OpenSkills => self.open_menu(Menu::Skills),
            MenuAction::OpenModels(p) => self.open_menu(Menu::Models { provider: p }),
            MenuAction::AddProvider => self.open_menu(Menu::EditProvider { name: None }),
            MenuAction::EditProvider(name) => {
                self.open_menu(Menu::EditProvider { name: Some(name) })
            }
            MenuAction::DeleteProvider(p) => self.open_menu(Menu::ConfirmDelete {
                label: format!("delete provider '{p}' and all its models?"),
                // stored unwrapped; build_menu_rows adds the single Confirm layer
                action: MenuAction::DeleteProvider(p),
            }),
            MenuAction::AddModel(p) => self.open_menu(Menu::EditModel {
                provider: p,
                key: None,
            }),
            MenuAction::EditModel(p, k) => self.open_menu(Menu::EditModel {
                provider: p,
                key: Some(k),
            }),
            MenuAction::DeleteModel(p, k) => self.open_menu(Menu::ConfirmDelete {
                label: format!("delete model '{k}'?"),
                action: MenuAction::DeleteModel(p, k),
            }),
            MenuAction::PickModelList(p) => self.open_menu(Menu::PickModel { provider: p }),
            MenuAction::DeleteModelList(p) => self.open_menu(Menu::DeleteModelList { provider: p }),
            MenuAction::OpenSession(id) => {
                if id == self.session.id.to_string() {
                    self.menu_home();
                    self.status("already in this session", StatusKind::Info);
                } else if self.streaming {
                    self.show_busy_status();
                } else {
                    match Session::load(&id) {
                        Ok(s) => self.apply_session(s),
                        Err(e) => self.status(&format!("load session: {e:#}"), StatusKind::Err),
                    }
                }
            }
            MenuAction::NewSession => {
                self.start_new_session();
            }
            MenuAction::RenameSession(id) => {
                self.open_menu(Menu::EditSessionTitle { id });
            }
            MenuAction::PinSession(id) => {
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id.to_string() == id) {
                    s.pinned = !s.pinned;
                    let _ = s.save();
                    let state = if s.pinned { "pinned" } else { "unpinned" };
                    self.status(&format!("session {state}"), StatusKind::Ok);
                }
                if self.session.id.to_string() == id {
                    // keep the in-memory copy consistent with the file
                    if let Some(s) = self.sessions.iter().find(|s| s.id.to_string() == id) {
                        self.session.pinned = s.pinned;
                    }
                }
                // re-sort and rebuild while staying in the menu
                Session::sort_sessions(&mut self.sessions);
                self.build_menu_rows();
            }
            MenuAction::ForkSessionList => {
                self.open_menu(Menu::ForkPoint);
            }
            MenuAction::ForkAt(last_idx) => {
                if self.streaming {
                    self.show_busy_status();
                    return;
                }
                let fork = self
                    .session
                    .fork_upto(last_idx.min(self.session.messages.len().saturating_sub(1)));
                self.apply_session(fork);
            }
            MenuAction::DeleteSessionList => {
                self.open_menu(Menu::DeleteSessions);
            }
            MenuAction::ToggleTypewriter => {
                self.cfg.ui.typewriter = !self.cfg.ui.typewriter;
                self.cfg.save().ok();
                let on = self.cfg.ui.typewriter;
                self.status(&format!("typewriter: {}", on_off(on)), StatusKind::Ok);
                self.build_menu_rows();
            }
            MenuAction::ToggleHttpLog => {
                self.cfg.ui.http_log = !self.cfg.ui.http_log;
                self.cfg.save().ok();
                crate::providers::set_http_log(self.cfg.ui.http_log);
                let on = self.cfg.ui.http_log;
                self.status(&format!("http debug log: {}", on_off(on)), StatusKind::Ok);
                self.build_menu_rows();
            }
            MenuAction::ToggleShowCost => {
                self.cfg.ui.show_cost = !self.cfg.ui.show_cost;
                self.cfg.save().ok();
                let on = self.cfg.ui.show_cost;
                self.status(&format!("show cost: {}", on_off(on)), StatusKind::Ok);
                self.build_menu_rows();
            }
            MenuAction::CycleModelThinking => {
                let all = ThinkingLevel::ALL;
                let cur = all
                    .iter()
                    .position(|l| *l == self.model_cfg.thinking)
                    .unwrap_or(0);
                let next = all[(cur + 1) % all.len()];
                self.model_cfg.thinking = next;
                if let Some(m) = self.cfg.models.get_mut(&self.session.model_key) {
                    m.thinking = next;
                }
                self.cfg.save().ok();
                self.build_menu_rows();
            }
            MenuAction::CycleDefaultThinking => {
                let all = ThinkingLevel::ALL;
                let cur = all
                    .iter()
                    .position(|l| *l == self.cfg.default_thinking)
                    .unwrap_or(0);
                self.cfg.default_thinking = all[(cur + 1) % all.len()];
                self.cfg.save().ok();
                self.build_menu_rows();
            }
            MenuAction::ToggleMode => {
                self.mode = self.mode.toggle();
                self.status(&format!("mode: {}", self.mode.label()), StatusKind::Info);
            }
            MenuAction::OpenSessions => {
                self.open_menu(Menu::Sessions);
            }
            MenuAction::SetTheme(idx) => {
                self.apply_theme(idx);
            }
            MenuAction::SetAnimTheme(idx) => {
                self.apply_anim_theme(idx);
            }
            MenuAction::DeleteSession(id) => {
                let title = self
                    .sessions
                    .iter()
                    .find(|s| s.id.to_string() == *id)
                    .map(|s| truncate_chars(&s.title.clone(), 30))
                    .unwrap_or_else(|| id.chars().take(8).collect());
                self.open_menu(Menu::ConfirmDelete {
                    label: format!("delete session '{title}'?"),
                    action: MenuAction::DeleteSession(id),
                });
            }
            MenuAction::UseModel(k) => {
                self.switch_model(&k);
                self.menu_home();
            }
            MenuAction::Confirm(inner) => {
                let inner = *inner;
                if let MenuAction::DeleteProvider(p) = &inner {
                    let removed: Vec<String> = self
                        .cfg
                        .models
                        .iter()
                        .filter(|(_, m)| &m.provider == p)
                        .map(|(k, _)| k.clone())
                        .collect();
                    for k in &removed {
                        self.cfg.models.remove(k);
                    }
                    self.cfg.providers.remove(p);
                    if self.model_cfg.provider == *p {
                        self.status(
                            "active model removed — pick a new one (/providers)",
                            StatusKind::Warn,
                        );
                    }
                }
                if let MenuAction::DeleteModel(_, k) = &inner {
                    self.cfg.models.remove(k);
                    if self.session.model_key == *k {
                        self.status(
                            "active model removed — pick a new one (/providers)",
                            StatusKind::Warn,
                        );
                    }
                }
                if let MenuAction::DeleteSession(id) = &inner {
                    match Session::delete(id) {
                        Ok(()) => {
                            self.sessions.retain(|s| s.id.to_string() != *id);
                            self.status("session deleted", StatusKind::Ok);
                        }
                        Err(e) => self.status(&format!("delete session: {e:#}"), StatusKind::Err),
                    }
                }
                self.cfg.save().ok();
                match &inner {
                    // provider gone: land on a fresh providers list
                    MenuAction::DeleteProvider(_) => {
                        self.menu_home();
                        self.open_menu(Menu::Providers);
                    }
                    // model gone: back to its provider's model list
                    MenuAction::DeleteModel(p, _) => {
                        self.menu_stack.pop();
                        self.open_menu(Menu::Models {
                            provider: p.clone(),
                        });
                    }
                    // session gone: back to the sessions list
                    MenuAction::DeleteSession(_) => {
                        self.menu_stack.pop();
                        self.open_menu(Menu::Sessions);
                    }
                    _ => {}
                }
            }
            MenuAction::OpenSubagent(id) => {
                self.menu_home();
                self.active_subagent = Some(id);
                self.follow = true;
                self.view_top = 0;
                self.dirty = true;
            }
            MenuAction::SetThinking(level) => {
                self.model_cfg.thinking = level;
                if let Some(m) = self.cfg.models.get_mut(&self.session.model_key) {
                    m.thinking = level;
                }
                self.cfg.save().ok();
                self.menu_home();
                self.status(&format!("thinking: {}", level.as_str()), StatusKind::Ok);
            }
            MenuAction::AskSelect(label) => {
                self.ask_answer(label);
            }
            MenuAction::AskToggle(idx) => {
                if let Some(v) = self.ask_picked.get_mut(idx) {
                    *v = !*v;
                    self.dirty = true;
                }
            }
            MenuAction::AskConfirm => {
                // send the toggled labels (in option order), or a note if none
                let picked = match self.cur_menu() {
                    Some(Menu::AskUser { options, .. }) => options
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| self.ask_picked.get(*i).copied().unwrap_or(false))
                        .map(|(_, (l, _))| l.clone())
                        .collect::<Vec<_>>(),
                    _ => vec![],
                };
                if picked.is_empty() {
                    self.ask_answer(String::new());
                } else {
                    self.ask_answer(picked.join("; "));
                }
            }
            MenuAction::AskFree => {
                let id = match self.cur_menu() {
                    Some(Menu::AskUser { id, .. }) => *id,
                    _ => return,
                };
                self.open_menu(Menu::AskFree { id });
            }
        }
        self.dirty = true;
    }

    pub(super) fn menu_title(&self) -> String {
        match &self.cur_menu() {
            Some(Menu::Settings) => " settings ".into(),
            Some(Menu::Appearance) => " appearance ".into(),
            Some(Menu::Mcp) => " mcp servers ".into(),
            Some(Menu::Lsp) => " lsp servers ".into(),
            Some(Menu::Skills) => " skills ".into(),
            Some(Menu::Providers) => " providers (enter: models) ".into(),
            Some(Menu::Models { provider }) => format!(" {provider} models "),
            Some(Menu::PickModel { provider }) => format!(" switch model: {provider} "),
            Some(Menu::DeleteModelList { provider }) => format!(" delete model @ {provider} "),
            Some(Menu::EditProvider { name }) => match name {
                Some(n) => format!(" edit provider: {n} (enter: save, esc: cancel) "),
                None => " new provider (enter: save, esc: cancel) ".into(),
            },
            Some(Menu::EditModel { provider, .. }) => {
                format!(" model @ {provider} (enter: save, esc: cancel) ")
            }
            Some(Menu::Sessions) => {
                if self.sessions_filter.is_empty() {
                    " sessions ".into()
                } else {
                    format!(" sessions · filter '{}' ", self.sessions_filter)
                }
            }
            Some(Menu::DeleteSessions) => " delete session ".into(),
            Some(Menu::ForkPoint) => " fork this session ".into(),
            Some(Menu::Debug) => " debug ".into(),
            Some(Menu::Themes) => " themes ".into(),
            Some(Menu::EditSessionTitle { .. }) => " rename session ".into(),
            Some(Menu::ConfirmDelete { .. }) => " confirm ".into(),
            Some(Menu::Thinking) => " thinking ".into(),
            Some(Menu::AskUser { multiple, .. }) => {
                if *multiple {
                    " ask · multiple (enter toggles, confirm to finish) ".into()
                } else {
                    " ask ".into()
                }
            }
            Some(Menu::Approval { .. }) => " confirm command ".into(),
            Some(Menu::AskFree { .. }) => " type your answer (enter: send, esc: cancel) ".into(),
            Some(Menu::Todo) => " to-do ".into(),
            Some(Menu::Subagents) => " subagents ".into(),
            None => String::new(),
        }
    }

    pub(super) fn build_menu_rows(&mut self) {
        self.menu_rows.clear();
        self.menu_footer_text = None;
        let Some(menu) = self.cur_menu().cloned() else {
            return;
        };
        let row = |l: Line<'static>, a: MenuAction| (l, a);
        match menu {
            Menu::Settings => {
                let section = |label: &str, detail: &str, action: MenuAction| {
                    row(
                        Line::from(vec![
                            Span::styled(format!("  {label:<16}"), Theme::accent_bold()),
                            Span::styled(detail.to_string(), Theme::dim()),
                        ]),
                        action,
                    )
                };
                self.menu_rows.push(section(
                    "Appearance",
                    "themes and UI",
                    MenuAction::OpenAppearance,
                ));
                self.menu_rows.push(section(
                    "Providers",
                    "models and API providers",
                    MenuAction::OpenProviders,
                ));
                self.menu_rows
                    .push(section("MCP", "tool servers", MenuAction::OpenMcp));
                self.menu_rows
                    .push(section("LSP", "language diagnostics", MenuAction::OpenLsp));
                self.menu_rows.push(section(
                    "Skills",
                    "agent instructions",
                    MenuAction::OpenSkills,
                ));
                self.menu_footer_text = Some("enter: open · esc: close".into());
            }
            Menu::Mcp => {
                self.menu_rows
                    .push(row(Line::from("  MCP servers"), MenuAction::None));
                if self.cfg.mcp.servers.is_empty() {
                    self.menu_rows
                        .push(row(Line::from("  no servers configured"), MenuAction::None));
                } else {
                    for server in &self.cfg.mcp.servers {
                        self.menu_rows.push(row(
                            Line::from(format!(
                                "  {:<20} {}",
                                server.name,
                                if server.enabled {
                                    "enabled"
                                } else {
                                    "disabled"
                                }
                            )),
                            MenuAction::None,
                        ));
                    }
                }
                self.menu_footer_text = Some("edit config.toml · esc: back".into());
            }
            Menu::Lsp => {
                self.menu_rows
                    .push(row(Line::from("  LSP servers"), MenuAction::None));
                if self.cfg.lsp.servers.is_empty() {
                    self.menu_rows
                        .push(row(Line::from("  no servers configured"), MenuAction::None));
                } else {
                    for server in &self.cfg.lsp.servers {
                        self.menu_rows.push(row(
                            Line::from(format!(
                                "  {:<20} {}",
                                server.name,
                                if server.enabled {
                                    "enabled"
                                } else {
                                    "disabled"
                                }
                            )),
                            MenuAction::None,
                        ));
                    }
                }
                self.menu_footer_text = Some("edit config.toml · esc: back".into());
            }
            Menu::Skills => {
                let root = std::env::current_dir().unwrap_or_default();
                let loaded = crate::prompts::skills::load(&self.cfg.skills, &root);
                self.menu_rows
                    .push(row(Line::from("  Loaded skills"), MenuAction::None));
                if loaded.is_empty() {
                    self.menu_rows
                        .push(row(Line::from("  no skills found"), MenuAction::None));
                } else {
                    for skill in loaded {
                        self.menu_rows.push(row(
                            Line::from(format!("  {}", skill.name)),
                            MenuAction::None,
                        ));
                    }
                }
                self.menu_footer_text = Some("/skill to activate · esc: back".into());
            }

            Menu::Appearance => {
                let setting = |label: &str, detail: &str, action: MenuAction| {
                    row(
                        Line::from(vec![
                            Span::styled(format!("  {label:<16}"), Theme::accent_bold()),
                            Span::styled(detail.to_string(), Theme::dim()),
                        ]),
                        action,
                    )
                };
                self.menu_rows.push(setting(
                    "Themes",
                    "open theme picker",
                    MenuAction::OpenThemes,
                ));
                self.menu_rows.push(setting(
                    "Typewriter",
                    on_off(self.cfg.ui.typewriter).as_str(),
                    MenuAction::ToggleTypewriter,
                ));
                self.menu_rows.push(setting(
                    "Show cost",
                    on_off(self.cfg.ui.show_cost).as_str(),
                    MenuAction::ToggleShowCost,
                ));
                self.menu_footer_text = Some("enter: open/toggle · esc: back".into());
            }
            Menu::Themes => {
                // when an animated theme is active, the static list shows no
                // marker — only one "*" is ever visible across both lists
                let cur = if crate::tui::theme::anim_theme_index().is_some() {
                    usize::MAX
                } else {
                    crate::tui::theme::theme_index()
                };
                for (i, t) in crate::tui::theme::THEMES.iter().enumerate() {
                    let mark = if i == cur { " *" } else { "" };
                    // the name glows in its own accent color (no swatch square)
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(
                            format!("  {}{mark}", t.name),
                            Style::new()
                                .fg(t.p.accent)
                                .bg(Theme::BG())
                                .add_modifier(Modifier::BOLD),
                        )]),
                        MenuAction::SetTheme(i),
                    ));
                }
                // animated themes flow right after the static ones, same 2-block swatch
                let cur_anim = crate::tui::theme::anim_theme_index();
                let tick = crate::tui::theme::anim_tick();
                for (i, t) in crate::tui::theme::ANIMATED_THEMES.iter().enumerate() {
                    let mark = if cur_anim == Some(i) { " *" } else { "" };
                    let p0 = crate::tui::theme::anim_palette_at(i, tick);
                    // the name glows in the live animated accent (no swatch square)
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(
                            format!("  {}{mark}", t.name),
                            Style::new()
                                .fg(p0.accent)
                                .bg(Theme::BG())
                                .add_modifier(Modifier::BOLD),
                        )]),
                        MenuAction::SetAnimTheme(i),
                    ));
                }
                self.menu_footer_text =
                    Some("enter: apply · selection stays open · esc: close".into());
            }
            Menu::Debug => {
                let setting = |l: &str, val: String, a: MenuAction| {
                    row(
                        Line::from(vec![
                            Span::styled(format!(" {l:<18}"), Theme::dim()),
                            Span::styled(val, Theme::accent()),
                        ]),
                        a,
                    )
                };
                let info = |l: &str, v: &str| {
                    row(
                        Line::from(vec![
                            Span::styled(format!(" {l:<18}"), Theme::dim()),
                            Span::styled(v.to_string(), Theme::base()),
                        ]),
                        MenuAction::None,
                    )
                };
                let log_path = crate::config::data_dir()
                    .map(|d| d.join("debug.log").display().to_string())
                    .unwrap_or_else(|_| "debug.log".into());
                let cfg_dir = crate::config::config_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                self.menu_rows.push(setting(
                    "typewriter",
                    on_off(self.cfg.ui.typewriter),
                    MenuAction::ToggleTypewriter,
                ));
                self.menu_rows.push(setting(
                    "http debug log",
                    on_off(self.cfg.ui.http_log),
                    MenuAction::ToggleHttpLog,
                ));
                self.menu_rows.push(setting(
                    "thinking",
                    self.model_cfg.thinking.as_str().to_string(),
                    MenuAction::CycleModelThinking,
                ));
                self.menu_rows.push(setting(
                    "default thinking",
                    self.cfg.default_thinking.as_str().to_string(),
                    MenuAction::CycleDefaultThinking,
                ));
                self.menu_rows.push(setting(
                    "mode",
                    self.mode.label().to_string(),
                    MenuAction::ToggleMode,
                ));
                self.menu_rows.push(setting(
                    "sessions",
                    format!("{} saved", self.sessions.len()),
                    MenuAction::OpenSessions,
                ));
                self.menu_rows.push(info("model", &self.model_cfg.id));
                self.menu_rows
                    .push(info("provider", &self.model_cfg.provider));
                let key_state = self
                    .cfg
                    .providers
                    .get(&self.model_cfg.provider)
                    .and_then(|pc| {
                        pc.key_env_name(&self.model_cfg.provider)
                            .map(|n| format!("env ${n}"))
                            .or_else(|| {
                                pc.api_key
                                    .as_deref()
                                    .is_some_and(|k| !k.is_empty())
                                    .then(|| "set".to_string())
                            })
                    })
                    .unwrap_or_else(|| "none".into());
                self.menu_rows.push(info("api key", &key_state));
                self.menu_rows.push(info("log file", &log_path));
                self.menu_rows.push(info("config", &cfg_dir));
                self.menu_footer_text = Some("enter: toggle value · esc: done".into());
            }
            Menu::Sessions => {
                let q = self.sessions_filter.to_lowercase();
                let visible: Vec<&Session> = self
                    .sessions
                    .iter()
                    .filter(|s| {
                        q.is_empty()
                            || s.title.to_lowercase().contains(&q)
                            || s.model_key.to_lowercase().contains(&q)
                    })
                    .collect();
                if !self.startup {
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(" + new session", Theme::ACCENT_SOFT())]),
                        MenuAction::NewSession,
                    ));
                }
                let cur_id = self.session.id.to_string();
                let pinned: Vec<&Session> = visible.iter().filter(|s| s.pinned).copied().collect();
                if !pinned.is_empty() {
                    const FRAME_W: usize = 72;
                    let head = " pinned ";
                    let mid = {
                        let label = format!(" {head} ");
                        let fill = FRAME_W.saturating_sub(label.chars().count());
                        format!(
                            "{}{}{}",
                            "─".repeat(fill / 2),
                            label,
                            "─".repeat(fill - fill / 2)
                        )
                    };
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(format!("╭{mid}╮"), Theme::ACCENT_SOFT())]),
                        MenuAction::None,
                    ));
                    for s in &pinned {
                        self.menu_rows
                            .push(session_row(s, s.id.to_string() == cur_id, true));
                    }
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(
                            format!("╰{}╯", "─".repeat(FRAME_W)),
                            Theme::ACCENT_SOFT(),
                        )]),
                        MenuAction::None,
                    ));
                }
                for s in &visible {
                    if s.pinned {
                        continue;
                    }
                    self.menu_rows
                        .push(session_row(s, s.id.to_string() == cur_id, false));
                }
                if visible.is_empty() {
                    let note = if q.is_empty() {
                        " (no saved sessions yet)"
                    } else {
                        " (no matches)"
                    };
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(note.to_string(), Theme::dim())]),
                        MenuAction::None,
                    ));
                }
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        " · fork this session".to_string(),
                        Theme::FG(),
                    )]),
                    MenuAction::ForkSessionList,
                ));
                self.menu_footer_text = Some(if self.sessions_filter.is_empty() {
                    "enter: open · r: rename · p: pin · d: delete · type to filter".into()
                } else {
                    "type: filter · backspace: erase · esc: clear filter".into()
                });
            }
            Menu::DeleteSessions => {
                for (i, s) in self.sessions.iter().enumerate() {
                    let date = fmt_date(s.last_activity());
                    self.menu_rows.push(row(
                        Line::from(vec![
                            Span::styled(
                                format!(" {}", truncate_chars(&s.title, 40)),
                                Theme::ERR(),
                            ),
                            Span::styled(
                                format!("  {date} · {}", truncate_chars(&s.model_key, 16)),
                                Theme::dim(),
                            ),
                        ]),
                        MenuAction::DeleteSession(self.sessions[i].id.to_string()),
                    ));
                }
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" esc: back".to_string(), Theme::dim())]),
                    MenuAction::Back,
                ));
            }
            Menu::ForkPoint => {
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        format!(
                            " whole conversation ({} messages)",
                            self.session.messages.len()
                        ),
                        Theme::ACCENT_SOFT(),
                    )]),
                    MenuAction::ForkAt(usize::MAX),
                ));
                for (i, m) in self.session.messages.iter().enumerate() {
                    let who = match m.role {
                        Role::User => "You",
                        Role::Assistant => "Agent",
                        Role::System | Role::Tool => continue,
                    };
                    self.menu_rows.push(row(
                        Line::from(vec![
                            Span::styled(format!(" {i:>3}. {who}: "), Theme::accent()),
                            Span::styled(
                                truncate_chars(m.content.lines().next().unwrap_or(""), 44),
                                Theme::base(),
                            ),
                        ]),
                        MenuAction::ForkAt(i),
                    ));
                }
                self.menu_footer_text = Some("enter: fork from here · esc: cancel".into());
            }
            Menu::Providers => {
                for (name, pc) in &self.cfg.providers {
                    let models = self
                        .cfg
                        .models
                        .values()
                        .filter(|m| &m.provider == name)
                        .count();
                    let key_state = if pc.api_key.as_deref().is_some_and(|k| !k.is_empty()) {
                        "key set".to_string()
                    } else if let Some(env) = pc.key_env_name(name) {
                        format!("key ${env}")
                    } else {
                        "no key".to_string()
                    };
                    self.menu_rows.push(row(
                        Line::from(vec![
                            Span::styled(format!(" {name}"), Theme::accent()),
                            Span::styled(
                                format!("  {} · {models} models · {key_state}", pc.base_url),
                                Theme::dim(),
                            ),
                        ]),
                        MenuAction::OpenModels(name.clone()),
                    ));
                }
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" + add provider", Theme::ACCENT_SOFT())]),
                    MenuAction::AddProvider,
                ));
            }
            Menu::Models { provider } => {
                for (k, m) in &self.cfg.models {
                    if m.provider == provider {
                        let current = k == &self.session.model_key;
                        let mark = if current { " *" } else { "" };
                        self.menu_rows.push(row(
                            Line::from(vec![
                                Span::styled(format!(" {k}{mark}"), Theme::accent()),
                                Span::styled(
                                    format!(
                                        "  {} · ctx {} · th:{}",
                                        m.id,
                                        m.context,
                                        m.thinking.as_str()
                                    ),
                                    Theme::dim(),
                                ),
                            ]),
                            MenuAction::EditModel(provider.clone(), k.clone()),
                        ));
                    }
                }
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" + add model", Theme::ACCENT_SOFT())]),
                    MenuAction::AddModel(provider.clone()),
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" · switch active model", Theme::FG())]),
                    MenuAction::PickModelList(provider.clone()),
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" · edit provider", Theme::FG())]),
                    MenuAction::EditProvider(provider.clone()),
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" · delete model", Theme::ERR())]),
                    MenuAction::DeleteModelList(provider.clone()),
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" · delete provider", Theme::ERR())]),
                    MenuAction::DeleteProvider(provider.clone()),
                ));
            }
            Menu::PickModel { provider } => {
                for (k, m) in &self.cfg.models {
                    if m.provider == provider {
                        let current = k == &self.session.model_key;
                        let mark = if current { " *current" } else { "" };
                        self.menu_rows.push(row(
                            Line::from(vec![
                                Span::styled(format!(" {k}"), Theme::accent()),
                                Span::styled(format!("  {}{mark}", m.id), Theme::dim()),
                            ]),
                            MenuAction::UseModel(k.clone()),
                        ));
                    }
                }
            }
            Menu::DeleteModelList { provider } => {
                for (k, m) in &self.cfg.models {
                    if m.provider == provider {
                        self.menu_rows.push(row(
                            Line::from(vec![
                                Span::styled(format!(" {k}"), Theme::ERR()),
                                Span::styled(format!("  {}", m.id), Theme::dim()),
                            ]),
                            MenuAction::DeleteModel(provider.clone(), k.clone()),
                        ));
                    }
                }
            }
            Menu::ConfirmDelete { label, action } => {
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(format!(" {label}"), Theme::WARN())]),
                    MenuAction::None,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" enter: confirm delete", Theme::ERR())]),
                    MenuAction::Confirm(Box::new(action)),
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" esc: cancel", Theme::dim())]),
                    MenuAction::Back,
                ));
            }
            Menu::Thinking => {
                for lvl in ThinkingLevel::SELECTABLE {
                    let current = lvl == self.model_cfg.thinking;
                    let mark = if current { " *current" } else { "" };
                    self.menu_rows.push(row(
                        Line::from(vec![
                            Span::styled(format!(" {}", lvl.as_str()), Theme::accent()),
                            Span::styled(mark.to_string(), Theme::dim()),
                        ]),
                        MenuAction::SetThinking(lvl),
                    ));
                }
            }
            Menu::AskUser {
                question,
                options,
                multiple,
                allow_free,
                ..
            } => {
                if !options.is_empty() {
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(
                            format!(" {question}"),
                            Theme::accent_bold(),
                        )]),
                        MenuAction::None,
                    ));
                }
                for (i, (label, desc)) in options.iter().enumerate() {
                    let checked = if multiple {
                        if self.ask_picked.get(i).copied().unwrap_or(false) {
                            " [x] "
                        } else {
                            " [ ] "
                        }
                    } else {
                        " "
                    };
                    let mut spans = vec![
                        Span::styled(format!("{checked}{}. ", i + 1), Theme::accent()),
                        Span::styled(format!("{label}"), Theme::base()),
                    ];
                    if let Some(d) = desc {
                        spans.push(Span::styled(format!(" — {d}"), Theme::dim()));
                    }
                    self.menu_rows.push(row(
                        Line::from(spans),
                        if multiple {
                            MenuAction::AskToggle(i)
                        } else {
                            MenuAction::AskSelect(label.clone())
                        },
                    ));
                }
                if multiple {
                    let n = self.ask_picked.iter().filter(|v| **v).count();
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(
                            format!(" confirm ({n} selected)",),
                            Theme::ACCENT_SOFT(),
                        )]),
                        MenuAction::AskConfirm,
                    ));
                }
                if allow_free {
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(
                            " type a custom answer".to_string(),
                            Theme::FG(),
                        )]),
                        MenuAction::AskFree,
                    ));
                }
                self.menu_footer_text = Some(if multiple {
                    "enter: toggle · confirm row: send · esc: skip".into()
                } else {
                    "enter: choose · esc: skip".into()
                });
            }
            Menu::Approval {
                command, reason, ..
            } => {
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        " The agent wants to run:",
                        Theme::WARN(),
                    )]),
                    MenuAction::None,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(format!("  {command}"), Theme::base())]),
                    MenuAction::None,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        format!("  reason: {reason}"),
                        Theme::dim(),
                    )]),
                    MenuAction::None,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        " enter: run once".to_string(),
                        Theme::ACCENT_SOFT(),
                    )]),
                    MenuAction::None,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        " a: always allow this session".to_string(),
                        Theme::ACCENT_SOFT(),
                    )]),
                    MenuAction::None,
                ));
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(" d: deny".to_string(), Theme::ERR())]),
                    MenuAction::None,
                ));
            }
            Menu::EditProvider { .. }
            | Menu::EditModel { .. }
            | Menu::EditSessionTitle { .. }
            | Menu::AskFree { .. } => {}
            Menu::Subagents => {
                if self.subagents.is_empty() {
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled("  no subagents yet", Theme::dim())]),
                        MenuAction::None,
                    ));
                } else {
                    for (id, task, status, _, _) in &self.subagents {
                        let style = match status.as_str() {
                            "completed" => Theme::ok(),
                            "failed" => Theme::err(),
                            _ => Theme::accent(),
                        };
                        self.menu_rows.push(row(
                            Line::from(vec![
                                Span::styled(format!(" subagent-{id:<3}"), style),
                                Span::styled(format!(" {status:<10} "), Theme::dim()),
                                Span::styled(task.clone(), Theme::base()),
                            ]),
                            MenuAction::OpenSubagent(*id),
                        ));
                    }
                }
            }
            Menu::Todo => {
                self.menu_rows.push(row(
                    Line::from(vec![Span::styled(
                        " agent to-do list",
                        Theme::accent_bold(),
                    )]),
                    MenuAction::None,
                ));
                if self.todos.is_empty() {
                    self.menu_rows.push(row(
                        Line::from(vec![Span::styled(
                            "  (no active plan steps yet)",
                            Theme::dim(),
                        )]),
                        MenuAction::None,
                    ));
                } else {
                    for (i, item) in self.todos.iter().enumerate() {
                        self.menu_rows.push(row(
                            Line::from(vec![
                                Span::styled(format!("  {}. ", i + 1), Theme::accent()),
                                Span::styled(item.clone(), Theme::base()),
                            ]),
                            MenuAction::None,
                        ));
                    }
                }
                self.menu_footer_text = Some("esc: close · ctrl+t: toggle".into());
            }
        }
        if self.menu_sel >= self.menu_rows.len() {
            self.menu_sel = self.menu_rows.len().saturating_sub(1);
        }
    }
}

fn session_row(s: &Session, is_current: bool, framed: bool) -> (Line<'static>, MenuAction) {
    const FRAME_CONTENT: usize = 72;
    let badge = if s.forked_from_id.is_some() {
        "[fork] "
    } else {
        ""
    };
    let mark = if is_current { " *" } else { "" };
    let left = format!(" {badge}{}{mark}", truncate_chars(&s.title, 28));
    let mut dim = format!(
        "{} · {} · {} tok",
        fmt_date(s.last_activity()),
        truncate_chars(&s.model_key, 14),
        fmt_k(s.context_tokens_used())
    );
    if let Some(parent) = &s.forked_from_title {
        dim.push_str(&format!(" · from '{}'", truncate_chars(parent, 20)));
    }
    let action = MenuAction::OpenSession(s.id.to_string());
    if !framed {
        return (
            Line::from(vec![
                Span::styled(left, Theme::accent()),
                Span::styled(format!("  {dim}"), Theme::dim()),
            ]),
            action,
        );
    }
    let text_len = left.chars().count() + 2 + dim.chars().count();
    let pad = FRAME_CONTENT.saturating_sub(text_len);
    (
        Line::from(vec![
            Span::styled("│".to_string(), Theme::ACCENT_SOFT()),
            Span::styled(left, Theme::accent()),
            Span::styled(format!("  {dim}"), Theme::dim()),
            Span::styled(" ".repeat(pad), Theme::base()),
            Span::styled("│".to_string(), Theme::ACCENT_SOFT()),
        ]),
        action,
    )
}
