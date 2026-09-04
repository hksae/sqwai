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

impl App {
    pub(super) fn poll_input(
        &mut self,
        ev_rx: &std::sync::mpsc::Receiver<crossterm::event::Event>,
    ) -> Result<()> {
        use crossterm::event::{
            Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
        };
        while let Ok(ev) = ev_rx.try_recv() {
            if self.startup && self.menu_stack.is_empty() {
                if let Event::Key(k) = ev {
                    if k.kind == KeyEventKind::Press && k.modifiers.is_empty() {
                        match k.code {
                            KeyCode::Char('q') => self.quit = true,
                            KeyCode::Char('n') => {
                                self.start_new_session();
                            }
                            KeyCode::Enter => self.open_menu(Menu::Sessions),
                            _ => {}
                        }
                    }
                }
                continue;
            }
            match ev {
                Event::Key(k) => {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    // Some Windows consoles do not emit bracketed paste at
                    // all: they replay clipboard characters as key events.
                    // Consume that replay before Enter can submit its first
                    // line or the remainder can be inserted twice.
                    if consume_duplicate_paste_key(&mut self.pasted_clipboard, k) {
                        continue;
                    }
                    // a fresh keypress dismisses the previous in-menu notice
                    if !self.menu_stack.is_empty() {
                        self.menu_status = None;
                    }
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
                    let alt = k.modifiers.contains(KeyModifiers::ALT);
                    // Handle paste as a complete, terminal-independent action. In
                    // particular, do not pass Ctrl+V through to TextArea::input:
                    // some Windows terminal/keymap combinations interpret it as
                    // an accept/submit action after the clipboard text is inserted.
                    if ctrl && matches!(k.code, KeyCode::Char('v') | KeyCode::Char('V')) {
                        // In PowerShell/Windows Terminal Ctrl+V may be reported
                        // as a key event while the terminal also injects the
                        // clipboard bytes asynchronously. Do not let the
                        // synthetic Enter submit the first line.
                        self.paste_clipboard();
                        self.paste_enter_guard = true;
                        self.dirty = true;
                        continue;
                    }
                    if self.paste_enter_guard {
                        // Keep the synthetic Enter from submitting the first
                        // pasted line. Character replay is filtered by the
                        // clipboard-prefix matcher above.
                        if matches!(k.code, KeyCode::Enter | KeyCode::Char('\r')) {
                            self.paste_enter_guard = false;
                            continue;
                        }
                        self.paste_enter_guard = false;
                    }
                    match k.code {
                        KeyCode::Char('c') if ctrl => {
                            // exit is only /exit; ctrl+c copies selection or clears the line
                            if let Some(sel) = self.sel {
                                self.copy_selection(&sel);
                            } else {
                                self.input = Self::fresh_input(String::new());
                            }
                        }
                        KeyCode::Esc => {
                            if self.active_subagent.is_some() {
                                self.active_subagent = None;
                                self.follow = true;
                                self.view_top = 0;
                                self.dirty = true;
                            } else if !self.menu_stack.is_empty() {
                                // first esc clears an active sessions filter
                                if matches!(self.cur_menu(), Some(Menu::Sessions))
                                    && !self.sessions_filter.is_empty()
                                {
                                    self.sessions_filter.clear();
                                    self.menu_sel = 0;
                                    self.build_menu_rows();
                                    self.dirty = true;
                                } else {
                                    self.menu_back();
                                }
                            } else if self.popup_visible() {
                                self.popup_dismiss = true;
                                self.hover = None;
                            } else if self.streaming {
                                self.clear_busy_statuses();
                                self.clear_subagent_ui_on_stop();
                                self.aborted = true;
                                if let Some(a) = &self.agent {
                                    a.abort();
                                }
                            }
                        }
                        KeyCode::Char('q') if self.startup && self.menu_stack.is_empty() => {
                            self.quit = true;
                        }
                        KeyCode::Char('n') if self.startup && self.menu_stack.is_empty() => {
                            self.start_new_session();
                        }
                        KeyCode::Enter
                            if !ctrl && !shift && self.startup && self.menu_stack.is_empty() =>
                        {
                            self.open_menu(Menu::Sessions)
                        }
                        KeyCode::Enter if !ctrl && !shift && self.menu_stack.is_empty() => {
                            self.submit()
                        }
                        KeyCode::Up if !self.menu_stack.is_empty() => self.menu_nav(-1),
                        KeyCode::Down if !self.menu_stack.is_empty() => self.menu_nav(1),
                        KeyCode::PageUp if !self.menu_stack.is_empty() => self.menu_nav(-2),
                        KeyCode::PageDown if !self.menu_stack.is_empty() => self.menu_nav(2),
                        KeyCode::Left | KeyCode::Right if !self.menu_stack.is_empty() => {
                            self.form_nav_key(k)
                        }
                        KeyCode::Home | KeyCode::End if !self.menu_stack.is_empty() => {
                            if self.is_form_menu() {
                                self.form_nav_key(k);
                            } else {
                                self.menu_jump(k.code == KeyCode::End);
                            }
                        }
                        KeyCode::Enter if !self.menu_stack.is_empty() => self.menu_activate(),
                        // plan/act is switched by the user only (design §5)
                        KeyCode::Tab if self.menu_stack.is_empty() => {
                            self.mode = self.mode.toggle()
                        }
                        KeyCode::Tab if !self.menu_stack.is_empty() => {
                            self.menu_nav(if shift { -1 } else { 1 })
                        }
                        KeyCode::Char('y') | KeyCode::Char('n')
                            if matches!(self.cur_menu(), Some(Menu::ConfirmDelete { .. })) =>
                        {
                            if k.code == KeyCode::Char('y') {
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
                        KeyCode::Char('a') if ctrl && self.menu_stack.is_empty() => {
                            self.open_menu(Menu::Subagents)
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
                        KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
                            if !self.menu_stack.is_empty() =>
                        {
                            self.form_edit_key(k)
                        }
                        KeyCode::Char(c) if ctrl && TEXT_COMBOS.contains(&c) => {
                            self.jump_to_bottom_on_typing();
                            text_combo(&mut self.input, c);
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
                        if !self.is_form_menu() {
                            self.menu_nav(-1);
                        }
                    }
                    MouseEventKind::ScrollDown if !self.menu_stack.is_empty() => {
                        if !self.is_form_menu() {
                            self.menu_nav(1);
                        }
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
                            self.popup_scroll = self.popup_scroll.saturating_sub(3);
                            self.hover = None;
                            self.dirty = true;
                        } else {
                            self.scroll(4);
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
                            self.popup_scroll = self.popup_scroll.saturating_add(3);
                            self.hover = None;
                            self.dirty = true;
                        } else {
                            self.scroll(-4);
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) if !self.menu_stack.is_empty() => {}
                    MouseEventKind::Drag(MouseButton::Left) if !self.menu_stack.is_empty() => {}
                    MouseEventKind::Up(MouseButton::Left) if !self.menu_stack.is_empty() => {
                        // inside: pick a row; outside: act like esc
                        if self.in_menu_rect(m.row, m.column) {
                            self.menu_click(m.row);
                        } else {
                            self.sel = None;
                            self.menu_back();
                        }
                    }
                    MouseEventKind::Moved if !self.menu_stack.is_empty() => self.menu_hover(m.row),
                    MouseEventKind::Down(MouseButton::Left)
                        if self.in_input_rect(m.row, m.column) =>
                    {
                        self.input_mouse_down(m.row, m.column)
                    }
                    MouseEventKind::Drag(MouseButton::Left) if self.input.is_selecting() => {
                        self.input_mouse_drag(m.row, m.column)
                    }
                    MouseEventKind::Up(MouseButton::Left) if self.input.is_selecting() => {
                        self.input_mouse_up()
                    }
                    MouseEventKind::Down(MouseButton::Left) => self.mouse_down(m.row, m.column),
                    MouseEventKind::Drag(MouseButton::Left) => self.mouse_drag(m.row, m.column),
                    MouseEventKind::Up(MouseButton::Left) => self.mouse_up(m.row, m.column),
                    MouseEventKind::Moved => self.mouse_move(m.row),
                    _ => {}
                },
                Event::Paste(p) => {
                    // The Ctrl+V branch already inserted this clipboard text.
                    // PowerShell may emit a delayed bracketed-paste event, or
                    // split it into chunks. While the replay marker is present,
                    // consume every such event and never insert it a second time.
                    if self.pasted_clipboard.is_some() {
                        let _ = consume_duplicate_paste(&mut self.pasted_clipboard, &p);
                        continue;
                    }
                    // A genuine paste event must be inserted as one transaction,
                    // not routed through key handling where Enter can submit it.
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
                    self.jump_to_bottom_on_typing();
                    for (i, line) in p.split('\n').enumerate() {
                        if i > 0 {
                            self.input.insert_newline();
                        }
                        let line = line.strip_suffix('\r').unwrap_or(line);
                        self.input.insert_str(line);
                    }
                    self.dirty = true;
                }
                Event::Resize(_, _) => self.dirty = true,
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
        let column = col.saturating_sub(self.last_input.x + 1);
        tui_textarea::CursorMove::Jump(line, column)
    }

    fn input_mouse_down(&mut self, row: u16, col: u16) {
        self.input.move_cursor(self.input_cursor_at(row, col));
        self.input.start_selection();
        self.dirty = true;
    }

    fn input_mouse_drag(&mut self, row: u16, col: u16) {
        self.input.move_cursor(self.input_cursor_at(row, col));
        self.dirty = true;
    }

    fn input_mouse_up(&mut self) {
        if self.input.is_selecting() {
            self.input.copy();
            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                let _ = clipboard.set_text(self.input.yank_text());
            }
            // Keep the selection active so the copied range remains visibly
            // selected, matching transcript selection behavior.
        }
        self.dirty = true;
    }

    pub(super) fn paste_clipboard(&mut self) {
        let Ok(mut cb) = arboard::Clipboard::new() else {
            return;
        };
        let Ok(txt) = cb.get_text() else {
            return;
        };
        if !self.menu_stack.is_empty() {
            // form fields stay single-line
            let text = txt.replace(['\r', '\n'], " ");
            if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(self.form_focus) {
                ta.insert_str(&text);
                self.dirty = true;
            }
            return;
        }
        self.pasted_clipboard = Some(txt.clone());
        self.jump_to_bottom_on_typing();
        for (i, line) in txt.split('\n').enumerate() {
            if i > 0 {
                self.input.insert_newline();
            }
            self.input
                .insert_str(line.strip_suffix('\r').unwrap_or(line));
        }
        self.dirty = true;
    }
}

pub(super) fn consume_duplicate_paste(slot: &mut Option<String>, chunk: &str) -> bool {
    let Some(expected) = slot.as_deref() else {
        return false;
    };
    if expected == chunk {
        *slot = None;
        return true;
    }
    if expected.starts_with(chunk) {
        let remainder = expected[chunk.len()..].to_string();
        *slot = Some(remainder);
        return true;
    }
    *slot = None;
    false
}

pub(super) fn consume_duplicate_paste_key(
    slot: &mut Option<String>,
    key: crossterm::event::KeyEvent,
) -> bool {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return false;
    }
    let Some(ch) = (match key.code {
        KeyCode::Char(ch) => Some(ch),
        KeyCode::Enter => Some('\n'),
        _ => None,
    }) else {
        return false;
    };
    consume_duplicate_paste(slot, &ch.to_string())
}

/// ctrl combos supported identically in the message input and every form field
pub(super) const TEXT_COMBOS: &[char] = &['z', 'y', 'a', 'e', 'u', 'k', 'w', 'd'];

/// shared editor shortcuts for every text input
pub(super) fn text_combo(ta: &mut TextArea<'static>, c: char) -> bool {
    use tui_textarea::CursorMove;
    match c {
        'z' => {
            let _ = ta.undo();
        }
        'y' => {
            let _ = ta.redo();
        }
        'a' => ta.move_cursor(CursorMove::Head),
        'e' => ta.move_cursor(CursorMove::End),
        'k' => {
            let _ = ta.delete_line_by_end();
        }
        'u' => {
            let _ = ta.delete_line_by_head();
        }
        'w' => {
            let _ = ta.delete_word();
        }
        'd' => {
            let _ = ta.delete_char();
        }
        _ => return false,
    }
    true
}
