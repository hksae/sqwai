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
use crate::config::{Config, EffortLevel, ModelConfig, WireFormat};
use crate::providers::{self, ChatRequest, Message as PMessage, Role, SharedProvider};
use crate::session::Session;
use crate::tui::markdown::{Highlighter, render, wrap_tagged};
use crate::tui::theme::Theme;

const ENTER_BURST_GAP: Duration = Duration::from_millis(30);

/// One tick never batches more pasted text than this; the remainder stays
/// queued for the next poll instead of stalling the render loop.
const MAX_PASTE_BATCH_BYTES: usize = 32 * 1024;

#[derive(Debug, Default)]
pub(super) struct EnterGate {
    pending: Option<Instant>,
    burst_until: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EnterDecision {
    Wait,
    Newline,
}

impl EnterGate {
    pub(super) fn on_enter(&mut self, now: Instant) -> EnterDecision {
        if let Some(pending) = self.pending.take()
            && now.duration_since(pending) < ENTER_BURST_GAP
        {
            self.burst_until = Some(now + ENTER_BURST_GAP);
            return EnterDecision::Newline;
        }
        if self.burst_until.is_some_and(|until| now < until) {
            self.burst_until = Some(now + ENTER_BURST_GAP);
            return EnterDecision::Newline;
        }
        self.pending = Some(now);
        EnterDecision::Wait
    }

    pub(super) fn before_key(&mut self, now: Instant) -> bool {
        let Some(at) = self.pending else {
            return false;
        };
        if now.duration_since(at) < ENTER_BURST_GAP {
            self.pending = None;
            self.burst_until = Some(now + ENTER_BURST_GAP);
            true
        } else {
            false
        }
    }

    pub(super) fn note_key(&mut self, now: Instant) {
        if self.burst_until.is_some_and(|until| now < until) {
            self.burst_until = Some(now + ENTER_BURST_GAP);
        }
    }

    pub(super) fn flush(&mut self, now: Instant) -> bool {
        if self
            .pending
            .is_some_and(|at| now.duration_since(at) >= ENTER_BURST_GAP)
        {
            self.pending = None;
            true
        } else {
            false
        }
    }
}

impl App {
    pub(super) fn is_text_input_focused(&self) -> bool {
        if !self.menu_stack.is_empty() {
            if self.is_inline_ask_free() {
                return true;
            }
            if self.is_form_menu() {
                return matches!(
                    self.form_fields.get(self.form_focus),
                    Some(FormField::Text { .. })
                );
            }
            return false;
        }
        true
    }

    fn paste_text(&mut self, text: &str) {
        self.jump_to_bottom_on_typing();
        self.input.insert_str(text);
        self.dirty = true;
    }

    /// A lone digit that would answer the inline question/proposal must reach
    /// the hotkey arms below, not dissolve into a text batch with its
    /// neighbors when several keys arrive in one tick.
    fn is_answer_digit(&self, c: char) -> bool {
        if self.ask_custom_focus.is_some() || !self.menu_stack.is_empty() {
            return false;
        }
        if self.active_ask_seg().is_some() && ('1'..='9').contains(&c) {
            return true;
        }
        self.active_proposal_seg().is_some() && ('1'..='3').contains(&c)
    }

    pub(super) fn poll_input(
        &mut self,
        ev_rx: &std::sync::mpsc::Receiver<crossterm::event::Event>,
    ) -> Result<()> {
        use crossterm::event::{
            Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
        };
        while let Some(ev) = self
            .pending_events
            .pop_front()
            .or_else(|| ev_rx.try_recv().ok())
        {
            if crate::tui::event_log::is_enabled() {
                crate::tui::event_log::log("RX", crate::tui::event_log::describe(&ev));
            }
            match ev {
                Event::Key(k) => {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    if consume_replayed_paste_key(&mut self.paste_replay, k, Instant::now()) {
                        continue;
                    }
                    if matches!(k.code, KeyCode::Enter | KeyCode::Char('\r')) {
                        match self.paste_enter_until {
                            // Synthetic Enter following our own Ctrl+V insert:
                            // swallow once, then disarm so a genuine Enter is
                            // never eaten by a stale guard.
                            Some(until) if Instant::now() <= until => {
                                self.paste_enter_until = None;
                                continue;
                            }
                            _ => self.paste_enter_until = None,
                        }
                    }
                    self.dirty = true;
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
                    let alt = k.modifiers.contains(KeyModifiers::ALT);
                    // Ctrl combos are matched against Latin letters, but with a
                    // Cyrillic (ЙЦУКЕН) layout the terminal reports the layout
                    // character: Ctrl+C arrives as 'с' and would fall through
                    // to typing (jumping the viewport to the bottom) instead of
                    // copying. Translate to the QWERTY key at the same physical
                    // position so every Ctrl combo works on any layout.
                    let mut k = k;
                    if ctrl
                        && let KeyCode::Char(c) = k.code
                        && let Some(lat) = qwerty_char(c)
                    {
                        k.code = KeyCode::Char(lat);
                    }

                    if self.is_text_input_focused()
                        && self.paste_replay.is_none()
                        && !ctrl
                        && !alt
                        && let KeyCode::Char(c) = k.code
                        && !self.is_answer_digit(c)
                    {
                        let mut text_batch = String::new();
                        text_batch.push(c);
                        let mut batch_bytes = c.len_utf8();
                        while let Some(next_ev) = self
                            .pending_events
                            .pop_front()
                            .or_else(|| ev_rx.try_recv().ok())
                        {
                            if let Some(next_c) = is_paste_key(&next_ev) {
                                // Bound one tick's batching: the remainder
                                // stays queued for the next poll instead of
                                // holding the render loop hostage.
                                if batch_bytes + next_c.len_utf8() > MAX_PASTE_BATCH_BYTES {
                                    self.pending_events.push_back(next_ev);
                                    break;
                                }
                                batch_bytes += next_c.len_utf8();
                                text_batch.push(next_c);
                            } else {
                                self.pending_events.push_back(next_ev);
                                break;
                            }
                        }
                        if text_batch.chars().count() > 1 {
                            while batch_bytes < MAX_PASTE_BATCH_BYTES
                                && let Ok(next_ev) =
                                    ev_rx.recv_timeout(std::time::Duration::from_millis(5))
                            {
                                if let Some(next_c) = is_paste_key(&next_ev) {
                                    batch_bytes += next_c.len_utf8();
                                    text_batch.push(next_c);
                                } else {
                                    self.pending_events.push_back(next_ev);
                                    break;
                                }
                            }
                            if !self.menu_stack.is_empty() {
                                if let Some(q) = self.ask_custom_focus {
                                    if let Some(custom) = self.ask_custom.get_mut(q) {
                                        custom.push_str(&text_batch);
                                        self.build_menu_rows();
                                    }
                                } else if self.is_inline_ask_free() {
                                    self.jump_to_bottom_on_typing();
                                    self.paste_text(&text_batch);
                                } else if self.is_form_menu() {
                                    let p = text_batch.replace(['\r', '\n'], " ");
                                    if let Some(FormField::Text { ta, .. }) =
                                        self.form_fields.get_mut(self.form_focus)
                                    {
                                        ta.insert_str(p);
                                    }
                                }
                            } else if let Some(q) = self.ask_custom_focus {
                                if let Some(seg) = self.active_ask_seg()
                                    && let Some(Segment::AskUser { custom, .. }) =
                                        self.segments.get_mut(seg)
                                    && let Some(slot) = custom.get_mut(q)
                                {
                                    slot.push_str(&text_batch);
                                    self.touch_segment(seg);
                                    self.follow = true;
                                }
                            } else {
                                self.paste_text(&text_batch);
                            }
                            self.dirty = true;
                            continue;
                        }
                    }

                    // Ctrl+V is handled below through the clipboard API.
                    // Without bracketed paste, PowerShell cannot inject an
                    // embedded newline as a separate submit event.
                    // a fresh keypress dismisses the previous in-menu notice
                    if !self.menu_stack.is_empty() {
                        self.menu_status = None;
                    }
                    let now = Instant::now();
                    let is_enter = matches!(k.code, KeyCode::Enter | KeyCode::Char('\r'));
                    if !is_enter && self.enter_gate.before_key(now) {
                        self.input.insert_newline();
                    }
                    if !is_enter {
                        self.enter_gate.note_key(now);
                    }
                    // Handle paste as a complete, terminal-independent action. In
                    // particular, do not pass Ctrl+V through to TextArea::input:
                    // some Windows terminal/keymap combinations interpret it as
                    // an accept/submit action after the clipboard text is inserted.
                    if ctrl && matches!(k.code, KeyCode::Char('v') | KeyCode::Char('V')) {
                        let Ok(mut cb) = arboard::Clipboard::new() else {
                            continue;
                        };
                        let Ok(txt) = cb.get_text() else {
                            continue;
                        };
                        let txt = normalize_paste(&txt);
                        self.paste_replay = Some(PasteReplay::new(txt.clone(), now));
                        self.paste_enter_until = Some(now + PASTE_REPLAY_TTL);
                        if !self.menu_stack.is_empty() {
                            let p = txt.replace(['\r', '\n'], " ");
                            if let Some(FormField::Text { ta, .. }) =
                                self.form_fields.get_mut(self.form_focus)
                            {
                                ta.insert_str(p);
                            }
                            self.dirty = true;
                            continue;
                        }
                        self.paste_text(&txt);
                        continue;
                    }
                    match k.code {
                        KeyCode::Char('c') if ctrl => {
                            if self.is_form_menu() {
                                if let Some(FormField::Text { ta, .. }) =
                                    self.form_fields.get_mut(self.form_focus)
                                    && ta.is_selecting()
                                {
                                    ta.copy();
                                    if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                        let text = ta.yank_text();
                                        if !text.is_empty() {
                                            let _ = clipboard.set_text(text);
                                        }
                                    }
                                    self.dirty = true;
                                }
                                continue;
                            }
                            if self.input.is_selecting() {
                                self.input.copy();
                                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                    let text = self.input.yank_text();
                                    if !text.is_empty() {
                                        let _ = clipboard.set_text(text);
                                    }
                                }
                                self.dirty = true;
                                continue;
                            }
                            if let Some(sel) = self.sel {
                                self.copy_selection(&sel);
                            } else if self.input_text().is_empty() {
                                if self
                                    .last_ctrl_c
                                    .is_some_and(|t| t.elapsed() < Duration::from_millis(1500))
                                {
                                    self.quit = true;
                                } else {
                                    self.last_ctrl_c = Some(Instant::now());
                                    self.status("press Ctrl+C again to exit", StatusKind::Info);
                                }
                            } else {
                                self.input = Self::fresh_input(String::new());
                                self.last_ctrl_c = None;
                            }
                        }
                        KeyCode::Char('d') if ctrl && self.menu_stack.is_empty() => {
                            if self.input_text().trim().is_empty() {
                                self.quit = true;
                            }
                        }
                        KeyCode::Esc => {
                            if self.active_subagent.is_some() {
                                self.close_subagent_view();
                            } else if !self.menu_stack.is_empty() {
                                // If custom is focused, Esc blurs it first
                                if self.ask_custom_focus.is_some() {
                                    self.ask_custom_focus = None;
                                    self.build_menu_rows();
                                    self.dirty = true;
                                    continue;
                                }
                                // legacy menu ask (no longer opened for new
                                // asks, kept for compatibility)
                                if matches!(
                                    self.cur_menu(),
                                    Some(Menu::AskUser { .. }) | Some(Menu::AskFree { .. })
                                ) {
                                    self.menu_back();
                                    continue;
                                }
                                // first esc clears an active sessions filter
                                if matches!(self.cur_menu(), Some(Menu::Sessions))
                                    && !self.sessions_filter.is_empty()
                                {
                                    self.sessions_filter.clear();
                                    self.menu_sel = 0;
                                    self.build_menu_rows();
                                    self.dirty = true;
                                } else if let Some(Menu::GraphView { search_filter, .. }) = self.cur_menu_mut()
                                    && search_filter.is_some()
                                {
                                    *search_filter = None;
                                    self.menu_sel = 0;
                                    self.build_menu_rows();
                                    self.dirty = true;
                                } else {
                                    self.menu_back();
                                }
                            } else if self.popup_visible() {
                                self.popup_dismiss = true;
                                self.hover = None;
                            } else if self.active_ask_seg().is_some() {
                                // inline ask lives in the chat, not in a menu:
                                // Esc blurs a custom editor, otherwise skips
                                // (empty answer) — never aborts the turn here
                                self.inline_ask_skip();
                            } else if self.streaming {
                                if self.tool_running() {
                                    // §3.7 / §7 S: a tool is mid-flight —
                                    // cancel that call cooperatively (it gets
                                    // a normal cancelled tool_result) rather
                                    // than tearing down the whole turn. The
                                    // turn still ends once the cancellation is
                                    // observed (run_agent stops requesting
                                    // further model turns), but whatever was
                                    // already produced is kept, unlike a hard
                                    // abort.
                                    if let Some(a) = &self.agent {
                                        a.request_tool_cancel();
                                    }
                                    self.status("cancelling…", StatusKind::Info);
                                } else {
                                    self.clear_busy_statuses();
                                    self.clear_subagent_ui_on_stop();
                                    self.aborted = true;
                                    if let Some(a) = &self.agent {
                                        a.abort();
                                    }
                                }
                            }
                        }
                        KeyCode::Enter if !ctrl && !shift && self.menu_stack.is_empty() => {
                            // an open inline question owns plain Enter
                            // (confirm); the composer still submits via the
                            // gate only once the question is answered
                            if self.active_ask_seg().is_some() {
                                self.inline_ask_confirm();
                            } else if self.enter_gate.on_enter(now) == EnterDecision::Newline {
                                self.input.insert_newline();
                            }
                        }
                        // inline ask (chat, no menu): Tab switches questions;
                        // options are picked by mouse or digits 1-9, arrows
                        // stay with the composer
                        KeyCode::Tab
                            if self.menu_stack.is_empty() && self.active_ask_seg().is_some() =>
                        {
                            let (q, n) = self
                                .active_ask_seg()
                                .and_then(|s| match self.segments.get(s) {
                                    Some(Segment::AskUser {
                                        focus, questions, ..
                                    }) => Some((*focus, questions.len().max(1))),
                                    _ => None,
                                })
                                .unwrap_or((0, 1));
                            let next = if shift { (q + n - 1) % n } else { (q + 1) % n };
                            self.inline_ask_focus(next);
                        }
                        KeyCode::Up if !self.menu_stack.is_empty() => self.menu_nav(-1),
                        KeyCode::Down if !self.menu_stack.is_empty() => self.menu_nav(1),
                        KeyCode::PageUp if !self.menu_stack.is_empty() => self.menu_nav(-2),
                        KeyCode::PageDown if !self.menu_stack.is_empty() => self.menu_nav(2),
                        KeyCode::Left | KeyCode::Right if !self.menu_stack.is_empty() => {
                            if matches!(self.cur_menu(), Some(Menu::Effort)) {
                                self.menu_nav(if k.code == KeyCode::Left { -1 } else { 1 })
                            } else {
                                self.form_nav_key(k)
                            }
                        }
                        KeyCode::Home | KeyCode::End if !self.menu_stack.is_empty() => {
                            if self.is_form_menu() {
                                self.form_nav_key(k);
                            } else {
                                self.menu_jump(k.code == KeyCode::End);
                            }
                        }
                        KeyCode::Enter if self.is_inline_ask_free() => {
                            let text = self.input_text();
                            self.ask_answer(text);
                        }
                        KeyCode::Enter if !self.menu_stack.is_empty() && self.is_inline_ask() => {
                            self.menu_activate()
                        }
                        KeyCode::Enter if !self.menu_stack.is_empty() => self.menu_activate(),
                        // digits answer the inline question directly
                        // (no overlay to click); custom-editor focus keeps
                        // typing for the free-text field instead
                        KeyCode::Char(c)
                            if self.menu_stack.is_empty()
                                && self.active_ask_seg().is_some()
                                && self.ask_custom_focus.is_none()
                                && !ctrl
                                && !alt
                                && ('1'..='9').contains(&c) =>
                        {
                            let digit = (c as usize) - ('0' as usize);
                            let Some(seg) = self.active_ask_seg() else {
                                continue;
                            };
                            let (q, multiple, n) = match self.segments.get(seg) {
                                Some(Segment::AskUser {
                                    focus, questions, ..
                                }) => {
                                    let qq = questions.get(*focus);
                                    (
                                        *focus,
                                        qq.is_some_and(|qq| qq.multiple),
                                        qq.map(|qq| qq.options.len()).unwrap_or(0),
                                    )
                                }
                                _ => continue,
                            };
                            if digit >= 1 && digit <= n {
                                if multiple {
                                    self.inline_ask_toggle(q, digit - 1);
                                } else {
                                    self.inline_ask_select(q, digit - 1);
                                }
                            }
                        }
                        // digits answer the inline plan proposal the same way:
                        // 1 opens the preview, 2/3 accept/decline
                        KeyCode::Char(c)
                            if self.menu_stack.is_empty()
                                && self.active_proposal_seg().is_some()
                                && self.ask_custom_focus.is_none()
                                && !ctrl
                                && !alt
                                && ('1'..='3').contains(&c) =>
                        {
                            match c {
                                '1' => self.open_proposal_preview(),
                                '2' => self.proposal_answer(true),
                                _ => self.proposal_answer(false),
                            }
                        }
                        // plan/act is switched by the user only (design §5)
                        KeyCode::Tab
                            if self.menu_stack.is_empty() && self.active_ask_seg().is_none() =>
                        {
                            self.mode = self.mode.toggle()
                        }
                        KeyCode::Tab if matches!(self.cur_menu(), Some(Menu::AskUser { .. })) => {
                            if let Some(Menu::AskUser { questions, .. }) = self.cur_menu() {
                                let n = questions.len().max(1);
                                if shift {
                                    if self.ask_focus == 0 {
                                        self.ask_focus = n - 1;
                                    } else {
                                        self.ask_focus -= 1;
                                    }
                                } else {
                                    self.ask_focus = (self.ask_focus + 1) % n;
                                }
                                self.build_menu_rows();
                                self.dirty = true;
                            }
                        }
                        KeyCode::Tab if !self.menu_stack.is_empty() => {
                            self.menu_nav(if shift { -1 } else { 1 })
                        }
                        KeyCode::Char('y')
                        | KeyCode::Char('Y')
                        | KeyCode::Char('n')
                        | KeyCode::Char('N')
                            if matches!(self.cur_menu(), Some(Menu::ConfirmDelete { .. })) =>
                        {
                            if k.code == KeyCode::Char('y') || k.code == KeyCode::Char('Y') {
                                self.run_confirm_action();
                            } else {
                                self.menu_back();
                            }
                        }
                        // command approval: a = always this session, d = deny
                        KeyCode::Char('a')
                            if matches!(self.cur_menu(), Some(Menu::Approval { .. })) =>
                        {
                            self.approval_decide(ApprovalDecision::AlwaysSession)
                        }
                        KeyCode::Char('d')
                            if matches!(self.cur_menu(), Some(Menu::Approval { .. })) =>
                        {
                            self.approval_decide(ApprovalDecision::Deny)
                        }
                        KeyCode::Char('s') if ctrl && self.menu_stack.is_empty() => {
                            self.open_menu(Menu::Sessions)
                        }
                        KeyCode::Char('t') if ctrl && self.menu_stack.is_empty() => {
                            self.open_menu(Menu::Todo)
                        }
                        KeyCode::Char('b') | KeyCode::Char('B')
                            if ctrl && self.menu_stack.is_empty() =>
                        {
                            self.open_menu(Menu::Subagents);
                        }
                        KeyCode::Char('a') | KeyCode::Char('A')
                            if ctrl && shift && self.menu_stack.is_empty() =>
                        {
                            self.open_menu(Menu::Subagents);
                        }
                        KeyCode::Char('p') | KeyCode::Char('P')
                            if ctrl && self.menu_stack.is_empty() =>
                        {
                            self.open_menu(Menu::Providers);
                        }
                        KeyCode::Char('l') | KeyCode::Char('L')
                            if ctrl && self.menu_stack.is_empty() =>
                        {
                            self.open_menu(Menu::Plan);
                        }
                        KeyCode::Char('o') | KeyCode::Char('O')
                            if ctrl && self.menu_stack.is_empty() =>
                        {
                            self.open_menu(Menu::Settings);
                        }
                        KeyCode::Char('e') | KeyCode::Char('E')
                            if ctrl && self.menu_stack.is_empty() =>
                        {
                            self.open_menu(Menu::Effort);
                        }
                        KeyCode::Char('g') | KeyCode::Char('G')
                            if ctrl
                                && (self.menu_stack.is_empty()
                                    || matches!(self.cur_menu(), Some(Menu::GraphView { .. }))) =>
                        {
                            if matches!(self.cur_menu(), Some(Menu::GraphView { .. })) {
                                self.menu_back();
                            } else {
                                self.open_graph_view();
                            }
                        }
                        KeyCode::Char('r')
                            if matches!(self.cur_menu(), Some(Menu::Sessions))
                                && self.sessions_filter.is_empty() =>
                        {
                            if let Some(id) = self.selected_session_id() {
                                self.run_action(MenuAction::RenameSession(id));
                            }
                        }
                        KeyCode::Char('p')
                            if matches!(self.cur_menu(), Some(Menu::Sessions))
                                && self.sessions_filter.is_empty() =>
                        {
                            if let Some(id) = self.selected_session_id() {
                                self.run_action(MenuAction::PinSession(id));
                            }
                        }
                        KeyCode::Char('d')
                            if matches!(self.cur_menu(), Some(Menu::Sessions))
                                && self.sessions_filter.is_empty() =>
                        {
                            if let Some(id) = self.selected_session_id() {
                                self.run_action(MenuAction::DeleteSession(id));
                            }
                        }
                        // type-to-filter in the sessions menu
                        KeyCode::Char(c) if matches!(self.cur_menu(), Some(Menu::Sessions)) => {
                            self.sessions_filter.push(c);
                            self.menu_sel = 0;
                            self.build_menu_rows();
                            self.dirty = true;
                        }
                        KeyCode::Backspace if matches!(self.cur_menu(), Some(Menu::Sessions)) => {
                            self.sessions_filter.pop();
                            self.menu_sel = 0;
                            self.build_menu_rows();
                            self.dirty = true;
                        }
                        // graph view controls
                        KeyCode::Char('+') | KeyCode::Char('=')
                            if matches!(self.cur_menu(), Some(Menu::GraphView { search_filter: None, .. })) =>
                        {
                            self.run_action(MenuAction::GraphDepth(1));
                        }
                        KeyCode::Char('-')
                            if matches!(self.cur_menu(), Some(Menu::GraphView { search_filter: None, .. })) =>
                        {
                            self.run_action(MenuAction::GraphDepth(-1));
                        }
                        KeyCode::Char('f') | KeyCode::Char('F') | KeyCode::Char('/')
                            if matches!(self.cur_menu(), Some(Menu::GraphView { search_filter: None, .. })) =>
                        {
                            if let Some(Menu::GraphView { search_filter, .. }) = self.cur_menu_mut() {
                                *search_filter = Some(String::new());
                            }
                            self.menu_sel = 0;
                            self.build_menu_rows();
                            self.dirty = true;
                        }
                        KeyCode::Backspace
                            if matches!(self.cur_menu(), Some(Menu::GraphView { search_filter: None, .. })) =>
                        {
                            self.run_action(MenuAction::GraphBack);
                        }
                        KeyCode::Backspace
                            if matches!(self.cur_menu(), Some(Menu::GraphView { search_filter: Some(_), .. })) =>
                        {
                            if let Some(Menu::GraphView { search_filter: Some(filter), .. }) = self.cur_menu_mut() {
                                filter.pop();
                            }
                            self.menu_sel = 0;
                            self.build_menu_rows();
                            self.dirty = true;
                        }
                        KeyCode::Char(c)
                            if !ctrl
                                && !alt
                                && matches!(self.cur_menu(), Some(Menu::GraphView { search_filter: Some(_), .. })) =>
                        {
                            if let Some(Menu::GraphView { search_filter: Some(filter), .. }) = self.cur_menu_mut() {
                                filter.push(c);
                            }
                            self.menu_sel = 0;
                            self.build_menu_rows();
                            self.dirty = true;
                        }
                        KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
                            if self.menu_stack.is_empty() && self.ask_custom_focus.is_some() =>
                        {
                            // free-text editor inside the inline question:
                            // typing lands in the segment, never the composer
                            if let Some(q) = self.ask_custom_focus
                                && let Some(seg) = self.active_ask_seg()
                                && let Some(Segment::AskUser { custom, .. }) =
                                    self.segments.get_mut(seg)
                                && let Some(slot) = custom.get_mut(q)
                            {
                                match k.code {
                                    KeyCode::Char(c) if !ctrl && !alt => slot.push(c),
                                    KeyCode::Backspace => {
                                        slot.pop();
                                    }
                                    KeyCode::Delete => {
                                        slot.clear();
                                    }
                                    _ => {}
                                }
                                self.touch_segment(seg);
                                self.follow = true;
                                self.dirty = true;
                            }
                        }
                        KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
                            if !self.menu_stack.is_empty() =>
                        {
                            if let Some(q) = self.ask_custom_focus {
                                if let Some(custom) = self.ask_custom.get_mut(q) {
                                    match k.code {
                                        KeyCode::Char(c) => custom.push(c),
                                        KeyCode::Backspace => {
                                            custom.pop();
                                        }
                                        KeyCode::Delete => {
                                            custom.clear();
                                        }
                                        _ => {}
                                    }
                                    self.build_menu_rows();
                                    self.dirty = true;
                                }
                            } else if self.is_inline_ask_free() {
                                if ctrl && handle_text_combo(&mut self.input, k) {
                                    self.jump_to_bottom_on_typing();
                                    self.dirty = true;
                                } else {
                                    self.jump_to_bottom_on_typing();
                                    self.bar_error = None;
                                    self.input.input(k);
                                }
                            } else {
                                self.form_edit_key(k);
                            }
                        }
                        _ if ctrl && handle_text_combo(&mut self.input, k) => {
                            self.jump_to_bottom_on_typing();
                            self.dirty = true;
                        }
                        KeyCode::Char('j') if ctrl => self.input.insert_newline(),
                        KeyCode::PageUp if self.menu_stack.is_empty() => self.page(-1),
                        KeyCode::PageDown if self.menu_stack.is_empty() => self.page(1),
                        KeyCode::Up if ctrl => self.scroll(4),
                        KeyCode::Down if ctrl => self.scroll(-4),
                        // move between lines of a multi-line message without arrows
                        KeyCode::Up if alt => self.input.move_cursor(tui_textarea::CursorMove::Up),
                        KeyCode::Down if alt => {
                            self.input.move_cursor(tui_textarea::CursorMove::Down)
                        }
                        KeyCode::Home if ctrl => {
                            self.follow = false;
                            self.view_top = 0;
                            self.dirty = true;
                        }
                        KeyCode::End if ctrl => {
                            self.follow = true;
                            self.view_top = 0;
                            self.dirty = true;
                        }
                        _ if self.is_inline_ask_free() => {
                            self.jump_to_bottom_on_typing();
                            self.bar_error = None;
                            self.input.input(k);
                        }
                        _ if !self.menu_stack.is_empty() => {}
                        _ => {
                            self.jump_to_bottom_on_typing();
                            self.bar_error = None;
                            self.input.input(k);
                        }
                    }
                    if matches!(
                        k.code,
                        KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
                    ) && !ctrl
                    {
                        self.popup_dismiss = false;
                        self.popup_scroll = 0;
                        self.hover = None;
                    }
                    if !shift {
                        self.sel = None;
                    }
                    self.dirty = true;
                }
                Event::Mouse(m) => match m.kind {
                    // wheel navigates list menus (forms keep wheel inert)
                    MouseEventKind::ScrollUp if !self.menu_stack.is_empty() => {
                        self.menu_scroll_by(-3);
                    }
                    MouseEventKind::ScrollDown if !self.menu_stack.is_empty() => {
                        self.menu_scroll_by(3);
                    }
                    MouseEventKind::ScrollUp => {
                        if self.menu_stack.is_empty()
                            && m.column >= self.last_input.x
                            && m.column < self.last_input.right()
                            && m.row >= self.last_input.y
                            && m.row < self.last_input.bottom()
                            && self.input.lines().len() > 1
                        {
                            self.input
                                .scroll(tui_textarea::Scrolling::Delta { rows: -1, cols: 0 });
                            self.dirty = true;
                        } else if self.popup_visible() {
                            let moved = self.popup_scroll_by(-3);
                            let unhovered = self.hover.is_some();
                            self.hover = None;
                            if moved || unhovered {
                                self.dirty = true;
                            }
                        } else {
                            // 3 lines per notch (was 4): smaller steps read as
                            // smoother motion now that each notch draws its own
                            // frame instead of batching into a 50ms tick.
                            self.scroll(3);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if self.menu_stack.is_empty()
                            && m.column >= self.last_input.x
                            && m.column < self.last_input.right()
                            && m.row >= self.last_input.y
                            && m.row < self.last_input.bottom()
                            && self.input.lines().len() > 1
                        {
                            self.input
                                .scroll(tui_textarea::Scrolling::Delta { rows: 1, cols: 0 });
                            self.dirty = true;
                        } else if self.popup_visible() {
                            let moved = self.popup_scroll_by(3);
                            let unhovered = self.hover.is_some();
                            self.hover = None;
                            if moved || unhovered {
                                self.dirty = true;
                            }
                        } else {
                            self.scroll(-3);
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) if !self.menu_stack.is_empty() => {
                        if self.is_form_menu() {
                            if self.in_menu_rect(m.row, m.column) {
                                self.form_mouse_down(m.row, m.column);
                            } else {
                                if let Some(FormField::Text { ta, .. }) =
                                    self.form_fields.get_mut(self.form_focus)
                                {
                                    ta.cancel_selection();
                                }
                            }
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) if !self.menu_stack.is_empty() => {
                        if self.is_form_menu() && self.form_is_selecting() {
                            self.form_mouse_drag(m.row, m.column);
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) if !self.menu_stack.is_empty() => {
                        if self.is_form_menu() {
                            if !self.in_menu_rect(m.row, m.column) {
                                self.sel = None;
                                self.menu_back();
                            } else if self.form_is_selecting() {
                                self.form_mouse_up();
                            }
                        } else {
                            // inside: pick a row; outside: act like esc
                            if self.in_menu_rect(m.row, m.column) {
                                if matches!(self.cur_menu(), Some(Menu::Effort)) {
                                    // click moves the slider preview, the popup
                                    // stays open; Enter commits, Esc cancels
                                    if let Some(idx) = self.effort_index_at(m.row, m.column)
                                        && idx != self.menu_sel
                                    {
                                        self.menu_sel = idx;
                                        self.dirty = true;
                                    }
                                } else if matches!(self.cur_menu(), Some(Menu::GraphView { .. }))
                                    && self.menu_rect.width >= 100
                                    && m.column >= self.menu_rect.x + 54
                                {
                                    // clicked inside details pane, ignore
                                } else {
                                    self.menu_click(m.row);
                                }
                            } else {
                                self.sel = None;
                                self.menu_back();
                            }
                        }
                    }
                    MouseEventKind::Moved if !self.menu_stack.is_empty() => {
                        // effort slider: hover previews nothing (only click
                        // or arrows change the level)
                        if !matches!(self.cur_menu(), Some(Menu::Effort)) {
                            let _ = self.menu_hover(m.row);
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left)
                        if self.in_input_rect(m.row, m.column) =>
                    {
                        self.input_mouse_down(m.row, m.column)
                    }
                    MouseEventKind::Drag(MouseButton::Left)
                        if self.input_dragging
                            || (self.in_input_rect(m.row, m.column)
                                && self.input.is_selecting()) =>
                    {
                        self.input_mouse_drag(m.row, m.column)
                    }
                    MouseEventKind::Up(MouseButton::Left)
                        if self.input_dragging || self.input.is_selecting() =>
                    {
                        self.input_mouse_up()
                    }
                    MouseEventKind::Down(MouseButton::Left) => self.mouse_down(m.row, m.column),
                    MouseEventKind::Drag(MouseButton::Left) => self.mouse_drag(m.row, m.column),
                    MouseEventKind::Up(MouseButton::Left) => self.mouse_up(m.row, m.column),
                    MouseEventKind::Moved => {
                        // Collapse a hover burst into its last position: only
                        // it affects highlight/hit-testing, the rest would
                        // each force a full re-render for nothing.
                        while matches!(
                            self.pending_events.front(),
                            Some(Event::Mouse(nm)) if nm.kind == MouseEventKind::Moved
                        ) {
                            self.pending_events.pop_front();
                        }
                        self.mouse_move(m.row)
                    }
                    _ => {}
                },
                Event::Paste(p) => {
                    let now = Instant::now();
                    let p = normalize_paste(&p);
                    // Some Windows terminals emit both our Ctrl+V key
                    // event and one or more bracketed-paste events for the
                    // same clipboard payload. The key handler already
                    // inserted the complete text, so discard native payloads
                    // while advancing the replay marker by their exact
                    // prefix. This also handles a payload split at a newline.
                    if consume_replayed_paste_text(&mut self.paste_replay, &p, now) {
                        self.paste_enter_until = Some(now + PASTE_REPLAY_TTL);
                        continue;
                    }
                    self.paste_enter_until = None;
                    if !self.menu_stack.is_empty() {
                        let p = p.replace(['\r', '\n'], " ");
                        if let Some(FormField::Text { ta, .. }) =
                            self.form_fields.get_mut(self.form_focus)
                        {
                            ta.insert_str(p);
                        }
                        self.dirty = true;
                        continue;
                    }
                    self.paste_text(&p);
                }
                Event::Resize(w, h) => {
                    // timestamp only; the draw path debounces the heavy
                    // width rebuild until the drag settles
                    self.last_resize = Some(Instant::now());
                    self.term_size = ratatui::layout::Size::new(w, h);
                    self.dirty = true;
                }
                _ => {}
            }
        }
        Ok(())
    }

    // ---------- mouse ----------

    fn in_input_rect(&self, row: u16, col: u16) -> bool {
        col >= self.last_input.x
            && col < self.last_input.right()
            && row >= self.last_input.y
            && row < self.last_input.bottom()
    }

    fn input_cursor_at(&self, row: u16, col: u16) -> tui_textarea::CursorMove {
        let line = row.saturating_sub(self.last_input.y);
        let column = col.saturating_sub(self.last_input.x + self.input_marker_w() + 1);
        tui_textarea::CursorMove::Jump(line, column)
    }

    pub(super) fn input_mouse_down(&mut self, row: u16, col: u16) {
        self.input.cancel_selection();
        self.input.move_cursor(self.input_cursor_at(row, col));
        self.input_dragging = false;
        self.dirty = true;
    }

    pub(super) fn input_mouse_drag(&mut self, row: u16, col: u16) {
        if !self.input_dragging {
            self.input.start_selection();
            self.input_dragging = true;
        }
        self.input.move_cursor(self.input_cursor_at(row, col));
        self.dirty = true;
    }

    pub(super) fn input_mouse_up(&mut self) {
        if self.input_dragging && self.input.is_selecting() {
            self.input.copy();
            let text = self.input.yank_text();
            if !text.is_empty()
                && let Ok(mut clipboard) = arboard::Clipboard::new()
            {
                let _ = clipboard.set_text(text);
            }
        } else {
            self.input.cancel_selection();
        }
        self.input_dragging = false;
        self.dirty = true;
    }

    #[allow(dead_code)]
    pub(super) fn paste_clipboard(&mut self) {
        let Ok(mut cb) = arboard::Clipboard::new() else {
            return;
        };
        let Ok(txt) = cb.get_text() else {
            return;
        };
        let txt = normalize_paste(&txt);
        if !self.menu_stack.is_empty() {
            // form fields stay single-line
            let text = txt.replace(['\r', '\n'], " ");
            if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(self.form_focus) {
                ta.insert_str(&text);
                self.dirty = true;
            }
            return;
        }
        self.paste_text(&txt);
    }
}

/// QWERTY key at the same physical position for a ЙЦУКЕН character. Latin
/// characters (and anything else) map to themselves via `None` passthrough at
/// the call site; only Cyrillic letters need translation.
fn qwerty_char(c: char) -> Option<char> {
    // Normalize case first: uppercase Cyrillic (Ctrl+Shift combos) must
    // translate too — matching on the raw char returned None before the
    // is_uppercase check below was ever reached.
    let lower = c.to_lowercase().next()?;
    let lat = match lower {
        'й' => 'q',
        'ц' => 'w',
        'у' => 'e',
        'к' => 'r',
        'е' => 't',
        'н' => 'y',
        'г' => 'u',
        'ш' => 'i',
        'щ' => 'o',
        'з' => 'p',
        'ф' => 'a',
        'ы' => 's',
        'в' => 'd',
        'а' => 'f',
        'п' => 'g',
        'р' => 'h',
        'о' => 'j',
        'л' => 'k',
        'д' => 'l',
        'я' => 'z',
        'ч' => 'x',
        'с' => 'c',
        'м' => 'v',
        'и' => 'b',
        'т' => 'n',
        'ь' => 'm',
        'ё' => '`',
        'б' => ',',
        'ю' => '.',
        'х' => '[',
        'ъ' => ']',
        _ => return None,
    };
    if c.is_uppercase() {
        Some(lat.to_ascii_uppercase())
    } else {
        Some(lat)
    }
}

fn is_paste_key(ev: &crossterm::event::Event) -> Option<char> {
    if let crossterm::event::Event::Key(k) = ev
        && k.kind == crossterm::event::KeyEventKind::Press
        && !k.modifiers.intersects(
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
        )
    {
        match k.code {
            crossterm::event::KeyCode::Enter
            | crossterm::event::KeyCode::Char('\r')
            | crossterm::event::KeyCode::Char('\n') => return Some('\n'),
            crossterm::event::KeyCode::Char(c) => return Some(c),
            _ => {}
        }
    }
    None
}

fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Bounded replay state for terminal-echoed clipboard content.
///
/// Some terminals re-emit a Ctrl+V payload as key/Paste events after the
/// application already inserted it. The marker tracks how much of the echo
/// was consumed so the echo never reaches normal input handling — without
/// ever swallowing genuine input:
///
/// - matching is prefix-only against the unconsumed remainder (offset, no
///   suffix copies — char-by-char echo of a large paste stays linear);
/// - the deadline is sliding: every consumed chunk extends it, so a slow
///   stream survives while a dead marker (no replay coming) expires instead
///   of eating future keys that happen to match the clipboard prefix;
/// - a mismatch keeps the marker (terminals interleave trigger artifacts),
///   but the deadline bounds that generosity.
#[derive(Debug, Clone)]
pub(super) struct PasteReplay {
    text: String,
    offset: usize,
    until: Instant,
}

/// Replay markers live only long enough for terminal echo, never across
/// turns: echo bursts arrive within milliseconds of the paste.
const PASTE_REPLAY_TTL: Duration = Duration::from_secs(2);

impl PasteReplay {
    fn new(text: String, now: Instant) -> Self {
        Self {
            text,
            offset: 0,
            until: now + PASTE_REPLAY_TTL,
        }
    }

    /// False once the deadline passed (slot cleared by the caller).
    fn live(slot: &mut Option<Self>, now: Instant) -> bool {
        if slot.as_ref().is_some_and(|r| now > r.until) {
            *slot = None;
        }
        slot.is_some()
    }

    fn remainder(&self) -> &str {
        self.text.get(self.offset..).unwrap_or("")
    }

    fn advance(&mut self, consumed: usize, now: Instant) -> bool {
        self.offset += consumed;
        self.until = now + PASTE_REPLAY_TTL;
        self.offset < self.text.len()
    }
}

fn consume_replayed_paste_text(slot: &mut Option<PasteReplay>, text: &str, now: Instant) -> bool {
    if !PasteReplay::live(slot, now) {
        return false;
    }
    let r = slot.as_mut().unwrap();
    if let Some(rem) = r.remainder().strip_prefix(text) {
        let consumed = r.text.len() - r.offset - rem.len();
        if !r.advance(consumed, now) {
            *slot = None;
        }
        true
    } else {
        false
    }
}

fn consume_replayed_paste_key(
    slot: &mut Option<PasteReplay>,
    key: crossterm::event::KeyEvent,
    now: Instant,
) -> bool {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return false;
    }
    if !PasteReplay::live(slot, now) {
        return false;
    }
    // Key events other than plain characters can never be paste echo.
    let is_newline = matches!(key.code, KeyCode::Char('\r') | KeyCode::Enter);
    if !is_newline && !matches!(key.code, KeyCode::Char(_)) {
        return false;
    }
    let r = slot.as_mut().unwrap();
    let matched = if is_newline {
        if let Some(rem) = r.remainder().strip_prefix('\n') {
            let consumed = r.text.len() - r.offset - rem.len();
            r.advance(consumed, now);
            true
        } else {
            // On Windows, some terminals replay a clipboard newline as Enter
            // after the preceding characters, while the next characters may be
            // delivered in a later batch. Consume that boundary without
            // advancing the marker; otherwise it submits the first line and the
            // rest of the paste is left in the editor. Bounded by the replay
            // deadline above, so a stale marker cannot eat genuine Enters.
            true
        }
    } else if let KeyCode::Char(ch) = key.code {
        if r.remainder().starts_with(ch) {
            r.advance(ch.len_utf8(), now);
            true
        } else {
            // A terminal may interleave the Ctrl+V trigger or a key-release
            // artifact with the replayed payload. Do not discard the marker on
            // one unrelated event; otherwise the first embedded Enter can submit
            // the first line and leave the rest in the editor.
            false
        }
    } else {
        false
    };
    if matched && slot.as_ref().is_some_and(|r| r.offset >= r.text.len()) {
        *slot = None;
    }
    matched
}

/// ctrl combos supported identically in the message input and every form field
#[allow(dead_code)]
pub(super) const TEXT_COMBOS: &[char] = &['z', 'y', 'a', 'e', 'u', 'k', 'w', 'd'];

pub(super) fn is_redo_key(k: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    if !ctrl {
        return false;
    }
    if (shift && matches!(k.code, KeyCode::Char('z') | KeyCode::Char('Z')))
        || k.code == KeyCode::Char('Z')
        || matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y'))
    {
        return true;
    }
    false
}

pub(super) fn is_undo_key(k: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    if !ctrl {
        return false;
    }
    !shift && k.code == KeyCode::Char('z')
}

pub(super) fn is_select_all_key(k: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    ctrl && !shift && matches!(k.code, KeyCode::Char('a') | KeyCode::Char('A'))
}

pub(super) fn select_all(ta: &mut TextArea<'static>) {
    ta.cancel_selection();
    ta.move_cursor(tui_textarea::CursorMove::Top);
    ta.move_cursor(tui_textarea::CursorMove::Head);
    ta.start_selection();
    ta.move_cursor(tui_textarea::CursorMove::Bottom);
    ta.move_cursor(tui_textarea::CursorMove::End);
}

pub(super) fn handle_text_combo(ta: &mut TextArea<'static>, k: crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    use tui_textarea::CursorMove;
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if !ctrl {
        return false;
    }
    if is_select_all_key(&k) {
        select_all(ta);
        return true;
    }
    if is_undo_key(&k) {
        let _ = ta.undo();
        return true;
    }
    if is_redo_key(&k) {
        let _ = ta.redo();
        return true;
    }
    match k.code {
        KeyCode::Char('e') => {
            ta.move_cursor(CursorMove::End);
            true
        }
        KeyCode::Char('k') => {
            let _ = ta.delete_line_by_end();
            true
        }
        KeyCode::Char('u') => {
            let _ = ta.delete_line_by_head();
            true
        }
        KeyCode::Char('w') => {
            let _ = ta.delete_word();
            true
        }
        KeyCode::Char('d') => {
            let _ = ta.delete_char();
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_native_paste_events_are_consumed_without_submission() {
        let now = Instant::now();
        let mut pending = Some(PasteReplay::new("first line\nsecond line".to_string(), now));
        assert!(consume_replayed_paste_text(
            &mut pending,
            "first line\n",
            now
        ));
        assert_eq!(pending.as_ref().map(|r| r.remainder()), Some("second line"));
        assert!(consume_replayed_paste_text(
            &mut pending,
            "second line",
            now
        ));
        assert!(pending.is_none());
    }

    #[test]
    fn replayed_paste_newline_cannot_submit_first_line() {
        let now = Instant::now();
        let mut pending = Some(PasteReplay::new("second line".to_string(), now));
        let enter = crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        assert!(consume_replayed_paste_key(&mut pending, enter, now));
        assert_eq!(pending.as_ref().map(|r| r.remainder()), Some("second line"));
        let first = crossterm::event::KeyEvent::new(KeyCode::Char('s'), KeyModifiers::empty());
        assert!(consume_replayed_paste_key(&mut pending, first, now));
        assert_eq!(pending.as_ref().map(|r| r.remainder()), Some("econd line"));
    }

    #[test]
    fn stale_replay_marker_cannot_swallow_genuine_input() {
        let start = Instant::now();
        let mut pending = Some(PasteReplay::new("hello".to_string(), start));
        let after = start + PASTE_REPLAY_TTL + Duration::from_millis(1);
        // the terminal never replayed: after the deadline the 'h' the user
        // types must reach the editor instead of vanishing into the marker
        let h = crossterm::event::KeyEvent::new(KeyCode::Char('h'), KeyModifiers::empty());
        assert!(!consume_replayed_paste_key(&mut pending, h, after));
        assert!(pending.is_none());
        let enter = crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        assert!(!consume_replayed_paste_key(&mut pending, enter, after));
    }

    #[test]
    fn replay_advances_by_offset_without_copying() {
        let now = Instant::now();
        let mut pending = Some(PasteReplay::new("abcdefgh".to_string(), now));
        assert!(consume_replayed_paste_text(&mut pending, "abcdefg", now));
        let r = pending.as_ref().unwrap();
        assert_eq!(r.offset, 7);
        assert_eq!(r.text, "abcdefgh");
        assert!(consume_replayed_paste_text(&mut pending, "h", now));
        assert!(pending.is_none());
    }

    #[test]
    fn qwerty_translates_uppercase_cyrillic() {
        assert_eq!(qwerty_char('с'), Some('c'));
        assert_eq!(qwerty_char('С'), Some('C'));
        assert_eq!(qwerty_char('C'), None);
        assert_eq!(qwerty_char('a'), None);
    }

    #[test]
    fn normalize_paste_handles_windows_line_endings() {
        assert_eq!(normalize_paste("one\r\ntwo\rthree"), "one\ntwo\nthree");
    }

    #[test]
    fn enter_gate_turns_tight_enter_into_newline() {
        let start = Instant::now();
        let mut gate = EnterGate::default();
        assert_eq!(gate.on_enter(start), EnterDecision::Wait);
        assert!(gate.before_key(start + Duration::from_millis(1)));
        assert!(!gate.flush(start + Duration::from_millis(31)));
    }

    #[test]
    fn enter_gate_flushes_a_real_enter() {
        let start = Instant::now();
        let mut gate = EnterGate::default();
        assert_eq!(gate.on_enter(start), EnterDecision::Wait);
        assert!(gate.flush(start + Duration::from_millis(31)));
    }
}
