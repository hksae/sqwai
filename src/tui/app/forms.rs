#![allow(unused_imports)]
use super::events::handle_text_combo;
use super::*;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tui_textarea::TextArea;

use crate::agent::loop_task::AgentEvent;
use crate::config::{Config, EffortControl, EffortLevel, ModelConfig, WireFormat};
use crate::providers::{self, ChatRequest, Message as PMessage, Role, SharedProvider};
use crate::session::Session;
use crate::tui::markdown::{Highlighter, render, wrap_tagged};
use crate::tui::theme::Theme;

const FORMAT_OPTS: &[&str] = &["openai", "anthropic", "responses"];
/// Per-model effort options: always the full level list — a hardcoded copy
/// here once dropped `xhigh`, showing `off` for xhigh models and clobbering
/// the level on save.
const EFFORT_OPTS: &[&str] = &EffortLevel::STRS;
/// Per-model effort-control options: `auto` (derive from the wire format)
///
/// leads, then every declared control in cycle order (see
/// `CycleEffortControl`). `ALWAYS_OPTS` is the on/off switch beside it.
/// The `[1..]` tail must stay equal to `EffortControl::STRS` — pinned by
/// `edit_model_form_options_match_control_cycle` — so the form can never
/// lag behind a new control variant again.
pub(super) const EFFORT_CONTROL_OPTS: &[&str] = &[
    "auto",
    EffortControl::STRS[0],
    EffortControl::STRS[1],
    EffortControl::STRS[2],
    EffortControl::STRS[3],
];
pub(super) const ALWAYS_OPTS: &[&str] = &["off", "on"];
const MCP_TRANSPORT_OPTS: &[&str] = &["stdio", "http"];

/// "K=V,K=V" <-> env map rendering for server forms.
fn join_env(env: &std::collections::BTreeMap<String, String>) -> String {
    env.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_env(raw: &str) -> std::collections::BTreeMap<String, String> {
    raw.split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

pub(super) enum FormField {
    /// free text edited through a real textarea (cursor, word jumps, paste)
    Text {
        label: String,
        ta: Box<TextArea<'static>>,
    },
    /// pick-one value cycled with left/right
    Choice {
        label: String,
        options: &'static [&'static str],
        sel: usize,
    },
}

impl FormField {
    pub(super) fn text(label: &str, value: String) -> Self {
        let mut ta = Box::new(TextArea::new(vec![value]));
        ta.set_style(Theme::base());
        ta.set_cursor_line_style(Style::new());
        ta.set_cursor_style(Style::new().bg(ratatui::style::Color::White).fg(ratatui::style::Color::Black));
        ta.set_selection_style(Style::new().bg(ratatui::style::Color::Cyan).fg(ratatui::style::Color::Black));
        Self::Text {
            label: label.into(),
            ta,
        }
    }

    pub(super) fn choice(label: &str, options: &'static [&'static str], sel: usize) -> Self {
        Self::Choice {
            label: label.into(),
            options,
            sel,
        }
    }

    pub(super) fn label(&self) -> &str {
        match self {
            Self::Text { label, .. } | Self::Choice { label, .. } => label,
        }
    }

    pub(super) fn current_value(&self) -> String {
        match self {
            // fields are single-line; guard against pasted newlines anyway
            Self::Text { ta, .. } => ta.lines().join(" "),
            Self::Choice { options, sel, .. } => {
                options.get(*sel).copied().unwrap_or("").to_string()
            }
        }
    }

    pub(super) fn trimmed(&self) -> String {
        self.current_value().trim().to_string()
    }
}

impl App {
    pub(super) fn prefill_form(&mut self) {
        self.form_fields.clear();
        match self.cur_menu() {
            Some(Menu::EditProvider { name }) => {
                let is_builtin = name
                    .as_ref()
                    .is_some_and(|n| self.cfg.is_builtin_provider(n));
                match name.as_ref().and_then(|n| self.cfg.providers.get(n)) {
                    Some(pc) => {
                        if is_builtin {
                            self.form_fields = vec![
                                FormField::text("api key", pc.api_key.clone().unwrap_or_default()),
                                FormField::text(
                                    "key env var",
                                    pc.api_key_env.clone().unwrap_or_default(),
                                ),
                            ];
                        } else {
                            let fmt_sel = FORMAT_OPTS
                                .iter()
                                .position(|s| *s == pc.format.as_str())
                                .unwrap_or(0);
                            self.form_fields = vec![
                                FormField::text("name", name.clone().unwrap_or_default()),
                                FormField::choice("format", FORMAT_OPTS, fmt_sel),
                                FormField::text("base url", pc.base_url.clone()),
                                FormField::text("api key", pc.api_key.clone().unwrap_or_default()),
                                FormField::text(
                                    "key env var",
                                    pc.api_key_env.clone().unwrap_or_default(),
                                ),
                            ];
                        }
                    }
                    _ => {
                        self.form_fields = vec![
                            FormField::text("name", String::new()),
                            FormField::choice("format", FORMAT_OPTS, 0),
                            FormField::text("base url", String::new()),
                            FormField::text("api key", String::new()),
                            FormField::text("key env var", String::new()),
                        ];
                    }
                }
            }
            Some(Menu::EditModel { key, .. }) => {
                match key.as_ref().and_then(|k| self.cfg.models.get(k)) {
                    Some(mc) => {
                        let ef_sel = EFFORT_OPTS
                            .iter()
                            .position(|s| *s == mc.effort.as_str())
                            .unwrap_or(0);
                        let ctl_sel = match mc.effort_control {
                            None => 0,
                            Some(c) => EffortControl::ALL
                                .iter()
                                .position(|x| *x == c)
                                .map(|i| i + 1)
                                .unwrap_or(0),
                        };
                        self.form_fields = vec![
                            FormField::text("key", key.clone().unwrap_or_default()),
                            FormField::text("request id", mc.id.clone()),
                            FormField::text("context", mc.context.to_string()),
                            FormField::choice("effort", EFFORT_OPTS, ef_sel),
                            FormField::choice("effort control", EFFORT_CONTROL_OPTS, ctl_sel),
                            FormField::choice(
                                "effort always on",
                                ALWAYS_OPTS,
                                usize::from(mc.effort_always_on),
                            ),
                        ];
                    }
                    _ => {
                        let ef_sel = EFFORT_OPTS
                            .iter()
                            .position(|s| *s == self.cfg.default_effort.as_str())
                            .unwrap_or(0);
                        self.form_fields = vec![
                            FormField::text("key", String::new()),
                            FormField::text("request id", String::new()),
                            FormField::text("context", "128000".into()),
                            FormField::choice("effort", EFFORT_OPTS, ef_sel),
                            FormField::choice("effort control", EFFORT_CONTROL_OPTS, 0),
                            FormField::choice("effort always on", ALWAYS_OPTS, 0),
                        ];
                    }
                }
            }
            Some(Menu::EditSessionTitle { id }) => {
                let title = self
                    .sessions
                    .iter()
                    .find(|s| s.id.to_string() == *id)
                    .map(|s| s.title.clone())
                    .unwrap_or_default();
                self.form_fields = vec![FormField::text("title", title)];
            }
            Some(Menu::EditScalar(setting)) => {
                self.form_fields =
                    vec![FormField::text(setting.label(), setting.current(&self.cfg))];
            }
            Some(Menu::AddListItem(section)) => {
                self.form_fields = vec![FormField::text(section.title(), String::new())];
            }
            Some(Menu::EditMcpServer { index }) => {
                match index.and_then(|i| self.cfg.mcp.servers.get(i)) {
                    Some(server) => {
                        let (type_sel, endpoint, args, env) = match &server.transport {
                            crate::config::McpTransport::Stdio { command, args, env } => {
                                (0, command.clone(), args.join(" "), join_env(env))
                            }
                            crate::config::McpTransport::Http { url, headers } => {
                                (1, url.clone(), String::new(), join_env(headers))
                            }
                        };
                        self.form_fields = vec![
                            FormField::text("name", server.name.clone()),
                            FormField::choice("type", MCP_TRANSPORT_OPTS, type_sel),
                            FormField::text("endpoint", endpoint),
                            FormField::text("args", args),
                            FormField::text("env", env),
                        ];
                    }
                    None => {
                        self.form_fields = vec![
                            FormField::text("name", String::new()),
                            FormField::choice("type", MCP_TRANSPORT_OPTS, 0),
                            FormField::text("endpoint", String::new()),
                            FormField::text("args", String::new()),
                            FormField::text("env", String::new()),
                        ];
                    }
                }
            }
            Some(Menu::EditLspServer { index }) => {
                match index.and_then(|i| self.cfg.lsp.servers.get(i)) {
                    Some(server) => {
                        self.form_fields = vec![
                            FormField::text("name", server.name.clone()),
                            FormField::text("language", server.language.clone()),
                            FormField::text("command", server.command.clone()),
                            FormField::text("args", server.args.join(" ")),
                            FormField::text("root markers", server.root_markers.join(",")),
                        ];
                    }
                    None => {
                        self.form_fields = vec![
                            FormField::text("name", String::new()),
                            FormField::text("language", String::new()),
                            FormField::text("command", String::new()),
                            FormField::text("args", String::new()),
                            FormField::text("root markers", String::new()),
                        ];
                    }
                }
            }
            Some(Menu::AskFree { .. }) => {
                self.form_fields = vec![FormField::text("answer", String::new())];
            }
            _ => {}
        }
    }

    pub(super) fn focused_is_choice(&self) -> bool {
        matches!(
            self.form_fields.get(self.form_focus),
            Some(FormField::Choice { .. })
        )
    }

    /// Label column width for the open form: fits the longest field label
    /// (`" {label:>w$} : "`), never narrower than the legacy 16 columns so
    /// short-label forms render exactly as before.
    pub(super) fn form_label_w(&self) -> u16 {
        let widest = self
            .form_fields
            .iter()
            .map(|f| f.label().chars().count())
            .max()
            .unwrap_or(0);
        (widest as u16 + 4).max(16)
    }

    pub(super) fn form_to_start(&mut self) {
        if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(self.form_focus) {
            ta.move_cursor(tui_textarea::CursorMove::Head);
        }
    }

    pub(super) fn form_to_end(&mut self) {
        if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(self.form_focus) {
            ta.move_cursor(tui_textarea::CursorMove::End);
        }
    }

    /// left/right/home/end: choices cycle their values, text fields move the cursor
    pub(super) fn form_nav_key(&mut self, k: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        if self.focused_is_choice() {
            match k.code {
                KeyCode::Left => self.choice_cycle(-1),
                KeyCode::Right => self.choice_cycle(1),
                _ => {}
            }
            return;
        }
        if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(self.form_focus) {
            ta.input(k);
            self.dirty = true;
        }
    }

    /// printable chars / backspace / delete (+ ctrl combos like ctrl+z, ctrl+w):
    /// choices ignore them, text fields forward to the textarea
    pub(super) fn form_edit_key(&mut self, k: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        if k.code == KeyCode::Enter {
            return; // fields stay single-line; enter saves via menu_activate
        }
        if self.focused_is_choice() {
            return;
        }
        if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(self.form_focus) {
            if handle_text_combo(ta, k) {
                self.dirty = true;
                return;
            }
            ta.input(k);
            self.dirty = true;
        }
    }

    pub(super) fn choice_cycle(&mut self, dir: i32) {
        if let Some(FormField::Choice { options, sel, .. }) =
            self.form_fields.get_mut(self.form_focus)
        {
            let n = options.len();
            *sel = if dir < 0 {
                (*sel + n - 1) % n
            } else {
                (*sel + 1) % n
            };
            self.dirty = true;
        }
    }

    pub(super) fn form_is_selecting(&self) -> bool {
        match self.form_fields.get(self.form_focus) {
            Some(FormField::Text { ta, .. }) => ta.is_selecting(),
            _ => false,
        }
    }

    pub(super) fn form_mouse_down(&mut self, row: u16, col: u16) {
        let top_y = self.menu_rect.y + 1;
        if row >= top_y {
            let idx = (row - top_y) as usize;
            if idx < self.form_fields.len() {
                if self.form_focus != idx {
                    if let Some(FormField::Text { ta, .. }) =
                        self.form_fields.get_mut(self.form_focus)
                    {
                        ta.cancel_selection();
                    }
                    self.form_focus = idx;
                }
                let label_w = self.form_label_w();
                let text_x = self.menu_rect.x + 1 + label_w;
                match self.form_fields.get_mut(idx) {
                    Some(FormField::Text { ta, .. }) => {
                        if col >= text_x {
                            let char_col = (col.saturating_sub(text_x)) as usize;
                            ta.move_cursor(tui_textarea::CursorMove::Jump(0, char_col as u16));
                            ta.start_selection();
                        } else {
                            ta.cancel_selection();
                            ta.move_cursor(tui_textarea::CursorMove::End);
                        }
                    }
                    Some(FormField::Choice { .. }) if col >= text_x => {
                        self.choice_cycle(1);
                    }
                    _ => {}
                }
                self.dirty = true;
            }
        }
    }

    pub(super) fn form_mouse_drag(&mut self, _row: u16, col: u16) {
        let label_w = self.form_label_w();
        if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(self.form_focus)
            && ta.is_selecting()
        {
            let text_x = self.menu_rect.x + 1 + label_w;
            let char_col = (col.saturating_sub(text_x)) as usize;
            ta.move_cursor(tui_textarea::CursorMove::Jump(0, char_col as u16));
            self.dirty = true;
        }
    }

    pub(super) fn form_mouse_up(&mut self) {
        if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(self.form_focus)
            && ta.is_selecting()
        {
            ta.copy();
            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                let text = ta.yank_text();
                if !text.is_empty() {
                    let _ = clipboard.set_text(text);
                }
            }
        }
        self.dirty = true;
    }

    pub(super) fn form_save(&mut self) {
        match self.cur_menu().cloned() {
            Some(Menu::EditSessionTitle { id }) => {
                let t = self
                    .form_fields
                    .first()
                    .map(|f| f.trimmed())
                    .unwrap_or_default();
                if t.is_empty() {
                    self.status("title cannot be empty", StatusKind::Err);
                    return;
                }
                let title = truncate_chars(&t, 60);
                // headers hold menu state; the durable file is updated
                // through a full load/save round-trip
                let mut found = self.sessions.iter().any(|s| s.id.to_string() == id);
                if let Ok(mut s) = Session::load(&id) {
                    s.title = title.clone();
                    let _ = s.save();
                    found = true;
                }
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id.to_string() == id) {
                    s.title = title.clone();
                }
                // keep the open session's header in sync
                if self.session.id.to_string() == id {
                    self.session.title = title.clone();
                    let _ = self.session.save();
                }
                if found || self.session.id.to_string() == id {
                    self.open_menu_replace(Menu::Sessions);
                    self.status("session renamed", StatusKind::Ok);
                } else {
                    self.status("session not found", StatusKind::Err);
                }
            }
            Some(Menu::EditProvider { name }) => {
                let is_builtin = name
                    .as_ref()
                    .is_some_and(|n| self.cfg.is_builtin_provider(n));
                if is_builtin {
                    let provider_name = name.clone().unwrap();
                    let key = self
                        .form_fields
                        .first()
                        .map(|f| f.trimmed())
                        .unwrap_or_default();
                    let key_env = self
                        .form_fields
                        .get(1)
                        .map(|f| f.trimmed())
                        .unwrap_or_default();
                    if let Some(pc) = self.cfg.providers.get_mut(&provider_name) {
                        pc.api_key = (!key.is_empty()).then_some(key);
                        pc.api_key_env = (!key_env.is_empty()).then_some(key_env);
                    }
                    self.cfg.save().ok();
                    self.open_menu_replace(Menu::Models {
                        provider: provider_name,
                    });
                    return;
                }
                let vals: Vec<String> = self.form_fields.iter().map(|f| f.trimmed()).collect();
                let (new_name, fmt, url, key, key_env) = (
                    vals.first().cloned().unwrap_or_default(),
                    vals.get(1).cloned().unwrap_or_default(),
                    vals.get(2).cloned().unwrap_or_default(),
                    vals.get(3).cloned().unwrap_or_default(),
                    vals.get(4).cloned().unwrap_or_default(),
                );
                if new_name.is_empty() || url.is_empty() {
                    self.status("name and base url are required", StatusKind::Err);
                    return;
                }
                let format = FORMAT_OPTS
                    .iter()
                    .position(|f| *f == fmt)
                    .and_then(|i| WireFormat::ALL.get(i))
                    .copied()
                    .unwrap_or(WireFormat::Openai);
                // Uniqueness must hold against every other entry: on rename,
                // the name being edited is excluded, but a collision with a
                // different existing provider is a hard error — otherwise the
                // insert below would silently destroy that provider.
                if name.as_deref() != Some(new_name.as_str())
                    && self.cfg.providers.contains_key(&new_name)
                {
                    self.status(
                        &format!("provider '{new_name}' already exists"),
                        StatusKind::Err,
                    );
                    return;
                }
                let pc = crate::config::ProviderConfig {
                    format,
                    base_url: url,
                    api_key: (!key.is_empty()).then_some(key),
                    api_key_env: (!key_env.is_empty()).then_some(key_env),
                    // not shown in the form; preserve what the config says
                    continuation: name
                        .as_ref()
                        .and_then(|n| self.cfg.providers.get(n))
                        .or_else(|| self.cfg.providers.get(&new_name))
                        .is_none_or(|p| p.continuation),
                };
                if let Some(old) = &name
                    && old != &new_name
                {
                    if let Some(pc_old) = self.cfg.providers.remove(old) {
                        for m in self.cfg.models.values_mut() {
                            if &m.provider == old {
                                m.provider = new_name.clone();
                            }
                        }
                        let _ = pc_old;
                    }
                    if self.model_cfg.provider == *old {
                        self.model_cfg.provider = new_name.clone();
                    }
                }
                self.cfg.providers.insert(new_name.clone(), pc);
                self.cfg.save().ok();
                self.open_menu_replace(Menu::Models { provider: new_name });
            }
            Some(Menu::EditModel { provider, key }) => {
                let vals: Vec<String> = self.form_fields.iter().map(|f| f.trimmed()).collect();
                let (new_key, id, ctx, th, ctl, always) = (
                    vals.first().cloned().unwrap_or_default(),
                    vals.get(1).cloned().unwrap_or_default(),
                    vals.get(2).cloned().unwrap_or_default(),
                    vals.get(3).cloned().unwrap_or_default(),
                    vals.get(4).cloned().unwrap_or_default(),
                    vals.get(5).cloned().unwrap_or_default(),
                );
                if new_key.is_empty() || id.is_empty() {
                    self.status("key and request id are required", StatusKind::Err);
                    return;
                }
                let Ok(context) = ctx.parse::<u64>() else {
                    self.status("context must be a number", StatusKind::Err);
                    return;
                };
                let effort = EffortLevel::from_str(&th).unwrap_or(EffortLevel::Off);
                let effort_control = EffortControl::from_str(&ctl);
                let effort_always_on = always == "on";
                if key.as_deref() != Some(new_key.as_str())
                    && self.cfg.models.contains_key(&new_key)
                {
                    self.status(
                        &format!("model '{new_key}' already exists"),
                        StatusKind::Err,
                    );
                    return;
                }
                if let Some(old) = &key
                    && old != &new_key
                {
                    self.cfg.models.remove(old);
                    if self.session.model_key == *old {
                        self.session.model_key = new_key.clone();
                    }
                }
                // Fields the form does not show must survive an edit: before
                // this, editing a model silently reset its prices. Effort
                // control and always-on come from the form now.
                let previous = key
                    .as_ref()
                    .and_then(|k| self.cfg.models.get(k))
                    .cloned()
                    .or_else(|| self.cfg.models.get(&new_key).cloned());
                let updated = ModelConfig {
                    provider: provider.clone(),
                    id,
                    context,
                    effort,
                    effort_control,
                    effort_always_on,
                    price_in: previous.as_ref().and_then(|p| p.price_in),
                    price_out: previous.as_ref().and_then(|p| p.price_out),
                    fallback: previous.as_ref().and_then(|p| p.fallback.clone()),
                };
                self.cfg.models.insert(new_key.clone(), updated.clone());
                if self.session.model_key == new_key {
                    self.model_cfg = updated;
                    self.session.context_limit = context;
                    self.session.last_response_id = None;
                    self.session.last_response_model = None;
                    self.rebuild_provider();
                }
                self.cfg.save().ok();
                self.open_menu_replace(Menu::Models { provider });
            }
            Some(Menu::AskFree { .. }) => {
                let t = self
                    .form_fields
                    .first()
                    .map(|f| f.trimmed())
                    .unwrap_or_default();
                self.ask_answer(t);
            }
            Some(Menu::EditScalar(setting)) => {
                let raw = self
                    .form_fields
                    .first()
                    .map(|f| f.trimmed())
                    .unwrap_or_default();
                let message = setting.apply(&mut self.cfg, &raw);
                self.cfg.save().ok();
                self.status(&message, StatusKind::Ok);
                self.open_menu_replace(match setting {
                    super::menus::ScalarSetting::UndoKeepPerSession
                    | super::menus::ScalarSetting::UndoMaxTreeFiles
                    | super::menus::ScalarSetting::UndoBlobGraceSecs
                    | super::menus::ScalarSetting::UndoShadowMaxBytes => Menu::Undo,
                    _ => Menu::Agent,
                });
            }
            Some(Menu::AddListItem(section)) => {
                let value = self
                    .form_fields
                    .first()
                    .map(|f| f.trimmed())
                    .unwrap_or_default();
                if value.is_empty() {
                    self.status("value cannot be empty", StatusKind::Err);
                    return;
                }
                section.push(&mut self.cfg, value.clone());
                self.cfg.save().ok();
                self.status(
                    &format!("added to {}: {value}", section.title()),
                    StatusKind::Ok,
                );
                self.open_menu_replace(match section {
                    super::menus::ListSection::SkillsDirs => Menu::Skills,
                    _ => Menu::Safety,
                });
            }
            Some(Menu::EditMcpServer { index }) => {
                let vals: Vec<String> = self.form_fields.iter().map(|f| f.trimmed()).collect();
                let (name, kind, endpoint, args, env) = (
                    vals.first().cloned().unwrap_or_default(),
                    vals.get(1).cloned().unwrap_or_default(),
                    vals.get(2).cloned().unwrap_or_default(),
                    vals.get(3).cloned().unwrap_or_default(),
                    vals.get(4).cloned().unwrap_or_default(),
                );
                if name.is_empty() || endpoint.is_empty() {
                    self.status("name and endpoint are required", StatusKind::Err);
                    return;
                }
                if self
                    .cfg
                    .mcp
                    .servers
                    .iter()
                    .enumerate()
                    .any(|(i, s)| s.name == name && Some(i) != index)
                {
                    self.status(
                        &format!("MCP server '{name}' already exists"),
                        StatusKind::Err,
                    );
                    return;
                }
                let transport = if kind == "http" {
                    crate::config::McpTransport::Http {
                        url: endpoint,
                        headers: parse_env(&env),
                    }
                } else {
                    crate::config::McpTransport::Stdio {
                        command: endpoint,
                        args: args.split_whitespace().map(str::to_string).collect(),
                        env: parse_env(&env),
                    }
                };
                let enabled = index
                    .and_then(|i| self.cfg.mcp.servers.get(i))
                    .is_none_or(|s| s.enabled);
                let server = crate::config::McpServerDef {
                    name: name.clone(),
                    enabled,
                    transport,
                };
                match index {
                    Some(i) if i < self.cfg.mcp.servers.len() => {
                        self.cfg.mcp.servers[i] = server;
                    }
                    _ => self.cfg.mcp.servers.push(server),
                }
                self.cfg.save().ok();
                self.status(&format!("MCP server '{name}' saved"), StatusKind::Ok);
                self.open_menu_replace(Menu::Mcp);
            }
            Some(Menu::EditLspServer { index }) => {
                let vals: Vec<String> = self.form_fields.iter().map(|f| f.trimmed()).collect();
                let (name, language, command, args, markers) = (
                    vals.first().cloned().unwrap_or_default(),
                    vals.get(1).cloned().unwrap_or_default(),
                    vals.get(2).cloned().unwrap_or_default(),
                    vals.get(3).cloned().unwrap_or_default(),
                    vals.get(4).cloned().unwrap_or_default(),
                );
                if name.is_empty() || command.is_empty() {
                    self.status("name and command are required", StatusKind::Err);
                    return;
                }
                if self
                    .cfg
                    .lsp
                    .servers
                    .iter()
                    .enumerate()
                    .any(|(i, s)| s.name == name && Some(i) != index)
                {
                    self.status(
                        &format!("LSP server '{name}' already exists"),
                        StatusKind::Err,
                    );
                    return;
                }
                let enabled = index
                    .and_then(|i| self.cfg.lsp.servers.get(i))
                    .is_none_or(|s| s.enabled);
                let server = crate::config::LspServerDef {
                    name: name.clone(),
                    enabled,
                    language,
                    command,
                    args: args.split_whitespace().map(str::to_string).collect(),
                    root_markers: markers
                        .split(',')
                        .map(|m| m.trim().to_string())
                        .filter(|m| !m.is_empty())
                        .collect(),
                };
                match index {
                    Some(i) if i < self.cfg.lsp.servers.len() => {
                        self.cfg.lsp.servers[i] = server;
                    }
                    _ => self.cfg.lsp.servers.push(server),
                }
                self.cfg.save().ok();
                self.status(&format!("LSP server '{name}' saved"), StatusKind::Ok);
                self.open_menu_replace(Menu::Lsp);
            }
            _ => {}
        }
    }
}
