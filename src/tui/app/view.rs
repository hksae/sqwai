#![allow(unused_imports)]
use super::menus::{COMMANDS, POPUP_MAX_ROWS};
use super::*;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use std::hash::{Hash, Hasher};
use tui_textarea::TextArea;

use crate::agent::loop_task::AgentEvent;
use crate::config::{Config, ModelConfig, ThinkingLevel, WireFormat};
use crate::providers::{self, ChatRequest, Message as PMessage, Role, SharedProvider};
use crate::session::Session;
use crate::tui::markdown::{Highlighter, render, wrap_tagged};
use crate::tui::theme::Theme;

const STARTUP_LOGO: &str = "█████   ████████ █████ ███ █████  ██████   ████
███▒▒   ███▒▒███ ▒▒███ ▒███▒▒███  ▒▒▒▒▒███ ▒▒███
▒▒█████ ▒███ ▒███  ▒███ ▒███ ▒███   ███████  ▒███
▒▒▒▒███▒███ ▒███  ▒▒███████████   ███▒▒███  ▒███
██████ ▒▒███████   ▒▒████▒████   ▒▒████████ █████
▒▒▒▒▒▒   ▒▒▒▒▒███    ▒▒▒▒ ▒▒▒▒     ▒▒▒▒▒▒▒▒ ▒▒▒▒▒";

fn startup_logo(width: u16, height: u16) -> Vec<String> {
    let source: Vec<Vec<char>> = STARTUP_LOGO
        .lines()
        .map(|line| line.chars().collect())
        .collect();
    let natural_width = source.iter().map(Vec::len).max().unwrap_or(0);
    let natural_height = source.len();
    let target_width = usize::from(width).max(1);
    let target_height = usize::from(height).max(1);
    let scale = (target_width as f32 / natural_width.max(1) as f32)
        .min(target_height as f32 / natural_height.max(1) as f32)
        .min(1.0);

    // Downscaling uses nearest-neighbor sampling to keep the original art
    // intact as much as possible within a smaller terminal.

    let output_width = ((natural_width as f32 * scale).round() as usize).max(1);
    let output_height = ((natural_height as f32 * scale).round() as usize).max(1);

    (0..output_height)
        .map(|row| {
            let source_row = row * natural_height / output_height;
            (0..output_width)
                .map(|column| {
                    let source_column = column * natural_width / output_width;
                    source
                        .get(source_row)
                        .and_then(|line| line.get(source_column))
                        .copied()
                        .unwrap_or(' ')
                })
                .collect()
        })
        .collect()
}

#[derive(Debug)]
pub(super) enum Segment {
    User(String),
    Assistant {
        text: String,
        live: bool,
    },
    Thinking {
        text: String,
        expanded: bool,
        live: bool,
    },
    /// one tool call: spinner while running, result line when finished,
    /// full output or diff on click
    Tool {
        name: String,
        args: String,
        /// `None` while the tool is still running
        ok: Option<bool>,
        output: String,
        diff: Option<String>,
        expanded: bool,
    },
    Status {
        text: String,
        kind: StatusKind,
    },
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum BlockKind {
    None,
    ThoughtCollapsed,
    ThoughtExpanded,
    Answer,
}

/// User message rendered inside a rounded pink outline.
fn user_box(text: &str, w: u16, hl: &Highlighter) -> Vec<Line<'static>> {
    let max_inner = w.saturating_sub(6).clamp(8, 100) as usize;
    let natural = text
        .lines()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, max_inner);
    let inner_w = natural as u16;
    let inner = render(text, inner_w, hl);
    let iw = inner_w as usize;
    let b = Theme::border_focused();
    let mut out = Vec::with_capacity(inner.len() + 2);
    let edge = |lft: &str, mid: String, rgt: &str| {
        Line::from(vec![
            Span::styled(lft.to_string(), b),
            Span::styled(mid, b),
            Span::styled(rgt.to_string(), b),
        ])
    };
    out.push(edge("╭", "─".repeat(iw + 2), "╮"));
    for l in inner {
        let t = line_text(&l);
        let pad = " ".repeat(iw.saturating_sub(t.chars().count()));
        out.push(Line::from(vec![
            Span::styled("│".to_string(), b),
            Span::styled(" ".to_string(), Theme::base()),
            Span::styled(format!("{t}{pad}"), Theme::base()),
            Span::styled(" ".to_string(), Theme::base()),
            Span::styled("│".to_string(), b),
        ]));
    }
    out.push(edge("╰", "─".repeat(iw + 2), "╯"));
    out
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct CellPos {
    pub(super) row: usize, // absolute row in cache_lines
    pub(super) col: usize,
}

#[derive(Clone, Copy)]
pub(super) struct Selection {
    pub(super) a: CellPos,
    pub(super) b: CellPos,
}

impl Selection {
    pub(super) fn rows(&self) -> (usize, usize) {
        (self.a.row.min(self.b.row), self.a.row.max(self.b.row))
    }
}

impl App {
    // ---------- mouse ----------

    /// screen row -> absolute row in cache_lines
    pub(super) fn abs_row(&self, screen_row: u16) -> usize {
        self.chat_top(self.last_chat.height.max(1))
            + (screen_row.saturating_sub(self.last_chat.y)) as usize
    }

    pub(super) fn mouse_down(&mut self, row: u16, col: u16) {
        self.press = Some(CellPos {
            row: self.abs_row(row),
            col: col.saturating_sub(1) as usize,
        });
        self.dragging = false;
    }

    pub(super) fn mouse_drag(&mut self, row: u16, col: u16) {
        let Some(p0) = self.press else { return };
        let cur = CellPos {
            row: self.abs_row(row),
            col: col.saturating_sub(1) as usize,
        };
        if !self.dragging && cur == p0 {
            return;
        }
        self.dragging = true;
        self.sel = Some(Selection { a: p0, b: cur });
        self.dirty = true;
    }

    pub(super) fn mouse_up(&mut self, row: u16, col: u16) {
        // thinking selector in the status bar
        if let Some((x0, x1)) = self.th_click {
            if row == self.status_y && col >= x0 && col <= x1 && self.menu_stack.is_empty() {
                self.open_menu(Menu::Thinking);
                return;
            }
        }
        let pressed = self.press.take();
        let was_drag = std::mem::take(&mut self.dragging);
        if was_drag {
            if let Some(sel) = self.sel.clone() {
                self.copy_selection(&sel);
            }
            return; // keep the selection visible until the next action
        }
        self.sel = None;
        // command popup item?
        if let Some((_, idx)) = self.popup_rows.iter().find(|(y, _)| *y == row) {
            let cmd = COMMANDS[*idx].to_string();
            self.apply_command_insert(&cmd);
            return;
        }
        if let Some(p) = pressed {
            self.click(p.row);
        }
    }

    pub(super) fn mouse_move(&mut self, row: u16) {
        if self.popup_visible() {
            let hov = self
                .popup_rows
                .iter()
                .find(|(y, _)| *y == row)
                .map(|(_, i)| *i);
            if hov != self.hover {
                self.hover = hov;
                self.dirty = true;
            }
        }
    }

    pub(super) fn click(&mut self, abs_row: usize) {
        if let Some(Some(seg_idx)) = self.cache_rowseg.get(abs_row) {
            if let Some(text) = self.code_at_row(*seg_idx, abs_row) {
                match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
                    Ok(()) => self.status("code copied to clipboard", StatusKind::Info),
                    Err(e) => self.status(&format!("copy failed: {e}"), StatusKind::Err),
                }
                return;
            }
            // clicking an error line copies its full text to the clipboard
            let err_text = match self.segments.get(*seg_idx) {
                Some(Segment::Status {
                    text,
                    kind: StatusKind::Err,
                }) => Some(text.clone()),
                _ => None,
            };
            if let Some(text) = err_text {
                match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
                    Ok(()) => self.status("error text copied to clipboard", StatusKind::Info),
                    Err(e) => self.status(&format!("copy failed: {e}"), StatusKind::Err),
                }
                return;
            }
            let toggle = match self.segments.get(*seg_idx) {
                Some(Segment::Thinking { expanded, .. }) => Some(!*expanded),
                // a finished tool row reveals its full output or diff
                Some(Segment::Tool {
                    ok: Some(_),
                    expanded,
                    ..
                }) => Some(!*expanded),
                _ => None,
            };
            if let Some(v) = toggle {
                match self.segments.get_mut(*seg_idx) {
                    Some(Segment::Thinking { expanded, .. }) => *expanded = v,
                    Some(Segment::Tool { expanded, .. }) => *expanded = v,
                    _ => {}
                }
                self.dirty = true;
            }
        }
    }

    fn code_at_row(&self, seg_idx: usize, abs_row: usize) -> Option<String> {
        let rendered = line_text(self.cache_lines.get(abs_row)?);
        if !rendered.trim_start().starts_with('│') && !rendered.contains("│ ") {
            return None;
        }
        let Segment::Assistant { text, .. } = self.segments.get(seg_idx)? else {
            return None;
        };
        let mut blocks = Vec::new();
        let mut in_code = false;
        let mut current = String::new();
        for line in text.lines() {
            if line.trim_start().starts_with("```") {
                if in_code {
                    blocks.push(current.trim_end_matches('\n').to_string());
                    current.clear();
                }
                in_code = !in_code;
            } else if in_code {
                current.push_str(line);
                current.push('\n');
            }
        }
        blocks.into_iter().next()
    }

    pub(super) fn copy_selection(&mut self, sel: &Selection) {
        let (r0, r1) = sel.rows();
        let max_row = self.cache_lines.len().saturating_sub(1);
        let mut out = String::new();
        for r in r0..=r1.min(max_row) {
            let chars: Vec<char> = line_text(&self.cache_lines[r]).chars().collect();
            let start = if r == r0 {
                sel.a.col.min(sel.b.col).min(chars.len())
            } else {
                0
            };
            let end = if r == r1 {
                sel.a.col.max(sel.b.col).min(chars.len())
            } else {
                chars.len()
            };
            let mut line: String = chars[start..end].iter().collect();
            line = line
                .trim_start_matches([' ', '│', '╭', '╰', '─'])
                .trim_end_matches([' ', '│', '╮', '╯', '─'])
                .to_string();
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&line);
        }
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(out)) {
            Ok(()) => {}
            Err(e) => self.status(&format!("copy failed: {e}"), StatusKind::Err),
        }
        self.dirty = true;
    }

    // ---------- rendering ----------

    /// cache key for a segment's rendered content; changing it forces a repaint
    pub(super) fn seg_key(&self, seg: &Segment) -> usize {
        let base = match seg {
            Segment::User(t) => t.chars().count(),
            Segment::Assistant { text, .. } => text.chars().count(),
            Segment::Thinking { text, expanded, .. } => {
                text.chars().count() * 2 + *expanded as usize
            }
            Segment::Tool {
                name,
                args,
                ok,
                output,
                diff,
                expanded,
            } => {
                let mut k = name.chars().count()
                    + args.chars().count()
                    + output.chars().count()
                    + diff.as_ref().map(|d| d.chars().count() * 3).unwrap_or(0)
                    + usize::from(*expanded) * 5;
                k = match ok {
                    // running: the spinner frame is part of the key
                    None => k.wrapping_add(self.spinner_tick * 7),
                    Some(true) => k.wrapping_add(1),
                    Some(false) => k.wrapping_add(2),
                };
                k
            }
            Segment::Status { .. } => 0,
        };
        // an animated theme shifts its palette every frame, so baked-in
        // frame/border/background colors must be re-rendered too — fold the
        // tick into the key. Segments only re-highlight while an anim theme
        // is active; static themes keep the cheap content-only key.
        if crate::tui::theme::anim_theme_index().is_some() {
            base.wrapping_mul(31)
                .wrapping_add(crate::tui::theme::anim_tick() as usize)
        } else {
            base
        }
    }

    pub(super) fn render_segment(&self, idx: usize, w: u16) -> Vec<(Line<'static>, Option<usize>)> {
        let mut out: Vec<(Line<'static>, Option<usize>)> = Vec::new();
        match &self.segments[idx] {
            Segment::User(text) => {
                for l in user_box(text, w, &self.hl) {
                    out.push((l, Some(idx)));
                }
            }
            Segment::Assistant { text, .. } => {
                for l in render(text, w, &self.hl) {
                    out.push((l, Some(idx)));
                }
            }
            Segment::Thinking { text, expanded, .. } => {
                if !*expanded {
                    let n = text.chars().count();
                    let spans = vec![Span::styled(
                        format!("  thinking ({n} chars)"),
                        Theme::dim(),
                    )];
                    out.push((Line::from(spans), Some(idx)));
                } else {
                    for l in render(text, w, &self.hl) {
                        out.push((dim_all(l), Some(idx)));
                    }
                    out.push((
                        Line::from(vec![Span::styled(
                            "  click to collapse".to_string(),
                            Style::new().fg(Theme::DIM()).add_modifier(Modifier::ITALIC),
                        )]),
                        Some(idx),
                    ));
                }
            }
            Segment::Tool {
                name,
                args: _,
                ok,
                output,
                diff,
                expanded,
            } => {
                let prefix = name.clone();
                let head = match ok {
                    None => Line::from(vec![
                        Span::styled(
                            format!(
                                "  {} ",
                                WORKING_SPINNER[self.spinner_tick % WORKING_SPINNER.len()]
                            ),
                            Theme::accent(),
                        ),
                        Span::styled(prefix.clone(), Theme::dim()),
                    ]),
                    Some(true) => Line::from(vec![
                        Span::styled("  ✓ ".to_string(), Theme::ok()),
                        Span::styled(prefix.clone(), Theme::dim()),
                    ]),
                    Some(false) => Line::from(vec![
                        Span::styled("  ✗ ".to_string(), Theme::err()),
                        Span::styled(prefix.clone(), Theme::dim()),
                    ]),
                };
                out.push((head, Some(idx)));
                if *expanded {
                    let body = diff.clone().unwrap_or_else(|| output.clone());
                    const MAX_ROWS: usize = 40;
                    let rows: Vec<&str> = body.lines().collect();
                    let shown = &rows[..rows.len().min(MAX_ROWS)];
                    let border = Theme::border_dim();
                    let inner_w = w.saturating_sub(8).max(8) as usize;
                    // Keep the box width stable. Deriving it from the longest
                    // line makes the border itself wrap when the chat width
                    // changes, which leaves visibly shifted corners behind.
                    let width = inner_w.max(8);
                    out.push((
                        Line::from(vec![Span::styled(
                            format!("    ╭{}╮", "─".repeat(width + 2)),
                            border,
                        )]),
                        Some(idx),
                    ));
                    for l in shown {
                        let st = if l.starts_with('+') && !l.starts_with("+++") {
                            Theme::ok()
                        } else if l.starts_with('-') && !l.starts_with("---") {
                            Theme::err()
                        } else if l.starts_with("@@") {
                            Theme::accent()
                        } else {
                            Theme::dim()
                        };
                        let line = truncate_chars(l, width);
                        out.push((
                            Line::from(vec![
                                Span::styled("    │ ", border),
                                Span::styled(format!("{line:<width$}"), st),
                                Span::styled(" │", border),
                            ]),
                            Some(idx),
                        ));
                    }
                    if rows.len() > MAX_ROWS {
                        let more = format!("… {} more lines", rows.len() - MAX_ROWS);
                        out.push((
                            Line::from(vec![
                                Span::styled("    │ ", border),
                                Span::styled(
                                    format!("{:<width$}", truncate_chars(&more, width)),
                                    Theme::dim(),
                                ),
                                Span::styled(" │", border),
                            ]),
                            Some(idx),
                        ));
                    }
                    out.push((
                        Line::from(vec![Span::styled(
                            format!("    ╰{}╯", "─".repeat(width + 2)),
                            border,
                        )]),
                        Some(idx),
                    ));
                }
            }
            Segment::Status { text, kind } => {
                let st = match kind {
                    StatusKind::Info => Theme::dim(),
                    StatusKind::Ok => Theme::ok(),
                    StatusKind::Warn => Theme::warn(),
                    StatusKind::Err => Theme::err(),
                };
                for part in text.split('\n') {
                    out.push((
                        Line::from(vec![Span::styled(format!("  {part}"), st)]),
                        Some(idx),
                    ));
                }
            }
        }
        out
    }

    pub(super) fn rebuild_cache(&mut self, width: u16) {
        let w = width.saturating_sub(2).max(10); // side padding
        let mut logical: Vec<(Line<'static>, Option<usize>)> = Vec::new();
        let mut in_group = false; // inside one "agent" turn
        let mut last_block = BlockKind::None;

        // seg_cache is positional: inserting/removing a segment shifts every
        // later entry. Detect that structural change before reusing any cache,
        // otherwise old tool lines can appear under the wrong segment.
        let layout: Vec<u64> = self.segments.iter().map(segment_layout_key).collect();
        if layout != self.seg_layout {
            self.seg_cache.clear();
            self.seg_layout = layout;
        }
        self.seg_cache.resize(self.segments.len(), None);

        for idx in 0..self.segments.len() {
            let seg = &self.segments[idx];
            // group spacing rules (cheap, done per assembly pass)
            match seg {
                Segment::User(_) => {
                    in_group = false;
                    last_block = BlockKind::None;
                    logical.push((blank(), None));
                }
                Segment::Assistant { .. } => {
                    if !in_group {
                        logical.push((blank(), None));
                        in_group = true;
                    } else if last_block == BlockKind::ThoughtExpanded {
                        logical.push((blank(), None));
                    }
                    last_block = BlockKind::Answer;
                }
                Segment::Thinking { expanded, .. } => {
                    if !in_group {
                        logical.push((blank(), None));
                        in_group = true;
                    } else if last_block == BlockKind::Answer {
                        logical.push((blank(), None));
                    }
                    last_block = if *expanded {
                        BlockKind::ThoughtExpanded
                    } else {
                        BlockKind::ThoughtCollapsed
                    };
                }
                Segment::Tool { .. } => {
                    // tool rows belong to the agent's turn, keep them grouped
                    if !in_group {
                        logical.push((blank(), None));
                        in_group = true;
                    }
                }
                Segment::Status { .. } => {}
            }

            // expensive part: reuse rendered lines unless the text changed
            let key = self.seg_key(seg);
            let needs_render = match self.seg_cache[idx].as_ref() {
                Some((k, _)) => *k != key,
                None => true,
            };
            if needs_render {
                let lines = self.render_segment(idx, w);
                self.seg_cache[idx] = Some((key, lines));
            }
            let cached = self.seg_cache[idx].as_ref().unwrap();
            logical.extend(cached.1.iter().cloned());
        }
        self.seg_cache.truncate(self.segments.len());

        if self.segments.is_empty() {
            let logo = startup_logo(w, 9);
            logical.clear();
            for line in logo {
                logical.push((Line::from(Span::styled(line, Theme::accent_bold())), None));
            }
        }
        let (lines, rowseg) = wrap_tagged(logical, w);
        self.cache_lines = lines;
        self.cache_rowseg = rowseg;
        self.cache_w = width;
    }

    pub(super) fn chat_top(&self, height: u16) -> usize {
        let h = height.max(1) as usize;
        let max = self.cache_lines.len().saturating_sub(h);
        if self.follow {
            max
        } else {
            self.view_top.min(max)
        }
    }

    pub(super) fn draw(&mut self, f: &mut ratatui::Frame) {
        let area = f.area();
        if area.width < 20 || area.height < 6 {
            return;
        }
        let input_rows = self.input.lines().len().clamp(1, 6) as u16;
        let input_h = (input_rows + 2).min(8);
        let layout = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(input_h),
            Constraint::Length(1),
        ])
        .split(area);
        let chat = Rect {
            x: area.x + 1,
            y: layout[1].y,
            width: area.width.saturating_sub(2),
            height: layout[1].height,
        };

        // expensive rebuild only when something actually changed
        if self.dirty || self.cache_w != area.width {
            self.rebuild_cache(area.width);
        }
        self.last_chat = chat;

        f.render_widget(Block::new().style(Theme::base()), area);

        if self.startup && self.menu_stack.is_empty() {
            let logo = startup_logo(chat.width, chat.height);
            let logo_widget = Paragraph::new(
                logo.into_iter()
                    .map(|line| Line::from(Span::styled(line, Theme::accent_bold())))
                    .collect::<Vec<_>>(),
            )
            .style(Theme::base());
            f.render_widget(logo_widget, chat);
            f.render_widget(self.header_line(), layout[0]);
            let hints = Paragraph::new(Line::from(vec![Span::styled(
                " enter sessions · n new session · q quit",
                Theme::dim(),
            )]))
            .style(Theme::base());
            f.render_widget(hints, layout[3]);
            return;
        }

        let top = self.chat_top(chat.height);
        let sel = self.sel;
        let visible: Vec<Line> = self
            .cache_lines
            .iter()
            .enumerate()
            .skip(top)
            .take(chat.height as usize)
            .map(|(abs, l)| match sel {
                Some(s) if abs >= s.rows().0 && abs <= s.rows().1 => {
                    let chars = line_text(l).chars().count();
                    if chars == 0 {
                        // empty row inside the selection: full-width highlight
                        return Line::from(vec![Span::styled(
                            " ".repeat(chat.width as usize),
                            Style::new().add_modifier(Modifier::REVERSED),
                        )]);
                    }
                    let cs = if abs == s.rows().0 {
                        s.a.col.min(s.b.col).min(chars)
                    } else {
                        0
                    };
                    let ce = if abs == s.rows().1 {
                        s.a.col.max(s.b.col).min(chars)
                    } else {
                        chars
                    };
                    apply_sel(l, cs, ce.max(cs))
                }
                _ => l.clone(),
            })
            .collect();
        f.render_widget(Paragraph::new(visible).style(Theme::base()), chat);

        f.render_widget(self.header_line(), layout[0]);

        self.input.set_block(Self::input_block());
        // the cursor is rendered by tui-textarea as a pink styled cell;
        // the hardware cursor stays hidden
        f.render_widget(&self.input, layout[2]);

        self.status_y = layout[3].y;
        let sb = self.status_bar(area.width);
        f.render_widget(sb, layout[3]);

        self.draw_popup(f, layout[2]);
        self.draw_menu(f, area);
    }

    pub(super) fn draw_menu(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let menu = self.cur_menu().cloned();
        if menu.is_none() {
            return;
        }
        let is_form = self.is_form_menu();
        self.build_menu_rows();

        // fixed extra lines under the content: hint footer and transient status
        let footer_h =
            usize::from(self.menu_footer_text.is_some()) + usize::from(self.menu_status.is_some());
        let avail_inner = (area.height.saturating_sub(6)).max(3) as usize;
        let content_rows: usize = if is_form {
            self.form_fields.len() + 1
        } else {
            // scrollable window; never smaller than what fits
            self.menu_rows
                .len()
                .min(avail_inner - footer_h.min(avail_inner - 1))
        };
        let inner = if is_form {
            content_rows
        } else {
            content_rows + footer_h
        };
        let h = (inner as u16 + 2).clamp(4, area.height.saturating_sub(4));
        let w = 78.min(area.width.saturating_sub(4)).max(30);
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(w)) / 2,
            y: area.y + (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };
        self.menu_rect = rect;

        // keep the selection inside the visible window for list menus
        let list_rows = if is_form { 0 } else { content_rows };
        if !is_form && list_rows > 0 {
            if self.menu_sel < self.menu_scroll {
                self.menu_scroll = self.menu_sel;
            }
            if self.menu_sel >= self.menu_scroll + list_rows {
                self.menu_scroll = self.menu_sel + 1 - list_rows;
            }
            if self.menu_scroll + list_rows > self.menu_rows.len() {
                self.menu_scroll = self.menu_rows.len().saturating_sub(list_rows);
            }
        }

        let mut rows: Vec<Line> = Vec::new();
        let mut focused_field_rect: Option<(usize, Rect)> = None;
        if is_form {
            // column layout: " {label:>12} : " then the value
            let label_w = 16u16;
            for (n, field) in self.form_fields.iter().enumerate() {
                let focused = n == self.form_focus;
                let lstyle = if focused {
                    Theme::accent()
                } else {
                    Theme::dim()
                };
                let prefix = Span::styled(format!(" {:>12} : ", field.label()), lstyle);
                let row_y = rect.y + 1 + n as u16;
                match field {
                    FormField::Text { ta, .. } => {
                        if focused {
                            // value + live cursor are drawn later by
                            // rendering the textarea itself over this row
                            focused_field_rect = Some((
                                n,
                                Rect {
                                    x: rect.x + 1 + label_w,
                                    y: row_y,
                                    width: rect.width.saturating_sub(label_w + 2),
                                    height: 1,
                                },
                            ));
                            rows.push(Line::from(prefix));
                        } else {
                            rows.push(Line::from(vec![
                                prefix,
                                Span::styled(ta.lines().join(""), Theme::base()),
                            ]));
                        }
                    }
                    FormField::Choice { options, sel, .. } => {
                        let vstyle = if focused {
                            Style::new().fg(Theme::FG()).bg(Theme::SURFACE())
                        } else {
                            Theme::base()
                        };
                        let val = options.get(*sel).copied().unwrap_or("");
                        rows.push(Line::from(vec![
                            prefix,
                            Span::styled(format!("‹{val}›"), vstyle),
                        ]));
                    }
                }
            }
            rows.push(Line::from(vec![Span::styled(
                " up/down: field · tab next · left/right: move/change · enter: save · esc: cancel"
                    .to_string(),
                Theme::dim(),
            )]));
        } else {
            // render only the visible window of the list
            for (n, (line, _)) in self
                .menu_rows
                .iter()
                .skip(self.menu_scroll)
                .take(content_rows)
                .enumerate()
            {
                let abs = self.menu_scroll + n;
                if abs == self.menu_sel {
                    rows.push(Line::from(
                        line.spans
                            .iter()
                            .map(|s| {
                                Span::styled(
                                    s.content.to_string(),
                                    s.style
                                        .patch(Style::new())
                                        .fg(Theme::BG())
                                        .bg(Theme::ACCENT())
                                        .add_modifier(Modifier::BOLD),
                                )
                            })
                            .collect::<Vec<_>>(),
                    ));
                } else {
                    rows.push(line.clone());
                }
            }
            if let Some(hint) = &self.menu_footer_text {
                rows.push(Line::from(vec![Span::styled(
                    format!(" {hint}"),
                    Theme::dim(),
                )]));
            }
            if let Some((text, kind)) = &self.menu_status {
                let st = match kind {
                    StatusKind::Info => Theme::dim(),
                    StatusKind::Ok => Theme::ok(),
                    StatusKind::Warn => Theme::warn(),
                    StatusKind::Err => Theme::err(),
                };
                rows.push(Line::from(vec![Span::styled(format!(" {text}"), st)]));
            }
        }

        f.render_widget(Clear, rect);
        f.render_widget(
            Paragraph::new(rows).style(Theme::base()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Theme::border_focused())
                    .title(Span::styled(
                        format!(" {} ", self.menu_title()),
                        Theme::dim(),
                    )),
            ),
            rect,
        );

        // draw the focused text field as a real textarea: same block cursor
        // and editing behavior as the message input
        if let Some((idx, field_rect)) = focused_field_rect
            && let Some(FormField::Text { ta, .. }) = self.form_fields.get_mut(idx)
        {
            f.render_widget(ta.as_ref(), field_rect);
        }
    }

    pub(super) fn draw_popup(&mut self, f: &mut ratatui::Frame, input_area: Rect) {
        if !self.popup_visible() {
            self.popup_rows.clear();
            return;
        }
        let items = self.popup_items();
        if items.is_empty() {
            self.popup_rows.clear();
            return;
        }
        let shown = items
            .len()
            .min(POPUP_MAX_ROWS)
            .min((input_area.y as usize).saturating_sub(2).max(3));
        let max_scroll = items.len().saturating_sub(shown);
        let skip = self.popup_scroll.min(max_scroll);
        let h = (shown + 2) as u16; // + borders
        let w = 64.min(input_area.width.saturating_sub(2)).max(24);
        let y = input_area.y.saturating_sub(h);
        let rect = Rect {
            x: input_area.x,
            y,
            width: w,
            height: h,
        };

        let mut rows: Vec<Line> = Vec::new();
        self.popup_rows.clear();
        for (n, &ci) in items.iter().skip(skip).take(shown).enumerate() {
            let cmd = COMMANDS[ci];
            let hovered = self.hover == Some(ci);
            let cmd_style = if hovered {
                Style::new()
                    .fg(Theme::BG())
                    .bg(Theme::ACCENT())
                    .add_modifier(Modifier::BOLD)
            } else {
                Theme::accent()
            };
            let pad = 1usize;
            rows.push(Line::from(vec![Span::styled(
                format!(" {cmd}{}", " ".repeat(pad)),
                cmd_style,
            )]));
            self.popup_rows.push((rect.y + 1 + n as u16, ci));
        }

        f.render_widget(Clear, rect);
        f.render_widget(
            Paragraph::new(rows).style(Theme::base()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Theme::border_focused()),
            ),
            rect,
        );
        // mini scrollbar on the right border when the list overflows
        if max_scroll > 0 {
            let track = shown;
            let thumb = 1.max(track * shown / items.len());
            let pos = skip * (track - thumb) / max_scroll.max(1);
            let bx = rect.right() - 1;
            for i in 0..track {
                if let Some(cell) = f
                    .buffer_mut()
                    .cell_mut(ratatui::layout::Position::new(bx, rect.y + 1 + i as u16))
                {
                    if i >= pos && i < pos + thumb {
                        cell.set_symbol("▐")
                            .set_style(Style::new().fg(Theme::ACCENT()));
                    }
                }
            }
        }
    }

    pub(super) fn header_line(&self) -> Paragraph<'static> {
        if self.segments.is_empty() {
            return Paragraph::new(Line::from(vec![
                Span::styled(" sqwai", Theme::accent_bold()),
                Span::styled(" · v0.1.0", Theme::dim()),
            ]))
            .style(Theme::base());
        }
        let context_used = self.session.context_tokens_used();
        let pct = self.session.context_percent() as u64;
        let tok = fmt_k(context_used);
        let mut spans = vec![
            Span::styled(" sqwai", Theme::accent_bold()),
            Span::styled(
                format!(" · {}", truncate_chars(&self.session.title, 30)),
                Theme::dim(),
            ),
            // last activity, same format as the sessions menu
            Span::styled(
                format!(" {}", fmt_date(self.session.last_activity())),
                Theme::dim(),
            ),
            Span::styled(
                format!(" · {}@{}", self.model_cfg.id, self.model_cfg.provider),
                Style::new().fg(Theme::ACCENT_SOFT()),
            ),
            Span::styled(format!(" · {tok} tok / {pct}% ctx"), Theme::accent()),
            // cumulative session total — billing/statistics only, kept separate
            // from the live context meter above (don't mix the two sizes)
            Span::styled(
                format!(" · Σ{} tok", fmt_k(self.session.cumulative_tokens())),
                Theme::dim(),
            ),
        ];
        // Only claim prompt-cache savings once the provider has actually
        // reported cached tokens. A documented cache key we never see a hit
        // for stays unverified, so it is not advertised.
        if self.session.cache_confirmed {
            if let Some(c) = self.session.usage.cached_tokens {
                let cp = if context_used > 0 {
                    c.saturating_mul(100).min(context_used) / context_used
                } else {
                    0
                };
                spans.push(Span::styled(format!(" · cache {cp}%"), Theme::dim()));
            }
        }
        // cost meter: enabled later from the settings menu ([ui] show_cost)
        if self.cfg.ui.show_cost
            && let (Some(pi), Some(po)) = (self.model_cfg.price_in, self.model_cfg.price_out)
        {
            let cost = self.session.usage.prompt_tokens as f64 * pi / 1e6
                + self.session.usage.completion_tokens as f64 * po / 1e6;
            spans.push(Span::styled(format!(" · ${cost:.2}"), Theme::accent()));
        }
        if self.streaming {
            spans.push(Span::styled(
                format!(
                    " · {} working",
                    WORKING_SPINNER[self.spinner_tick % WORKING_SPINNER.len()]
                ),
                Theme::accent(),
            ));
        }
        Paragraph::new(Line::from(spans)).style(Theme::base())
    }

    pub(super) fn status_bar(&mut self, w: u16) -> Paragraph<'static> {
        let spans = self.status_bar_spans(w);
        Paragraph::new(Line::from(spans)).style(Theme::base())
    }

    pub(super) fn status_bar_spans(&mut self, w: u16) -> Vec<Span<'static>> {
        // a live retry overrides everything else on the left side
        let (left, left_style) = if let Some(line) = &self.retry_line {
            (format!(" {line}"), Theme::warn())
        } else {
            match &self.bar_error {
                Some(e) => (format!(" err: {}", truncate_chars(e, 60)), Theme::err()),
                None => match &self.last_checkpoint {
                    // reassurance that the undo insurance exists
                    Some(cp) => (
                        format!(" checkpoint: {}", truncate_chars(cp, 44)),
                        Theme::dim(),
                    ),
                    None => (
                        " enter send · tab plan/act · esc stop".to_string(),
                        Theme::dim(),
                    ),
                },
            }
        };
        let dir = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default();

        let th_label = format!(" th:{} ", self.model_cfg.thinking.as_str());
        self.th_click = None;

        // right side: [folder] [th:level] [MODE chip]
        let lsp_label = if self.lsp_diagnostics > 0 {
            format!(" LSP:{} ", self.lsp_diagnostics)
        } else {
            String::new()
        };
        let mut right_len: usize = 1 + th_label.chars().count() + lsp_label.chars().count(); // mode chip always present
        if !dir.is_empty() {
            right_len += truncate_chars(&dir, 20).chars().count() + 1;
        }

        let lw = left.chars().count() as u16;
        let mut spans = vec![Span::styled(left, left_style)];
        let pad = (w as usize).saturating_sub(lw as usize + right_len);
        let th_x0 = lw + pad as u16;
        spans.push(Span::styled(" ".repeat(pad), Theme::base()));
        let th_style = if self.model_cfg.thinking == ThinkingLevel::Off {
            Theme::dim()
        } else {
            Style::new().fg(Theme::ACCENT_SOFT())
        };
        spans.push(Span::styled(th_label.clone(), th_style));
        self.th_click = Some((th_x0, th_x0 + th_label.chars().count() as u16));
        if !lsp_label.is_empty() {
            spans.push(Span::styled(lsp_label, Theme::warn()));
        }
        if !dir.is_empty() {
            spans.push(Span::styled(
                format!("{} ", truncate_chars(&dir, 20)),
                Theme::dim(),
            ));
        }
        let chip = match self.mode {
            Mode::Act => Theme::status_chip(),
            Mode::Plan => Style::new()
                .fg(Theme::BG())
                .bg(Theme::ACCENT_SOFT())
                .add_modifier(Modifier::BOLD),
        };
        spans.push(Span::styled(format!("{} ", self.mode.label()), chip));

        spans
    }
}

#[cfg(test)]
mod tests {
    use super::startup_logo;

    #[test]
    fn startup_logo_keeps_native_size_when_it_fits() {
        let logo = startup_logo(40, 6);
        assert_eq!(logo.len(), 5);
        assert_eq!(logo.iter().map(|line| line.chars().count()).max(), Some(40));
    }

    #[test]
    fn startup_logo_scales_to_width_and_height() {
        let logo = startup_logo(20, 4);
        assert!(logo.len() <= 4);
        assert!(logo.iter().all(|line| line.chars().count() <= 20));
        assert_eq!(logo.len(), 2);
        assert_eq!(logo.iter().map(|line| line.chars().count()).max(), Some(20));
    }

    #[test]
    fn startup_logo_stays_native_size_in_large_terminal() {
        let logo = startup_logo(100, 24);
        assert_eq!(logo.len(), 6);
        assert_eq!(logo.iter().map(|line| line.chars().count()).max(), Some(49));
    }

    #[test]
    fn startup_logo_handles_tiny_terminal() {
        let logo = startup_logo(1, 1);
        assert_eq!(logo.len(), 1);
        assert_eq!(logo[0].chars().count(), 1);
    }
}

fn segment_layout_key(seg: &Segment) -> u64 {
    // Include the stable identifying text, but not the live spinner/text body;
    // normal content changes are handled by seg_key, while insertions/removals
    // must invalidate the entire positional cache.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::mem::discriminant(seg).hash(&mut h);
    match seg {
        Segment::User(text) | Segment::Assistant { text, .. } => text.hash(&mut h),
        Segment::Thinking { .. } => {}
        Segment::Tool { name, args, .. } => {
            name.hash(&mut h);
            args.hash(&mut h);
        }
        Segment::Status { text, kind } => {
            text.hash(&mut h);
            std::mem::discriminant(kind).hash(&mut h);
        }
    }
    h.finish()
}

pub(super) fn blank() -> Line<'static> {
    Line::from(vec![Span::styled(String::new(), Theme::base())])
}

fn dim_all(l: Line<'static>) -> Line<'static> {
    Line::from(
        l.spans
            .into_iter()
            .map(|s| Span::styled(s.content, s.style.patch(Style::new().fg(Theme::DIM()))))
            .collect::<Vec<_>>(),
    )
}

fn line_text(l: &Line<'_>) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Apply reverse-video to the char range [cs, ce) of a logical line.
fn apply_sel(l: &Line<'_>, cs: usize, ce: usize) -> Line<'static> {
    let owned = |l: &Line<'_>| {
        Line::from(
            l.spans
                .iter()
                .map(|s| Span::styled(s.content.to_string(), s.style))
                .collect::<Vec<_>>(),
        )
    };
    if ce <= cs {
        return owned(l);
    }
    let mut out: Vec<Span> = Vec::new();
    let mut pos = 0usize;
    for span in &l.spans {
        let len = span.content.chars().count();
        let s0 = pos;
        let s1 = pos + len;
        pos = s1;
        if s1 <= cs || s0 >= ce {
            out.push(Span::styled(span.content.to_string(), span.style));
            continue;
        }
        // split this span into before/inside/after
        let chars: Vec<char> = span.content.chars().collect();
        let inside_start = cs.saturating_sub(s0).min(len);
        let inside_end = ce.saturating_sub(s0).min(len);
        let before: String = chars[..inside_start].iter().collect();
        let inside: String = chars[inside_start..inside_end].iter().collect();
        let after: String = chars[inside_end..].iter().collect();
        if !before.is_empty() {
            out.push(Span::styled(before, span.style));
        }
        out.push(Span::styled(
            inside,
            span.style.add_modifier(Modifier::REVERSED),
        ));
        if !after.is_empty() {
            out.push(Span::styled(after, span.style));
        }
    }
    Line::from(out)
}
