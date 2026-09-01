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
            match ev {
                Event::Key(k) => {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    // a fresh keypress dismisses the previous in-menu notice
                    if !self.menu_stack.is_empty() {
                        self.menu_status = None;
                    }
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
                    let alt = k.modifiers.contains(KeyModifiers::ALT);
                    match k.code {
                        KeyCode::Char('c') if ctrl => {
                            // exit is only /exit; ctrl+c copies selection or clears the line
                            if let Some(sel) = self.sel {
                                self.copy_selection(&sel);
                            } else {
                                self.input = Self::fresh_input(String::new());
                            }
                        }
                        // bracketed paste is unavailable on Windows terminals:
                        // read the clipboard directly
                        KeyCode::Char('v') if ctrl => self.paste_clipboard(),
                        KeyCode::Esc => {
                            if !self.menu_stack.is_empty() {
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
                        KeyCode::Enter if !ctrl && !shift && self.startup && self.menu_stack.is_empty() => {
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
                        if self.popup_visible() {
                            self.popup_scroll = self.popup_scroll.saturating_sub(3);
                            self.hover = None;
                            self.dirty = true;
                        } else {
                            self.scroll(4);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if self.popup_visible() {
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
                    MouseEventKind::Down(MouseButton::Left) => self.mouse_down(m.row, m.column),
                    MouseEventKind::Drag(MouseButton::Left) => self.mouse_drag(m.row, m.column),
                    MouseEventKind::Up(MouseButton::Left) => self.mouse_up(m.row, m.column),
                    MouseEventKind::Moved => self.mouse_move(m.row),
                    _ => {}
                },
                Event::Paste(p) => {
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

    pub(super) fn paste_clipboard(&mut self) {
        let Ok(mut cb) = arboard::Clipboard::new() else {
            return;
        };
        let Ok(txt) = cb.get_text() else {
            return;
        };
        if !self.menu_stack.is_empty() {
            // form fields stay single-line
            if let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(self.form_focus) {
                ta.insert_str(txt.replace(['\r', '\n'], " "));
                self.dirty = true;
            }
            return;
        }
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
