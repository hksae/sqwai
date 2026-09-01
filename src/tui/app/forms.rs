#![allow(unused_imports)]
use super::events::text_combo;
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

const FORMAT_OPTS: &[&str] = &["openai", "anthropic", "responses"];
const THINKING_OPTS: &[&str] = &["off", "low", "medium", "high", "max"];

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
        ta.set_cursor_style(Style::new().bg(Theme::ACCENT()).fg(Theme::BG()));
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
                        let th_sel = THINKING_OPTS
                            .iter()
                            .position(|s| *s == mc.thinking.as_str())
                            .unwrap_or(0);
                        self.form_fields = vec![
                            FormField::text("key", key.clone().unwrap_or_default()),
                            FormField::text("request id", mc.id.clone()),
                            FormField::text("context", mc.context.to_string()),
                            FormField::choice("thinking", THINKING_OPTS, th_sel),
                        ];
                    }
                    _ => {
                        let th_sel = THINKING_OPTS
                            .iter()
                            .position(|s| *s == self.cfg.default_thinking.as_str())
                            .unwrap_or(0);
                        self.form_fields = vec![
                            FormField::text("key", String::new()),
                            FormField::text("request id", String::new()),
                            FormField::text("context", "128000".into()),
                            FormField::choice("thinking", THINKING_OPTS, th_sel),
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
            if k.modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
                && let KeyCode::Char(c) = k.code
                && text_combo(ta, c)
            {
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
                if name.is_none() && self.cfg.providers.contains_key(&new_name) {
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
                };
                if let Some(old) = &name {
                    if old != &new_name {
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
                let thinking = ThinkingLevel::from_str(&th).unwrap_or(ThinkingLevel::Off);
                if key.is_none() && self.cfg.models.contains_key(&new_key) {
                    self.status(
                        &format!("model '{new_key}' already exists"),
                        StatusKind::Err,
                    );
                    return;
                }
                if let Some(old) = &key {
                    if old != &new_key {
                        self.cfg.models.remove(old);
                        if self.session.model_key == *old {
                            self.session.model_key = new_key.clone();
                        }
                    }
                }
                self.cfg.models.insert(
                    new_key.clone(),
                    ModelConfig {
                        provider: provider.clone(),
                        id,
                        context,
                        thinking,
                        price_in: None,
                        price_out: None,
                    },
                );
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
