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
use crate::config::{
    Config, EffortControl, EffortLevel, ModelConfig, ModelStatus, WireFormat, key_name,
    model_display, model_key,
};
use crate::providers::{self, ChatRequest, Message as PMessage, Role, SharedProvider};
use crate::session::Session;
use crate::tui::markdown::{Highlighter, render, wrap_tagged};
use crate::tui::theme::Theme;

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

pub(super) enum FormField {
    /// free text edited through a real textarea (cursor, word jumps, paste).
    /// What a field holds is always what it shows: an api key is bullets on the
    /// provider's own row, and plain text once that row's form is open.
    Text {
        label: String,
        ta: Box<TextArea<'static>>,
    },
}

impl FormField {
    /// Every field is opened to be edited, so the caret starts at the end of
    /// the prefilled value instead of before it.
    pub(super) fn text(label: &str, value: String) -> Self {
        let mut ta = Box::new(TextArea::new(vec![value]));
        ta.set_style(Theme::base());
        ta.set_cursor_line_style(Style::new());
        ta.set_cursor_style(Theme::cursor());
        ta.set_selection_style(Theme::selection());
        ta.move_cursor(tui_textarea::CursorMove::End);
        Self::Text {
            label: label.into(),
            ta,
        }
    }

    /// bullets, capped at 10 so a long key never stretches across the row
    pub(super) fn mask_value(value: &str) -> String {
        value.chars().take(10).map(|_| '•').collect()
    }

    pub(super) fn label(&self) -> &str {
        match self {
            Self::Text { label, .. } => label,
        }
    }

    pub(super) fn current_value(&self) -> String {
        match self {
            // fields are single-line; guard against pasted newlines anyway
            Self::Text { ta, .. } => ta.lines().join(" "),
        }
    }

    pub(super) fn trimmed(&self) -> String {
        self.current_value()
            .trim_matches(|c: char| {
                c.is_whitespace()
                    || matches!(c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' | '\0')
            })
            .to_string()
    }
}

impl App {
    pub(super) fn prefill_form(&mut self) {
        self.form_fields.clear();
        match self.cur_menu() {
            Some(Menu::EditModelField { key, field, .. }) => {
                use super::menus::ModelField;
                let val = match key.as_ref().and_then(|k| self.cfg.models.get(k)) {
                    Some(mc) => match field {
                        ModelField::Key => {
                            key.as_deref().map(key_name).unwrap_or_default().to_string()
                        }
                        ModelField::RequestId => mc.id.clone(),
                        ModelField::Context => mc.context.to_string(),
                    },
                    None => match field {
                        ModelField::Key => key
                            .as_deref()
                            .map(key_name)
                            .unwrap_or(&self.model_draft.key)
                            .to_string(),
                        ModelField::RequestId => self.model_draft.id.clone(),
                        ModelField::Context => self.model_draft.context.to_string(),
                    },
                };
                self.form_fields = vec![FormField::text(field.label(), val)];
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
                self.form_fields = vec![FormField::text(field.label(), val)];
            }
            Some(Menu::EditSessionTitle { id }) => {
                let title = self
                    .sessions
                    .iter()
                    .find(|s| s.id.to_string() == *id)
                    .map(|s| s.title.clone())
                    .unwrap_or_default();
                let f = FormField::text("title", title);
                self.form_fields = vec![f];
            }
            Some(Menu::EditScalar(setting)) => {
                let f = FormField::text(setting.label(), setting.edit_value(&self.cfg));
                self.form_fields = vec![f];
            }
            Some(Menu::AskFree { .. }) => {
                self.form_fields = vec![FormField::text("answer", String::new())];
            }
            _ => {}
        }
    }

    /// Label column width for the open form: fits the longest field label
    /// (`" {label:>w$} : "`), never narrower than the legacy 16 columns so
    /// short-label forms render exactly as before.
    pub(super) fn form_label_w(&self) -> u16 {
        let widest = self
            .form_fields
            .iter()
            .map(|f| unicode_width::UnicodeWidthStr::width(f.label()))
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

    /// left/right/home/end reach the textarea, which owns cursor movement.
    /// Every choice in the UI is a menu row now, so no form field cycles values.
    pub(super) fn form_nav_key(&mut self, k: crossterm::event::KeyEvent) {
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
        if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(self.form_focus) {
            if handle_text_combo(ta, k) {
                self.dirty = true;
                return;
            }
            ta.input(k);
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
                if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(idx) {
                    if col >= text_x {
                        let char_col = (col.saturating_sub(text_x)) as usize;
                        ta.move_cursor(tui_textarea::CursorMove::Jump(0, char_col as u16));
                        ta.start_selection();
                    } else {
                        ta.cancel_selection();
                        ta.move_cursor(tui_textarea::CursorMove::End);
                    }
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
                            self.status("name cannot be empty", StatusKind::Err);
                            return;
                        }
                        // the row edits the model's name; its provider comes from
                        // the menu it was opened from, so the identity is built
                        // here rather than typed.
                        let new_key = model_key(&provider, &raw);
                        if key.as_deref() != Some(new_key.as_str())
                            && self.cfg.models.contains_key(&new_key)
                        {
                            self.status(
                                &format!("model '{}' already exists", model_display(&new_key)),
                                StatusKind::Err,
                            );
                            return;
                        }
                        if key.as_deref().is_some_and(|k| self.cfg.is_builtin_model(k))
                            && key.as_deref() != Some(new_key.as_str())
                        {
                            self.status(
                                "built-in model name cannot be renamed — it is the catalog identity",
                                StatusKind::Err,
                            );
                            return;
                        }
                        if let Some(old) = &key {
                            if *old != new_key {
                                if let Some(mc) = self.cfg.models.remove(old) {
                                    self.cfg.models.insert(new_key.clone(), mc);
                                }
                                if self.session.model_key == *old {
                                    self.session.model_key = new_key.clone();
                                }
                                self.rekey_parent_edit_model(&new_key);
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
                        if name
                            .as_deref()
                            .is_some_and(|n| self.cfg.is_builtin_provider(n))
                            && name.as_deref() != Some(raw.as_str())
                        {
                            self.status("built-in provider cannot be renamed", StatusKind::Err);
                            return;
                        }
                        if let Some(old) = &name {
                            if *old != raw {
                                let pc = self.cfg.providers.remove(old);
                                let skipped = self.cfg.rename_provider_models(old, &raw);
                                if let Some(pc) = pc {
                                    self.cfg.providers.insert(raw.clone(), pc);
                                }
                                if self.model_cfg.provider == *old {
                                    self.model_cfg.provider = raw.clone();
                                }
                                self.rekey_parent_edit_provider(&raw);
                                self.cfg.save().ok();
                                if skipped > 0 {
                                    self.status(
                                        &format!(
                                            "{skipped} model(s) kept their old key: '{raw}' \
                                             already serves that name"
                                        ),
                                        StatusKind::Err,
                                    );
                                }
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
        let new_key = model_key(provider, &self.model_draft.key);
        let updated = ModelConfig {
            provider: provider.to_string(),
            id: self.model_draft.id.clone(),
            context: self.model_draft.context,
            effort: self.model_draft.effort,
            effort_control: None,
            effort_always_on: false,
            fallback: None,
            status: ModelStatus::Active,
            vision: None,
            pdf: None,
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
            models: Default::default(),
        };
        self.cfg.providers.insert(new_name.clone(), pc);
        self.cfg.save().ok();
        self.rekey_parent_edit_provider(&new_name);
    }
}
