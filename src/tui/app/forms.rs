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
use crate::config::{Config, EffortLevel, ModelConfig, WireFormat};
use crate::providers::{self, ChatRequest, Message as PMessage, Role, SharedProvider};
use crate::session::Session;
use crate::tui::markdown::{Highlighter, render, wrap_tagged};
use crate::tui::theme::Theme;

const FORMAT_OPTS: &[&str] = &["openai", "anthropic", "responses"];
const EFFORT_OPTS: &[&str] = &["off", "low", "medium", "high", "max"];

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
        ta.set_cursor_line_style(Style::new().bg(Theme::SURFACE()));
        ta.set_cursor_style(Style::new().bg(Theme::ACCENT_SOFT()).fg(Theme::BG()));
        ta.set_selection_style(Style::new().bg(Theme::ACCENT()).fg(Theme::BG()));
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
                match name.as_ref().and_then(|n| self.cfg.providers.get(n)) {
                    Some(pc) => {
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
                        self.form_fields = vec![
                            FormField::text("key", key.clone().unwrap_or_default()),
                            FormField::text("request id", mc.id.clone()),
                            FormField::text("context", mc.context.to_string()),
                            FormField::choice("effort", EFFORT_OPTS, ef_sel),
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
                let label_w = 16u16;
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
        if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(self.form_focus)
            && ta.is_selecting()
        {
            let label_w = 16u16;
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
                let mut found = false;
                for s in self.sessions.iter_mut() {
                    if s.id.to_string() == id {
                        s.title = title.clone();
                        let _ = s.save();
                        found = true;
                        break;
                    }
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
                let (new_key, id, ctx, th) = (
                    vals.first().cloned().unwrap_or_default(),
                    vals.get(1).cloned().unwrap_or_default(),
                    vals.get(2).cloned().unwrap_or_default(),
                    vals.get(3).cloned().unwrap_or_default(),
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
                // this, editing a model silently reset its prices, and it
                // would now also reset its declared effort support.
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
                    effort_control: previous.as_ref().and_then(|p| p.effort_control),
                    effort_always_on: previous.as_ref().is_some_and(|p| p.effort_always_on),
                    price_in: previous.as_ref().and_then(|p| p.price_in),
                    price_out: previous.as_ref().and_then(|p| p.price_out),
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
            _ => {}
        }
    }
}
