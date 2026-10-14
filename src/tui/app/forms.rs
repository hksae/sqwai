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
use crate::config::{Config, EffortControl, EffortLevel, ModelConfig, ModelStatus, WireFormat};
use crate::providers::{self, ChatRequest, Message as PMessage, Role, SharedProvider};
use crate::session::Session;
use crate::tui::markdown::{Highlighter, render, wrap_tagged};
use crate::tui::theme::Theme;

const FORMAT_OPTS: &[&str] = &["openai", "anthropic", "responses"];
/// Per-model effort options: always the full level list — a hardcoded copy
/// here once dropped `xhigh`, showing `off` for xhigh models and clobbering
/// the level on save.
const EFFORT_OPTS: &[&str] = &EffortLevel::STRS;
/// Legacy option lists, kept for the config-file round-trip tests: effort
/// control and always-on left the UI (auto by wire format), but stored
/// values must survive edits. The `[1..]` tail must stay equal to
/// `EffortControl::STRS`.
#[allow(dead_code)]
pub(super) const EFFORT_CONTROL_OPTS: &[&str] = &[
    "auto",
    EffortControl::STRS[0],
    EffortControl::STRS[1],
    EffortControl::STRS[2],
    EffortControl::STRS[3],
];
#[allow(dead_code)]
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
    /// free text edited through a real textarea (cursor, word jumps, paste).
    /// `secret` masks the value on screen (the stored text is always real);
    /// `revealed` flips it back while the user holds the toggle.
    Text {
        label: String,
        ta: Box<TextArea<'static>>,
        secret: bool,
        revealed: bool,
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
        ta.set_cursor_style(Theme::cursor());
        ta.set_selection_style(Theme::selection());
        Self::Text {
            label: label.into(),
            ta,
            secret: false,
            revealed: false,
        }
    }

    /// a secret text field (api keys): masked with bullets unless revealed.
    /// The textarea always holds the real value — masking is display-only,
    /// so save/copy/paste never see bullets.
    pub(super) fn secret(label: &str, value: String) -> Self {
        let mut f = Self::text(label, value);
        if let Self::Text { secret, .. } = &mut f {
            *secret = true;
        }
        f
    }

    /// bullets, capped at 10 so a long key never stretches across the row
    pub(super) fn mask_value(value: &str) -> String {
        value.chars().take(10).map(|_| '•').collect()
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
                                FormField::secret(
                                    "api key",
                                    pc.api_key.clone().unwrap_or_default(),
                                ),
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
                                FormField::secret(
                                    "api key",
                                    pc.api_key.clone().unwrap_or_default(),
                                ),
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
                            FormField::secret("api key", String::new()),
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
                        self.form_fields = vec![
                            FormField::text("key", key.clone().unwrap_or_default()),
                            FormField::text("request id", mc.id.clone()),
                            FormField::text("context", mc.context.to_string()),
                            FormField::choice("effort", EFFORT_OPTS, ef_sel),
                        ];
                    }
                    _ => {
                        // new models start at medium (the old global default)
                        let ef_sel = EFFORT_OPTS
                            .iter()
                            .position(|s| *s == EffortLevel::Medium.as_str())
                            .unwrap_or(0);
                        self.form_fields = vec![
                            FormField::text("key", String::new()),
                            FormField::text("request id", String::new()),
                            FormField::text("context", "128000".into()),
                            FormField::choice("effort", EFFORT_OPTS, ef_sel),
                        ];
                    }
                }
            }
            Some(Menu::EditModelField { key, field, .. }) => {
                use super::menus::ModelField;
                let val = match key.as_ref().and_then(|k| self.cfg.models.get(k)) {
                    Some(mc) => match field {
                        ModelField::Key => key.clone().unwrap_or_default(),
                        ModelField::RequestId => mc.id.clone(),
                        ModelField::Context => mc.context.to_string(),
                    },
                    None => match field {
                        ModelField::Key => key
                            .clone()
                            .unwrap_or_else(|| self.model_draft.key.clone()),
                        ModelField::RequestId => self.model_draft.id.clone(),
                        ModelField::Context => self.model_draft.context.to_string(),
                    },
                };
                let mut f = FormField::text(field.label(), val);
                if let FormField::Text { ta, .. } = &mut f {
                    ta.move_cursor(tui_textarea::CursorMove::End);
                }
                self.form_fields = vec![f];
            }
            Some(Menu::EditProviderField { name, field }) => {
                use super::menus::ProviderField;
                let val = match name.as_ref().and_then(|n| self.cfg.providers.get(n)) {
                    Some(pc) => match field {
                        ProviderField::Name => name.clone().unwrap_or_default(),
                        ProviderField::BaseUrl => pc.base_url.clone(),
                        ProviderField::ApiKey => pc.api_key.clone().unwrap_or_default(),
                        ProviderField::KeyEnvVar => pc.api_key_env.clone().unwrap_or_default(),
                    },
                    None => match field {
                        ProviderField::Name => name
                            .clone()
                            .unwrap_or_else(|| self.provider_draft.name.clone()),
                        ProviderField::BaseUrl => self.provider_draft.base_url.clone(),
                        ProviderField::ApiKey => self.provider_draft.api_key.clone(),
                        ProviderField::KeyEnvVar => self.provider_draft.api_key_env.clone(),
                    },
                };
                let mut f = FormField::text(field.label(), val);
                if let FormField::Text { ta, .. } = &mut f {
                    ta.move_cursor(tui_textarea::CursorMove::End);
                }
                self.form_fields = vec![f];
            }
            Some(Menu::EditSessionTitle { id }) => {
                let title = self
                    .sessions
                    .iter()
                    .find(|s| s.id.to_string() == *id)
                    .map(|s| s.title.clone())
                    .unwrap_or_default();
                let mut f = FormField::text("title", title);
                if let FormField::Text { ta, .. } = &mut f {
                    ta.move_cursor(tui_textarea::CursorMove::End);
                }
                self.form_fields = vec![f];
            }
            Some(Menu::EditScalar(setting)) => {
                let mut f = FormField::text(setting.label(), setting.current(&self.cfg));
                if let FormField::Text { ta, .. } = &mut f {
                    ta.move_cursor(tui_textarea::CursorMove::End);
                }
                self.form_fields = vec![f];
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

    /// left/right/home/end: choices cycle their values, text fields move the cursor.
    /// The effort row never cycles: it opens the Effort menu instead (arrows
    /// would silently fight the menu as the source of truth).
    pub(super) fn form_nav_key(&mut self, k: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        if self.focused_is_choice() {
            let is_effort =
                matches!(
                    self.form_fields.get(self.form_focus),
                    Some(FormField::Choice { .. })
                        if self.form_fields[self.form_focus].label() == "effort"
                ) && matches!(self.cur_menu(), Some(Menu::EditModel { key: Some(_), .. }));
            if !is_effort {
                match k.code {
                    KeyCode::Left => self.choice_cycle(-1),
                    KeyCode::Right => self.choice_cycle(1),
                    _ => {}
                }
            }
            return;
        }
        if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(self.form_focus) {
            ta.input(k);
            self.dirty = true;
        }
    }

    /// true when the focused field is a masked secret (used by the
    /// reveal-toggle keybinding, so plain text fields never react to it)
    pub(super) fn focused_is_secret(&self) -> bool {
        matches!(
            self.form_fields.get(self.form_focus),
            Some(FormField::Text { secret: true, .. })
        )
    }

    /// flip the focused secret field between bullets and the real value
    pub(super) fn toggle_secret_reveal(&mut self) {
        if let Some(FormField::Text {
            secret: true,
            revealed,
            ..
        }) = self.form_fields.get_mut(self.form_focus)
        {
            *revealed = !*revealed;
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

    /// Stash text drafts before a submenu detour, restore them on return.
    /// `prefill_form` rebuilds fields from config — without the stash an
    /// Effort-menu detour would eat unsaved typing.
    pub(super) fn stash_form_text(&mut self) {
        self.form_draft = self
            .form_fields
            .iter()
            .filter_map(|f| match f {
                FormField::Text { .. } => Some((f.label().to_string(), f.trimmed())),
                _ => None,
            })
            .collect();
    }

    pub(super) fn restore_form_text(&mut self) {
        if self.form_draft.is_empty() {
            return;
        }
        for (label, text) in std::mem::take(&mut self.form_draft) {
            if let Some(FormField::Text { ta, .. }) =
                self.form_fields.iter_mut().find(|f| f.label() == label)
            {
                // select-all + insert replaces the prefilled value in place:
                // styles, secrecy and cursor setup survive the round trip
                ta.select_all();
                ta.insert_str(text);
            }
        }
        self.dirty = true;
    }

    pub(super) fn form_mouse_down(&mut self, row: u16, col: u16) {
        let is_surf_field = matches!(
            self.cur_menu(),
            Some(
                Menu::EditModelField { .. }
                    | Menu::EditProviderField { .. }
                    | Menu::EditSessionTitle { .. }
                    | Menu::EditScalar(..)
            )
        );
        // popup fields start past the title row and the air row under it;
        // surface fields also have the top rule row above the title
        let top_y = self.menu_rect.y + if is_surf_field { 3 } else { 2 };
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
                let text_x = if is_surf_field {
                    self.menu_rect.x + 2
                } else {
                    self.menu_rect.x + 1 + label_w
                };
                // the effort row opens its menu instead of cycling (arrows
                // would silently fight the menu as the source of truth).
                // Decided before the field borrow below; new (unsaved)
                // models have no menu target yet, so their row still
                // cycles locally.
                let effort_target: Option<String> = match self.cur_menu() {
                    Some(Menu::EditModel { key: Some(k), .. }) => Some(k.clone()),
                    _ => None,
                };
                let open_effort = matches!(
                    self.form_fields.get(idx),
                    Some(FormField::Choice { .. })
                        if self.form_fields[idx].label() == "effort"
                ) && effort_target.is_some();
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
                    Some(FormField::Choice { .. }) if open_effort => {
                        self.stash_form_text();
                        self.open_menu(Menu::Effort {
                            model: effort_target,
                        });
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
        let is_surf_field = matches!(
            self.cur_menu(),
            Some(
                Menu::EditModelField { .. }
                    | Menu::EditProviderField { .. }
                    | Menu::EditSessionTitle { .. }
                    | Menu::EditScalar(..)
            )
        );
        let label_w = self.form_label_w();
        if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(self.form_focus)
            && ta.is_selecting()
        {
            let text_x = if is_surf_field {
                self.menu_rect.x + 2
            } else {
                self.menu_rect.x + 1 + label_w
            };
            let char_col = (col.saturating_sub(text_x)) as usize;
            ta.move_cursor(tui_textarea::CursorMove::Jump(0, char_col as u16));
            self.dirty = true;
        }
    }

    pub(super) fn form_mouse_up(&mut self) {
        if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(self.form_focus)
            && ta.is_selecting()
        {
            let text = Self::copy_textarea(ta);
            if !text.is_empty() {
                let _ = super::clip_set(&text);
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
                    self.menu_stack.pop();
                    if matches!(self.cur_menu(), Some(Menu::Sessions)) {
                        self.build_menu_rows();
                        self.dirty = true;
                    } else {
                        self.open_menu(Menu::Sessions);
                    }
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
                let (new_key, id, ctx) = (
                    vals.first().cloned().unwrap_or_default(),
                    vals.get(1).cloned().unwrap_or_default(),
                    vals.get(2).cloned().unwrap_or_default(),
                );
                if new_key.is_empty() || id.is_empty() {
                    self.status("key and request id are required", StatusKind::Err);
                    return;
                }
                let Ok(context) = ctx.parse::<u64>() else {
                    self.status("context must be a number", StatusKind::Err);
                    return;
                };
                if key.as_deref() != Some(new_key.as_str())
                    && self.cfg.models.contains_key(&new_key)
                {
                    self.status(
                        &format!("model '{new_key}' already exists"),
                        StatusKind::Err,
                    );
                    return;
                }
                // A built-in key is its catalog identity: after a rename the
                // catalog would re-seed the original key on the next load,
                // leaving a duplicate. Edit `request id` instead.
                if key.as_deref().is_some_and(|k| self.cfg.is_builtin_model(k))
                    && key.as_deref() != Some(new_key.as_str())
                {
                    self.status(
                        "built-in model key cannot be renamed — it is the catalog identity",
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
                // Fields the form does not show must survive an edit.
                // Effort lives in the Effort menu now (current model); the
                // form keeps identity only. New models take the row's choice.
                let previous = key
                    .as_ref()
                    .and_then(|k| self.cfg.models.get(k))
                    .cloned()
                    .or_else(|| self.cfg.models.get(&new_key).cloned());
                let form_effort = self
                    .form_fields
                    .iter()
                    .filter_map(|f| match f {
                        FormField::Choice { options, sel, .. } if f.label() == "effort" => {
                            options.get(*sel).copied()
                        }
                        _ => None,
                    })
                    .next();
                let effort = previous
                    .as_ref()
                    .map(|p| p.effort)
                    .or_else(|| form_effort.and_then(crate::config::EffortLevel::from_str))
                    .unwrap_or(crate::config::EffortLevel::Medium);
                let updated = ModelConfig {
                    provider: provider.clone(),
                    id,
                    context,
                    effort,
                    effort_control: previous.as_ref().and_then(|p| p.effort_control),
                    effort_always_on: previous.as_ref().is_some_and(|p| p.effort_always_on),
                    fallback: previous.as_ref().and_then(|p| p.fallback.clone()),
                    status: previous
                        .as_ref()
                        .map(|p| p.status)
                        .unwrap_or(ModelStatus::Active),
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
            Some(Menu::EditModelField {
                provider,
                key,
                field,
            }) => {
                use super::menus::ModelField;
                let raw = self
                    .form_fields
                    .first()
                    .map(|f| f.trimmed())
                    .unwrap_or_default();
                match field {
                    ModelField::Key => {
                        if raw.is_empty() {
                            self.status("key cannot be empty", StatusKind::Err);
                            return;
                        }
                        if key.as_deref() != Some(raw.as_str())
                            && self.cfg.models.contains_key(&raw)
                        {
                            self.status(
                                &format!("model '{raw}' already exists"),
                                StatusKind::Err,
                            );
                            return;
                        }
                        if key.as_deref().is_some_and(|k| self.cfg.is_builtin_model(k))
                            && key.as_deref() != Some(raw.as_str())
                        {
                            self.status(
                                "built-in model key cannot be renamed — it is the catalog identity",
                                StatusKind::Err,
                            );
                            return;
                        }
                        if let Some(old) = &key {
                            if *old != raw {
                                if let Some(mc) = self.cfg.models.remove(old) {
                                    self.cfg.models.insert(raw.clone(), mc);
                                }
                                if self.session.model_key == *old {
                                    self.session.model_key = raw.clone();
                                }
                                self.rekey_parent_edit_model(&raw);
                                self.cfg.save().ok();
                            }
                        } else {
                            self.model_draft.key = raw;
                            self.try_commit_model_draft(&provider);
                        }
                    }
                    ModelField::RequestId => {
                        if raw.is_empty() {
                            self.status("request id cannot be empty", StatusKind::Err);
                            return;
                        }
                        if let Some(k) = &key {
                            if let Some(mc) = self.cfg.models.get_mut(k) {
                                mc.id = raw;
                                let updated = mc.clone();
                                if self.session.model_key == *k {
                                    self.model_cfg = updated;
                                    self.session.last_response_id = None;
                                    self.session.last_response_model = None;
                                    self.rebuild_provider();
                                }
                                self.cfg.save().ok();
                            }
                        } else {
                            self.model_draft.id = raw;
                            self.try_commit_model_draft(&provider);
                        }
                    }
                    ModelField::Context => {
                        let Ok(context) = raw.parse::<u64>() else {
                            self.status("context must be a number", StatusKind::Err);
                            return;
                        };
                        if let Some(k) = &key {
                            if let Some(mc) = self.cfg.models.get_mut(k) {
                                mc.context = context;
                                let updated = mc.clone();
                                if self.session.model_key == *k {
                                    self.model_cfg = updated;
                                    self.session.context_limit = context;
                                    self.session.last_response_id = None;
                                    self.session.last_response_model = None;
                                    self.rebuild_provider();
                                }
                                self.cfg.save().ok();
                            }
                        } else {
                            self.model_draft.context = context;
                            self.try_commit_model_draft(&provider);
                        }
                    }
                }
                self.menu_back();
            }
            Some(Menu::EditProviderField { name, field }) => {
                use super::menus::ProviderField;
                let raw = self
                    .form_fields
                    .first()
                    .map(|f| f.trimmed())
                    .unwrap_or_default();
                match field {
                    ProviderField::Name => {
                        if raw.is_empty() {
                            self.status("name cannot be empty", StatusKind::Err);
                            return;
                        }
                        if name.as_deref() != Some(raw.as_str())
                            && self.cfg.providers.contains_key(&raw)
                        {
                            self.status(
                                &format!("provider '{raw}' already exists"),
                                StatusKind::Err,
                            );
                            return;
                        }
                        if name.as_deref().is_some_and(|n| self.cfg.is_builtin_provider(n))
                            && name.as_deref() != Some(raw.as_str())
                        {
                            self.status(
                                "built-in provider cannot be renamed",
                                StatusKind::Err,
                            );
                            return;
                        }
                        if let Some(old) = &name {
                            if *old != raw {
                                if let Some(pc) = self.cfg.providers.remove(old) {
                                    for m in self.cfg.models.values_mut() {
                                        if m.provider == *old {
                                            m.provider = raw.clone();
                                        }
                                    }
                                    self.cfg.providers.insert(raw.clone(), pc);
                                }
                                if self.model_cfg.provider == *old {
                                    self.model_cfg.provider = raw.clone();
                                }
                                self.rekey_parent_edit_provider(&raw);
                                self.cfg.save().ok();
                            }
                        } else {
                            self.provider_draft.name = raw;
                            self.try_commit_provider_draft();
                        }
                    }
                    ProviderField::BaseUrl => {
                        if raw.is_empty() {
                            self.status("base url cannot be empty", StatusKind::Err);
                            return;
                        }
                        if let Some(n) = &name {
                            if let Some(pc) = self.cfg.providers.get_mut(n) {
                                pc.base_url = raw;
                                self.cfg.save().ok();
                                if self.model_cfg.provider == *n {
                                    self.rebuild_provider();
                                }
                            }
                        } else {
                            self.provider_draft.base_url = raw;
                            self.try_commit_provider_draft();
                        }
                    }
                    ProviderField::ApiKey => {
                        if let Some(n) = &name {
                            if let Some(pc) = self.cfg.providers.get_mut(n) {
                                pc.api_key = (!raw.is_empty()).then_some(raw);
                                self.cfg.save().ok();
                                if self.model_cfg.provider == *n {
                                    self.rebuild_provider();
                                }
                            }
                        } else {
                            self.provider_draft.api_key = raw;
                            self.try_commit_provider_draft();
                        }
                    }
                    ProviderField::KeyEnvVar => {
                        if let Some(n) = &name {
                            if let Some(pc) = self.cfg.providers.get_mut(n) {
                                pc.api_key_env = (!raw.is_empty()).then_some(raw);
                                self.cfg.save().ok();
                                if self.model_cfg.provider == *n {
                                    self.rebuild_provider();
                                }
                            }
                        } else {
                            self.provider_draft.api_key_env = raw;
                            self.try_commit_provider_draft();
                        }
                    }
                }
                self.menu_back();
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
                self.menu_back();
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

    fn rekey_parent_edit_model(&mut self, new_key: &str) {
        if let Some(idx) = self.menu_stack.len().checked_sub(2)
            && let Some(Menu::EditModel { key, .. }) = self.menu_stack.get_mut(idx)
        {
            *key = Some(new_key.to_string());
        }
    }

    fn try_commit_model_draft(&mut self, provider: &str) {
        if self.model_draft.key.is_empty() || self.model_draft.id.is_empty() {
            return;
        }
        let new_key = self.model_draft.key.clone();
        let updated = ModelConfig {
            provider: provider.to_string(),
            id: self.model_draft.id.clone(),
            context: self.model_draft.context,
            effort: self.model_draft.effort,
            effort_control: None,
            effort_always_on: false,
            fallback: None,
            status: ModelStatus::Active,
        };
        self.cfg.models.insert(new_key.clone(), updated);
        self.cfg.save().ok();
        self.rekey_parent_edit_model(&new_key);
    }

    fn rekey_parent_edit_provider(&mut self, new_name: &str) {
        if let Some(idx) = self.menu_stack.len().checked_sub(2)
            && let Some(Menu::EditProvider { name }) = self.menu_stack.get_mut(idx)
        {
            *name = Some(new_name.to_string());
        }
    }

    pub(super) fn try_commit_provider_draft(&mut self) {
        if self.provider_draft.name.is_empty() || self.provider_draft.base_url.is_empty() {
            return;
        }
        let new_name = self.provider_draft.name.clone();
        if self.cfg.providers.contains_key(&new_name) {
            return;
        }
        let pc = crate::config::ProviderConfig {
            format: self.provider_draft.format,
            base_url: self.provider_draft.base_url.clone(),
            api_key: (!self.provider_draft.api_key.is_empty())
                .then(|| self.provider_draft.api_key.clone()),
            api_key_env: (!self.provider_draft.api_key_env.is_empty())
                .then(|| self.provider_draft.api_key_env.clone()),
            continuation: true,
        };
        self.cfg.providers.insert(new_name.clone(), pc);
        self.cfg.save().ok();
        self.rekey_parent_edit_provider(&new_name);
    }
}
