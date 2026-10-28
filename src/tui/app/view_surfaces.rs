//! Bottom surfaces, modal menus, command popups, and status bar rendering.

#![allow(unused_imports)]
use super::forms::FormField;
use super::menus::{Menu, MenuAction};
use super::view::*;
use super::*;

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use crate::config::EffortLevel;
use crate::tui::theme::Theme;

impl App {
    pub(super) fn form_hover_band(
        line: Line<'static>,
        hovered: bool,
        content_w: usize,
    ) -> Line<'static> {
        if !hovered {
            return line;
        }
        let mut out = Line::from(
            line.spans
                .into_iter()
                .map(|s| {
                    Span::styled(
                        s.content.to_string(),
                        s.style.patch(Style::new().bg(Theme::SELECTION_BG())),
                    )
                })
                .collect::<Vec<_>>(),
        );
        let pad = content_w.saturating_sub(cols(&line_text(&out)));
        if pad > 0 {
            out.spans.push(Span::styled(
                " ".repeat(pad),
                Style::new().bg(Theme::SELECTION_BG()),
            ));
        }
        out
    }

    pub(super) fn draw_menu(&mut self, buf: &mut Buffer, area: Rect) {
        let menu = self.cur_menu().cloned();
        if menu.is_none() || area.height < 8 || area.width < 20 {
            return;
        }
        if matches!(
            menu,
            Some(Menu::Effort { .. })
                | Some(Menu::Context)
                | Some(Menu::ContextView)
                | Some(Menu::Help)
                | Some(Menu::TestAnim)
                | Some(Menu::TestArt)
                | Some(Menu::TestColors)
                | Some(Menu::TestMd)
                | Some(Menu::Settings)
                | Some(Menu::Debug)
                | Some(Menu::Plan)
                | Some(Menu::EditScalar(..))
                | Some(Menu::UndoShadow)
                | Some(Menu::UndoList)
                | Some(Menu::Providers)
                | Some(Menu::EditProvider { .. })
                | Some(Menu::EditProviderField { .. })
                | Some(Menu::ProviderFormat { .. })
                | Some(Menu::Models { .. })
                | Some(Menu::EditModel { .. })
                | Some(Menu::EditModelField { .. })
                | Some(Menu::Sessions)
                | Some(Menu::EditSessionTitle { .. })
                | Some(Menu::ConfirmDelete { .. })
                | Some(Menu::Approval { .. })
                | Some(Menu::ConfirmFullAccess)
                | Some(Menu::Skills)
                | Some(Menu::SkillView { .. })
        ) {
            return;
        }
        let is_form = self.is_form_menu();
        let now = std::time::Instant::now();
        let stale = self
            .menu_built_at
            .is_none_or(|t| now.duration_since(t) >= std::time::Duration::from_millis(500));
        if stale {
            self.build_menu_rows();
            self.menu_built_at = Some(now);
        }

        self.effort_hits.clear();
        self.menu_footer_text = None;
        let footer_h = 0usize;
        let has_hints = matches!(
            menu,
            Some(Menu::Sessions)
                | Some(Menu::Context)
                | Some(Menu::TestAnim)
                | Some(Menu::TestArt)
                | Some(Menu::TestColors)
                | Some(Menu::TestMd)
                | Some(Menu::EditProvider { .. })
                | Some(Menu::EditModel { .. })
                | Some(Menu::EditSessionTitle { .. })
                | Some(Menu::EditScalar(..))
        );
        let chrome_h = if is_form {
            usize::from(has_hints)
        } else {
            footer_h + usize::from(self.menu_table_header.is_some()) + 1 + usize::from(has_hints)
        };
        let avail_inner = (area.height.saturating_sub(6)).max(3) as usize;
        let max_h = area.height.saturating_sub(4).max(4);
        let foot_len = self.menu_sticky_footer.len();
        let foot_total = foot_len + usize::from(foot_len > 0);
        let full_chrome = chrome_h + foot_total;
        let max_items = (max_h as usize)
            .saturating_sub(2)
            .saturating_sub(full_chrome)
            .max(1);
        let content_rows: usize = if is_form {
            self.form_fields.len() + 1
        } else {
            let body = self
                .menu_rows
                .len()
                .min(avail_inner.saturating_sub(full_chrome).max(1))
                .min(max_items);
            body.max(usize::from(!self.menu_rows.is_empty()))
        };
        let inner = content_rows + full_chrome;
        let h = (inner as u16 + 2).clamp(4, max_h);
        let w = 78.min(area.width.saturating_sub(4)).max(30).min(area.width);
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(w)) / 2,
            y: area.y + (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };
        self.menu_rect = rect;

        if matches!(menu, Some(Menu::Sessions) | Some(Menu::Providers)) {
            let want = super::menus::table_budget(rect.width) as u16;
            if want != self.table_built_w {
                self.build_menu_rows();
                self.table_built_w = want;
            }
        }
        if matches!(menu, Some(Menu::TestMd)) && self.md_built_w != rect.width {
            self.build_menu_rows();
            self.md_built_w = rect.width;
        }
        if matches!(menu, Some(Menu::Context)) && self.ctx_built_w != rect.width {
            self.build_menu_rows();
            self.ctx_built_w = rect.width;
        }

        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    let mut style = cell.style().add_modifier(Modifier::DIM);
                    if let Some(bg) = style.bg {
                        style.bg = Some(dim_bg(bg));
                    }
                    cell.set_style(style);
                }
            }
        }

        let list_rows = if is_form { 0 } else { content_rows };
        self.menu_visible_rows = list_rows;
        if !is_form && list_rows > 0 && self.menu_scroll + list_rows > self.menu_rows.len() {
            self.menu_scroll = self.menu_rows.len().saturating_sub(list_rows);
        }

        let mut rows: Vec<Line> = Vec::new();
        if !is_form {
            rows.push(Line::default());
        }
        if let Some(header) = self.menu_table_header.clone() {
            rows.push(header);
        }
        let mut focused_field_rect: Option<(usize, Rect)> = None;
        if is_form {
            rows.push(Line::default());
            let label_w = self.form_label_w();
            let inner_w = (label_w as usize).saturating_sub(4);
            for (n, field) in self.form_fields.iter().enumerate() {
                let focused = n == self.form_focus;
                let hovered = matches!(self.cur_menu(), Some(Menu::EditModel { .. }))
                    && Some(n) == self.form_hover_row;
                let lstyle = Style::new().fg(Color::White);
                let prefix = Span::styled(format!(" {:>inner_w$} : ", field.label()), lstyle);
                let row_y = rect.y + 2 + n as u16;
                match field {
                    FormField::Text {
                        ta,
                        secret,
                        revealed,
                        ..
                    } => {
                        let shown = if *secret && !*revealed {
                            FormField::mask_value(&ta.lines().join(""))
                        } else {
                            ta.lines().join("")
                        };
                        if focused {
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
                            rows.push(Self::form_hover_band(
                                Line::from(vec![prefix, Span::styled(shown, Theme::base())]),
                                hovered,
                                rect.width.saturating_sub(2) as usize,
                            ));
                        }
                    }
                    FormField::Choice { options, sel, .. } => {
                        let val = options.get(*sel).copied().unwrap_or("");
                        let spans = vec![prefix, Span::styled(val.to_string(), Theme::base())];
                        rows.push(Self::form_hover_band(
                            Line::from(spans),
                            hovered,
                            rect.width.saturating_sub(2) as usize,
                        ));
                    }
                }
            }
        } else {
            let content_w = rect.width.saturating_sub(2) as usize;
            for (n, (line, _)) in self
                .menu_rows
                .iter()
                .skip(self.menu_scroll)
                .take(content_rows)
                .enumerate()
            {
                let abs = self.menu_scroll + n;
                if abs == self.menu_sel && !self.menu_is_dead() {
                    let mut out = Line::from(
                        line.spans
                            .iter()
                            .map(|s| {
                                Span::styled(
                                    s.content.to_string(),
                                    s.style.patch(Theme::selection()),
                                )
                            })
                            .collect::<Vec<_>>(),
                    );
                    let pad = content_w.saturating_sub(cols(&line_text(&out)));
                    if pad > 0 {
                        out.spans.push(Span::styled(
                            " ".repeat(pad),
                            Style::new().bg(Theme::SELECTION_BG()),
                        ));
                    }
                    rows.push(out);
                } else {
                    rows.push(line.clone());
                }
            }
        }
        if !is_form && !self.menu_sticky_footer.is_empty() {
            let sep_w = rect.width.saturating_sub(2) as usize;
            rows.push(Line::from(vec![Span::styled(
                "─".repeat(sep_w),
                Theme::rule_color(),
            )]));
            let content_w = rect.width.saturating_sub(2) as usize;
            let base = self.menu_rows.len();
            for (f, (line, _)) in self.menu_sticky_footer.iter().enumerate() {
                if base + f == self.menu_sel {
                    let mut out = Line::from(
                        line.spans
                            .iter()
                            .map(|s| {
                                Span::styled(
                                    s.content.to_string(),
                                    s.style.patch(Theme::selection()),
                                )
                            })
                            .collect::<Vec<_>>(),
                    );
                    let pad = content_w.saturating_sub(cols(&line_text(&out)));
                    if pad > 0 {
                        out.spans.push(Span::styled(
                            " ".repeat(pad),
                            Style::new().bg(Theme::SELECTION_BG()),
                        ));
                    }
                    rows.push(out);
                } else {
                    rows.push(line.clone());
                }
            }
        }

        let mut block = Block::default()
            .padding(ratatui::widgets::Padding::horizontal(1))
            .title_alignment(Alignment::Center)
            .title(Span::styled(
                format!(" {} ", self.menu_title()),
                crate::tui::art::h2_style(),
            ));
        if matches!(self.cur_menu(), Some(Menu::Sessions)) {
            block = block.title_bottom(
                Theme::hints(&[
                    ("ctrl+p", "pin"),
                    ("ctrl+d", "delete"),
                    ("ctrl+r", "rename"),
                ])
                .right_aligned(),
            );
        } else if matches!(
            self.cur_menu(),
            Some(Menu::TestAnim)
                | Some(Menu::TestArt)
                | Some(Menu::TestColors)
                | Some(Menu::TestMd)
        ) {
            block = block.title_bottom(Theme::hints(&[("enter/esc", "close")]).right_aligned());
        } else if matches!(self.cur_menu(), Some(Menu::EditProvider { .. })) {
            block = block.title_bottom(
                Theme::hints(&[
                    ("enter", "save"),
                    ("esc", "cancel"),
                    ("ctrl+t", "show/hide key"),
                ])
                .right_aligned(),
            );
        } else if matches!(
            self.cur_menu(),
            Some(
                Menu::EditModel { .. }
                    | Menu::EditSessionTitle { .. }
                    | Menu::EditScalar(..)
            )
        ) {
            block = block.title_bottom(
                Theme::hints(&[("enter", "save"), ("esc", "cancel")]).right_aligned(),
            );
        }
        if has_hints {
            rows.push(Line::default());
        }
        Clear.render(rect, buf);
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    let mut style = cell.style();
                    style.bg = Some(Theme::MENU_BG());
                    cell.set_style(style);
                }
            }
        }
        Paragraph::new(rows)
            .style(Theme::base())
            .block(block)
            .render(rect, buf);
        if !is_form && !self.menu_rows.is_empty() {
            let total = self.menu_rows.len();
            let shown = total
                .saturating_sub(self.menu_scroll)
                .min(content_rows)
                .max(1);
            let max_scroll = total.saturating_sub(content_rows);
            if max_scroll > 0 {
                let thumb = 1.max(shown * shown / total);
                let pos = self.menu_scroll * (shown - thumb) / max_scroll.max(1);
                let bx = rect.right().saturating_sub(2);
                let top = rect.y + 2 + usize::from(self.menu_table_header.is_some()) as u16;
                for i in 0..shown {
                    if let Some(cell) = buf
                        .cell_mut(ratatui::layout::Position::new(bx, top + i as u16))
                        && i >= pos
                        && i < pos + thumb
                        && !matches!(
                            cell.symbol(),
                            "│" | "─" | "┌" | "┐" | "└" | "┘" | "├" | "┤" | "┬" | "┴" | "┼"
                        )
                    {
                        cell.set_symbol("▐")
                            .set_style(Style::new().fg(Theme::rule_color()));
                    }
                }
            }
        }

        if let Some((idx, field_rect)) = focused_field_rect
            && let Some(FormField::Text {
                ta,
                secret,
                revealed,
                ..
            }) = self.form_fields.get_mut(idx)
        {
            if *secret && !*revealed {
                let masked = FormField::mask_value(&ta.lines().join(""));
                let count = masked.chars().count();
                let ccol = ta.cursor().1.min(count);
                let mut plain = Style::new().fg(Theme::FG());
                if Some(idx) == self.form_hover_row {
                    plain = plain.bg(Theme::SELECTION_BG());
                }
                let cursor_style = Style::new().fg(Color::Black).bg(Color::White);
                let mut spans = Vec::with_capacity(count + 1);
                for (i, ch) in masked.chars().enumerate() {
                    let st = if i == ccol { cursor_style } else { plain };
                    spans.push(Span::styled(ch.to_string(), st));
                }
                if ccol == count {
                    spans.push(Span::styled(" ".to_string(), cursor_style));
                }
                Paragraph::new(Line::from(spans)).render(field_rect, buf);
            } else {
                let mut style = Style::new().fg(Theme::FG());
                if Some(idx) == self.form_hover_row {
                    style = style.bg(Theme::SELECTION_BG());
                }
                ta.set_style(style);
                ta.as_ref().render(field_rect, buf);
            }
        }
    }

    pub(super) fn draw_effort_slider(
        &mut self,
        buf: &mut Buffer,
        area: Rect,
        skip: u16,
        full_h: u16,
    ) {
        const COL_W: u16 = 8;
        let n = EffortLevel::SELECTABLE.len();
        let sel = self.menu_sel.min(n.saturating_sub(1));
        let active = EffortLevel::SELECTABLE[sel];
        let now = std::time::Instant::now();
        let endpoint = Theme::effort_rgb(active);
        if self.effort_blend_sel != Some(sel) {
            if let Some(old) = self.effort_blend_sel
                && let Some(old_lvl) = EffortLevel::SELECTABLE.get(old)
            {
                let old_to = Theme::effort_rgb(*old_lvl);
                let from = match &self.effort_blend {
                    Some((f, t0)) => {
                        let el = now.duration_since(*t0).as_millis();
                        if el >= MODE_BLEND_MS as u128 {
                            old_to
                        } else {
                            crate::tui::shimmer::blend(*f, old_to, el as f64 / MODE_BLEND_MS as f64)
                        }
                    }
                    None => old_to,
                };
                self.effort_blend = Some((from, now));
            }
            self.effort_blend_sel = Some(sel);
        }
        let ink = match &self.effort_blend {
            Some((f, t0)) if now.duration_since(*t0).as_millis() < MODE_BLEND_MS as u128 => {
                let el = now.duration_since(*t0).as_millis();
                let (r, g, b) =
                    crate::tui::shimmer::blend(*f, endpoint, el as f64 / MODE_BLEND_MS as f64);
                Color::Rgb(r, g, b)
            }
            _ => Theme::effort_color(active),
        };
        let ink = Style::new().fg(ink);

        let rect = area;
        self.menu_rect = rect;
        self.effort_hits.clear();
        let row_y = |k: u16| {
            let y = k.checked_sub(skip).map(|r| rect.y + r)?;
            (y < rect.bottom()).then_some(y)
        };
        let avail = rect.width.saturating_sub(2);
        let col_w = (avail / n.max(1) as u16).clamp(4, COL_W);
        let inner_w = (col_w * n as u16).min(avail);
        let base_x = rect.x + 1 + (avail.saturating_sub(inner_w)) / 2;
        if let Some(y) = row_y(0) {
            buf.set_line(
                rect.x,
                y,
                &Line::from(Span::styled(
                    "─".repeat(rect.width as usize),
                    Theme::brand_ink(),
                )),
                rect.width,
            );
        }
        let committed = self.effort_committed();
        if let Some(y) = row_y(1) {
            buf.set_line(
                rect.x,
                y,
                &Line::from(Span::styled(
                    format!(" {}", self.menu_title()),
                    crate::tui::art::h2_style(),
                )),
                rect.width,
            );
            if rect.width >= 40 {
                Paragraph::new(
                    Line::from(Span::styled(format!("{}", committed.as_str()), Theme::dim()))
                        .right_aligned(),
                )
                .render(
                    Rect {
                        x: rect.x,
                        y,
                        width: rect.width.saturating_sub(1),
                        height: 1,
                    },
                    buf,
                );
            }
        }
        let Some(labels_y) = row_y(3) else {
            return;
        };
        let Some(track_y) = row_y(4) else {
            return;
        };
        if let Some(y) = row_y(full_h.saturating_sub(1)) {
            Paragraph::new(
                Theme::hints(&[("←/→", "move"), ("enter", "confirm"), ("esc", "cancel")])
                    .right_aligned(),
            )
            .render(
                Rect {
                    x: rect.x,
                    y,
                    width: rect.width.saturating_sub(1),
                    height: 1,
                },
                buf,
            );
        }

        for (i, lvl) in EffortLevel::SELECTABLE.iter().enumerate() {
            let name = lvl.as_str();
            let col_x = base_x + col_w * i as u16;
            let name_w = name.len() as u16;
            let pad = col_w.saturating_sub(name_w) / 2;
            let style = if i == sel { ink } else { Theme::dim() };
            Paragraph::new(Line::from(vec![
                Span::styled(" ".repeat(pad as usize), Theme::base()),
                Span::styled(name.to_string(), style),
            ]))
            .render(
                Rect {
                    x: col_x,
                    y: labels_y,
                    width: col_w,
                    height: 1,
                },
                buf,
            );
            self.effort_hits.push((
                Rect {
                    x: col_x,
                    y: labels_y,
                    width: col_w,
                    height: 2,
                },
                i,
            ));
        }

        let total = col_w as usize * n;
        let dot_off = (col_w / 2) as usize;
        let dim = Theme::dim();
        let mut cells: Vec<(&str, Style)> = vec![(" ", Theme::base()); total];
        for i in 0..n {
            let dx = i * col_w as usize + dot_off;
            let (dot, dot_style) = if i <= sel { ("●", ink) } else { ("○", dim) };
            cells[dx] = (dot, dot_style);
            if i + 1 < n {
                let cstyle = if i < sel { ink } else { dim };
                for c in cells
                    .iter_mut()
                    .take((i + 1) * col_w as usize + dot_off)
                    .skip(dx + 1)
                {
                    *c = ("─", cstyle);
                }
            }
        }
        let mut spans: Vec<Span> = Vec::new();
        for (glyph, style) in cells {
            let same = spans
                .last()
                .map(|s: &Span| s.style == style)
                .unwrap_or(false);
            if same {
                let last: &mut Span = spans.last_mut().expect("nonempty");
                last.content.to_mut().push_str(glyph);
            } else {
                spans.push(Span::styled(glyph.to_string(), style));
            }
        }
        Paragraph::new(Line::from(spans)).render(
            Rect {
                x: base_x,
                y: track_y,
                width: inner_w,
                height: 1,
            },
            buf,
        );
    }

    pub(super) fn draw_context_surface(
        &mut self,
        buf: &mut Buffer,
        area: Rect,
        skip: u16,
        full_h: u16,
    ) {
        self.effort_hits.clear();
        self.menu_rect = area;
        const CARD_W: u16 = 78;
        let card_w = area.width.saturating_sub(2).min(CARD_W);
        let stale = self
            .menu_built_at
            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_millis(500));
        if self.ctx_built_w != card_w || stale {
            self.build_menu_rows();
            self.ctx_built_w = card_w;
            self.menu_built_at = Some(std::time::Instant::now());
        }
        let row_y = |k: u16| {
            let y = k.checked_sub(skip).map(|r| area.y + r)?;
            (y < area.bottom()).then_some(y)
        };
        const CARD0: u16 = 3;
        if let Some(y) = row_y(0) {
            buf.set_line(
                area.x,
                y,
                &Line::from(Span::styled(
                    "─".repeat(area.width as usize),
                    Theme::brand_ink(),
                )),
                area.width,
            );
        }
        if let Some(y) = row_y(1) {
            buf.set_line(
                area.x,
                y,
                &Line::from(Span::styled(
                    format!(" {}", self.menu_title()),
                    crate::tui::art::h2_style(),
                )),
                area.width,
            );
        }
        let first = (skip.saturating_sub(CARD0) as usize).min(self.menu_rows.len());
        if let Some(y) = row_y(CARD0 + first as u16) {
            for (idx, (line, _)) in self
                .menu_rows
                .iter()
                .skip(first)
                .take((area.bottom() - y) as usize)
                .enumerate()
            {
                let w = (cols(&line_text(line)) as u16).min(area.width);
                if w == 0 {
                    continue;
                }
                let x = area.x + area.width.saturating_sub(w) / 2;
                Paragraph::new(line.clone()).render(
                    Rect {
                        x,
                        y: y + idx as u16,
                        width: w,
                        height: 1,
                    },
                    buf,
                );
            }
        }
        if let Some(y) = row_y(full_h.saturating_sub(1)) {
            Paragraph::new(Theme::hints(&[("esc", "close")]).right_aligned()).render(
                Rect {
                    x: area.x,
                    y,
                    width: area.width.saturating_sub(1),
                    height: 1,
                },
                buf,
            );
        }
    }

    pub(super) fn list_shape(&self, screen_h: u16) -> (usize, usize, &'static str) {
        let dyn_win = (screen_h as usize / 2).saturating_sub(6).max(8);
        match self.cur_menu() {
            Some(Menu::Settings) => {
                let body = self.menu_rows.len().saturating_sub(1);
                let fit = (screen_h as usize).saturating_sub(5);
                let full_win = if body > fit {
                    (screen_h as usize).saturating_sub(6).max(1)
                } else {
                    fit.max(1)
                };
                (1, full_win, "settings")
            }
            Some(
                Menu::Debug
                | Menu::Plan
                | Menu::Help
                | Menu::ContextView
                | Menu::TestAnim
                | Menu::TestArt
                | Menu::TestColors
                | Menu::TestMd
                | Menu::SkillView { .. },
            ) => {
                let body = self.menu_rows.len();
                let fit = (screen_h as usize).saturating_sub(4);
                let full_win = if body > fit {
                    (screen_h as usize).saturating_sub(5).max(1)
                } else {
                    fit.max(1)
                };
                (0, full_win, "lines")
            }
            Some(Menu::UndoShadow) => (0, 3, ""),
            Some(Menu::Providers) => (1, dyn_win, "providers"),
            Some(Menu::EditProvider { .. }) => (0, 5, ""),
            Some(Menu::ProviderFormat { .. }) => (0, 3, ""),
            Some(Menu::Models { .. }) => (1, dyn_win, "models"),
            Some(Menu::EditModel { .. }) => (0, 4, ""),
            Some(Menu::Sessions) => (1, dyn_win, "sessions"),
            Some(Menu::Skills) => (1, dyn_win, "skills"),
            Some(Menu::ConfirmDelete { .. }) => (2, 2, ""),
            Some(Menu::ConfirmFullAccess) => (self.menu_rows.len().saturating_sub(2), 2, ""),
            Some(Menu::Approval { .. }) => (self.menu_rows.len().saturating_sub(3), 3, ""),
            Some(
                Menu::EditModelField { .. }
                | Menu::EditProviderField { .. }
                | Menu::EditSessionTitle { .. }
                | Menu::EditScalar(..),
            ) => (0, 1, ""),
            _ => (0, dyn_win, ""),
        }
    }

    pub(super) fn list_hints(&self) -> Vec<(&'static str, String)> {
        match self.cur_menu() {
            Some(Menu::Settings) => vec![
                ("enter", "change".to_string()),
                ("esc", "back".to_string()),
            ],
            Some(Menu::Debug) => vec![
                ("enter", "run".to_string()),
                ("esc", "back".to_string()),
            ],
            Some(Menu::Plan | Menu::Help) => vec![("esc", "close".to_string())],
            Some(
                Menu::ContextView
                | Menu::TestAnim
                | Menu::TestArt
                | Menu::TestColors
                | Menu::TestMd
                | Menu::SkillView { .. },
            ) => vec![
                ("up/down", "scroll".to_string()),
                ("esc", "close".to_string()),
            ],
            Some(Menu::Skills) => vec![
                ("ctrl+o", "view".to_string()),
                ("ctrl+r", "reset".to_string()),
                ("enter", "toggle".to_string()),
                ("esc", "back".to_string()),
            ],
            Some(Menu::Providers) => vec![
                ("ctrl+n", "add".to_string()),
                ("ctrl+u", "update".to_string()),
                ("ctrl+e", "edit".to_string()),
                ("ctrl+d", "delete".to_string()),
                ("enter", "open".to_string()),
                ("esc", "back".to_string()),
            ],
            Some(Menu::Models { .. }) => vec![
                ("ctrl+n", "add".to_string()),
                ("ctrl+e", "edit".to_string()),
                ("ctrl+t", "test".to_string()),
                ("ctrl+d", "delete".to_string()),
                ("enter", "use".to_string()),
                ("esc", "back".to_string()),
            ],
            Some(Menu::Sessions) => vec![
                ("ctrl+r", "rename".to_string()),
                ("ctrl+p", "pin".to_string()),
                ("ctrl+d", "delete".to_string()),
                ("enter", "open".to_string()),
                ("esc", "back".to_string()),
            ],
            Some(Menu::EditProvider { .. } | Menu::EditModel { .. }) => vec![
                ("enter", "edit".to_string()),
                ("esc", "back".to_string()),
            ],
            Some(
                Menu::ProviderFormat { .. }
                | Menu::UndoShadow
                | Menu::UndoList,
            ) => vec![
                ("enter", "use".to_string()),
                ("esc", "back".to_string()),
            ],
            Some(Menu::ConfirmDelete { .. }) => vec![
                ("enter", "confirm".to_string()),
                ("esc", "cancel".to_string()),
            ],
            Some(Menu::ConfirmFullAccess) => vec![
                ("enter", "confirm".to_string()),
                ("esc", "cancel".to_string()),
            ],
            Some(Menu::Approval { .. }) => vec![
                ("enter", "confirm".to_string()),
                ("esc", "deny".to_string()),
            ],
            Some(
                Menu::EditModelField { .. }
                | Menu::EditProviderField { .. }
                | Menu::EditSessionTitle { .. }
                | Menu::EditScalar(..),
            ) => vec![
                ("enter", "save".to_string()),
                ("esc", "back".to_string()),
            ],
            _ => vec![],
        }
    }

    pub(super) fn surface_badge(&self) -> Option<(String, Style)> {
        let provider = match self.cur_menu()? {
            Menu::Models { provider } => provider,
            _ => return None,
        };
        match self.provider_checks.get(provider.as_str()) {
            None => None,
            Some(ProviderCheck::Checking) => Some(("checking…".to_string(), Theme::dim())),
            Some(ProviderCheck::Ok(detail)) => Some((
                format!("ok ({detail})"),
                Theme::ok().add_modifier(Modifier::BOLD),
            )),
            Some(ProviderCheck::Err(reason)) => {
                let mut cut = String::new();
                for w in reason.split_whitespace() {
                    let next = if cut.is_empty() {
                        w.to_string()
                    } else {
                        format!("{cut} {w}")
                    };
                    if next.chars().count() > 24 {
                        break;
                    }
                    cut = next;
                }
                if cut.is_empty() {
                    cut = reason.chars().take(24).collect();
                }
                Some((
                    format!("refused: {cut}"),
                    Theme::err().add_modifier(Modifier::BOLD),
                ))
            }
        }
    }

    pub(super) fn draw_list_surface(
        &mut self,
        buf: &mut Buffer,
        area: Rect,
        off: u16,
        full_h: u16,
    ) {
        let hints = self.list_hints();
        let hint_start = full_h.saturating_sub(hints.len() as u16);
        let row_rail_at = |k: u16| -> u16 {
            if k >= hint_start {
                hints
                    .get((k - hint_start) as usize)
                    .map(|(key, what)| (key.chars().count() + what.chars().count() + 5) as u16)
                    .unwrap_or(0)
            } else {
                0
            }
        };
        let avail_w = area.width.saturating_sub(4).max(20);
        if matches!(
            self.cur_menu(),
            Some(Menu::ContextView | Menu::TestMd | Menu::SkillView { .. })
        ) && self.md_built_w != avail_w
        {
            self.md_built_w = avail_w;
            self.build_menu_rows();
        }
        let (head, win, more) = self.list_shape(buf.area.height);
        self.menu_rect = area;
        self.effort_hits.clear();
        self.surf_hits.clear();
        self.popup_rows.clear();
        const LIST0: u16 = 3;
        let body0 = LIST0 + head as u16;
        let row_y = |k: u16| {
            let y = k.checked_sub(off).map(|r| area.y + r)?;
            (y < area.bottom()).then_some(y)
        };
        if let Some(y) = row_y(0) {
            buf.set_line(
                area.x,
                y,
                &Line::from(Span::styled(
                    "─".repeat(area.width as usize),
                    Theme::brand_ink(),
                )),
                area.width,
            );
        }
        if let Some(y) = row_y(1) {
            let title = format!(" {}", self.menu_title());
            buf.set_line(
                area.x,
                y,
                &Line::from(Span::styled(title.clone(), crate::tui::art::h2_style())),
                area.width,
            );
            if let Some((badge, ink)) = self.surface_badge()
                && (area.width as usize) >= title.chars().count() + badge.chars().count() + 3
            {
                Paragraph::new(Line::from(Span::styled(badge, ink)).right_aligned()).render(
                    Rect {
                        x: area.x,
                        y,
                        width: area.width.saturating_sub(1),
                        height: 1,
                    },
                    buf,
                );
            }
        }
        // The air row under the title carries whatever the bottom bar would
        // say: a form that refuses a key, a retry, an expired notice. Without
        // this the surface owns the screen and those messages go unseen.
        if let Some(line) = self.notice_line(area.width)
            && let Some(y) = row_y(2)
        {
            buf.set_line(area.x, y, &line, area.width);
        }
        let body = self.menu_rows.len().saturating_sub(head);
        let shown = body.min(win).max(1);
        let mut top = self.surf_top.min(body.saturating_sub(shown));
        if !self.menu_is_dead() {
            if self.menu_sel >= head {
                let i = self.menu_sel - head;
                if i < top {
                    top = i;
                    while top > 0
                        && matches!(
                            self.menu_rows.get(head + top - 1).map(|(_, a)| a),
                            Some(MenuAction::None)
                        )
                    {
                        top -= 1;
                    }
                } else if i >= top + shown {
                    top = i + 1 - shown;
                }
            } else {
                top = 0;
            }
        }
        self.surf_top = top;
        fn paint_field(slf: &mut App, buf: &mut Buffer, area: Rect, y: u16, rail: u16) {
            let q = slf.surf_field().lines().join("");
            buf.set_line(
                area.x + 2,
                y,
                &Line::from(Span::styled("⌕ ", Theme::dim())),
                area.width.saturating_sub(2 + rail),
            );
            let field = Rect {
                x: area.x + 4,
                y,
                width: area.width.saturating_sub(6 + rail),
                height: 1,
            };
            if !slf.surf_focused() {
                Paragraph::new(Line::from(Span::styled(
                    if q.is_empty() {
                        "Search".to_string()
                    } else {
                        q
                    },
                    Theme::dim(),
                )))
                .render(field, buf);
                return;
            }
            if q.is_empty() {
                buf.set_line(
                    field.x,
                    y,
                    &Line::from(Span::styled("Search", Theme::dim())),
                    area.width.saturating_sub(4 + rail),
                );
            }
            slf.surf_field().render(field, buf);
            if q.is_empty()
                && let Some(cell) = buf.cell_mut((field.x, field.y))
            {
                cell.set_symbol("S");
            }
        }
        let paint = |i: usize, k: u16, slf: &mut Self, buf: &mut Buffer| {
            let Some(y) = row_y(k) else {
                return;
            };
            let Some((line, action)) = slf.menu_rows.get(i).cloned() else {
                return;
            };
            let rail = row_rail_at(k);
            let info = matches!(action, MenuAction::None);
            let is_field_surf = matches!(
                slf.cur_menu(),
                Some(
                    Menu::EditModelField { .. }
                        | Menu::EditProviderField { .. }
                        | Menu::EditSessionTitle { .. }
                        | Menu::EditScalar(..)
                )
            );
            let cur = (!info && slf.menu_sel == i) || is_field_surf;
            buf.set_line(
                area.x,
                y,
                &Line::from(Span::styled(
                    if cur { "› " } else { "  " },
                    if cur {
                        Theme::brand_ink()
                    } else {
                        Style::new().add_modifier(Modifier::DIM)
                    },
                )),
                area.width,
            );
            if is_field_surf {
                if let Some(FormField::Text {
                    ta,
                    secret,
                    revealed,
                    ..
                }) = slf.form_fields.first()
                {
                    let field_rect = Rect {
                        x: area.x + 2,
                        y,
                        width: area.width.saturating_sub(2 + rail),
                        height: 1,
                    };
                    if *secret && !*revealed {
                        let masked = FormField::mask_value(&ta.lines().join(""));
                        Paragraph::new(Line::from(Span::styled(masked, Theme::base())))
                            .render(field_rect, buf);
                        let col = (ta.cursor().1 as u16).min(field_rect.width.saturating_sub(1));
                        if field_rect.width > 0
                            && let Some(cell) = buf.cell_mut((field_rect.x + col, field_rect.y))
                        {
                            cell.set_style(Theme::cursor());
                        }
                    } else {
                        ta.render(field_rect, buf);
                    }
                }
            } else if matches!(action, MenuAction::FocusSearch) {
                paint_field(slf, buf, area, y, rail);
            } else {
                let avail = area.width.saturating_sub(2 + rail) as usize;
                let mut spans: Vec<Span> = if i == 0
                    && head == 2
                    && let Some(Menu::Approval {
                        subject, reason, ..
                    }) = slf.cur_menu()
                {
                    let mut s =
                        crate::tui::shimmer::shimmer_warn_spans(subject, slf.spinner_tick);
                    s.push(Span::styled(format!(" ({reason})"), Theme::dim()));
                    s
                } else if i < head.saturating_sub(2)
                    && matches!(slf.cur_menu(), Some(Menu::Approval { .. }))
                {
                    let row_text: String =
                        line.spans.iter().map(|sp| sp.content.as_ref()).collect();
                    crate::tui::shimmer::shimmer_warn_spans(&row_text, slf.spinner_tick)
                } else if i == 0
                    && let Some(Menu::ConfirmDelete { label, .. }) = slf.cur_menu()
                {
                    crate::tui::shimmer::shimmer_warn_spans(label, slf.spinner_tick)
                } else {
                    let mut s = Vec::new();
                    for mut sp in line.spans {
                        if cur {
                            sp.style = sp.style.add_modifier(Modifier::BOLD);
                        }
                        s.push(sp);
                    }
                    s
                };
                if matches!(action, MenuAction::OpenSession(_)) && !spans.is_empty() {
                    let title_cap = (avail / 2).clamp(16, 48).min(avail);
                    spans[0].content =
                        truncate_display_width(spans[0].content.as_ref(), title_cap).into();
                }
                if spans.len() == 2
                    && spans[1].content.starts_with(" (")
                    && cols(spans[0].content.as_ref()) + cols(spans[1].content.as_ref()) > avail
                {
                    spans.pop();
                }
                Paragraph::new(Line::from(spans)).render(
                    Rect {
                        x: area.x + 2,
                        y,
                        width: avail as u16,
                        height: 1,
                    },
                    buf,
                );
            }
            if !info {
                slf.surf_hits.push((y, i));
            }
        };
        for i in 0..head.min(self.menu_rows.len()) {
            paint(i, LIST0 + i as u16, self, buf);
        }
        for j in 0..shown {
            let i = head + top + j;
            if i >= self.menu_rows.len() {
                break;
            }
            paint(i, body0 + j as u16, self, buf);
        }
        let below = body.saturating_sub(top + shown);
        let cap_k = body0 + shown as u16;
        if below > 0
            && !more.is_empty()
            && let Some(y) = row_y(cap_k)
        {
            let rail = row_rail_at(cap_k);
            buf.set_line(
                area.x + 2,
                y,
                &Line::from(Span::styled(format!("… +{below} {more}"), Theme::dim())),
                area.width.saturating_sub(2 + rail),
            );
        }
        let n = hints.len() as u16;
        for (i, (key, what)) in hints.iter().enumerate() {
            if let Some(y) = row_y(full_h.saturating_sub(n - i as u16)) {
                Paragraph::new(Theme::hints(&[(*key, what.as_str())]).right_aligned()).render(
                    Rect {
                        x: area.x,
                        y,
                        width: area.width.saturating_sub(1),
                        height: 1,
                    },
                    buf,
                );
            }
        }
    }

    pub(super) fn draw_popup(&mut self, buf: &mut Buffer, area: Rect, off: u16, full_h: u16) {
        self.popup_rows.clear();
        if !self.popup_visible() {
            return;
        }
        let items = self.popup_items();
        let n = items.len() as i64;
        if items.is_empty() {
            return;
        }
        let body = (full_h as i64 - 1).max(1);
        if let Some(w) = self.popup_walk {
            if w < self.popup_top {
                self.popup_top = w;
            } else if w >= self.popup_top + body {
                self.popup_top = w - body + 1;
            }
        }
        let cursor_row = self.popup_walk.map(|w| w - self.popup_top + 1);
        let mut rows: Vec<Line> = Vec::new();
        for r in 0..area.height {
            let k = off + r;
            if k >= full_h {
                break;
            }
            if k == 0 {
                rows.push(Line::default());
                continue;
            }
            let idx = (self.popup_top + (k - 1) as i64).rem_euclid(n) as usize;
            let Some(item) = items.get(idx) else {
                break;
            };
            let cur = cursor_row == Some(k as i64);
            rows.push(Line::from(vec![
                Span::styled(
                    if cur { "› " } else { "  " },
                    if cur {
                        Theme::brand_ink()
                    } else {
                        Style::new().add_modifier(Modifier::DIM)
                    },
                ),
                Span::styled(
                    item.clone(),
                    if cur {
                        Style::new().fg(Theme::FG()).add_modifier(Modifier::BOLD)
                    } else {
                        Style::new().fg(Theme::FG())
                    },
                ),
            ]));
            self.popup_rows.push((area.y + r, item.clone()));
        }
        Paragraph::new(rows).render(area, buf);
    }

    pub(super) fn queue_line(&self, w: u16) -> Line<'static> {
        let n = self.pending_queue.len();
        let first = self
            .pending_queue
            .first()
            .map(|queued| queued.text.as_str())
            .unwrap_or("");
        let head = format!("  queued {n}:");
        let mut tail = format!(" {first}");
        if n > 1 {
            tail.push_str(&format!(" (+{} more)", n - 1));
        }
        if !self.streaming {
            tail.push_str(" · enter to send");
        }
        let tail = truncate_display_width(&tail, (w as usize).saturating_sub(head.width()));
        Line::from(vec![
            Span::styled(head, Style::new().fg(Color::White)),
            Span::styled(tail, Theme::dim()),
        ])
    }

    pub(super) fn notice_line(&mut self, w: u16) -> Option<Line<'static>> {
        let width = w as usize;
        if let Some(line) = self.retry_line.clone() {
            return Some(Line::from(Span::styled(
                truncate_display_width(&format!(" {line}"), width),
                Theme::warn(),
            )));
        }
        if let Some((text, kind)) = self.live_toast() {
            let st = match kind {
                StatusKind::Info => Theme::dim(),
                StatusKind::Ok => Theme::ok(),
                StatusKind::Warn => Theme::warn(),
                StatusKind::Err => Theme::err(),
            };
            return Some(Line::from(Span::styled(
                truncate_display_width(&format!(" {text}"), width),
                st,
            )));
        }
        if let Some(cp) = self.last_checkpoint.clone() {
            return Some(Line::from(Span::styled(
                truncate_display_width(&format!(" checkpoint: {cp}"), width),
                Theme::dim(),
            )));
        }
        None
    }

    pub(super) fn status_bar(&mut self, w: u16) -> Paragraph<'static> {
        let spans = self.status_bar_spans(w);
        Paragraph::new(Line::from(spans)).style(Theme::base())
    }

    pub(super) fn status_bar_spans(&mut self, w: u16) -> Vec<Span<'static>> {
        let context_used = self.session.context_tokens_used();
        let ctx_pct = (self.session.context_percent() as u64).min(100);
        let tok_str = fmt_k(context_used);
        let ctx_label = format!("{ctx_pct}% ({tok_str})");

        // The bottom line carries the name alone: the row has no room to spare,
        // and the model was picked from a provider-scoped menu, so the endpoint
        // is not in question here. Identity with the endpoint lives in the
        // session info, the sessions list and <agent_state>.
        let model_label = crate::config::key_name(&self.session.model_key).to_string();
        let effort_plan = self.effort_plan();
        let ef_short = effort_plan.short_label();
        let ef_label = ef_short
            .strip_prefix("ef:")
            .unwrap_or(&ef_short)
            .to_string();
        self.ef_click = None;
        self.md_click = None;
        self.ctx_click = None;

        // Two chips one column apart: the mode chip's trailing pad is the
        // whole gap, the approval chip carries no leading pad. The mode
        // chip answers "read-only or full", the approval chip "who
        // approves" — separate axes, separate keys (Tab, Shift+Tab), both
        // visible even in PLAN. [ui].approval_chip = false drops the
        // second chip from the bar; the axis itself stays on Shift+Tab.
        let mode_chip = format!(" {} ", self.mode.label());
        let approval_chip = if self.cfg.ui.approval_chip {
            format!("{} ", self.approval_mode.label())
        } else {
            String::new()
        };
        let left_base_cols = cols(&mode_chip) + cols(&approval_chip);
        let fixed_len: usize = cols(&ctx_label) + 1 + cols(&model_label) + 1 + cols(&ef_label) + 1;

        let lw = left_base_cols as u16;
        let right_len = fixed_len;
        let chip_settled = match self.mode {
            Mode::Act => Theme::mode_chip_act(),
            Mode::Plan => Theme::mode_chip_plan(),
        };
        let chip_style = match self.mode_blend {
            Some((_, t0))
                if t0.elapsed().as_millis() < MODE_BLEND_MS as u128
                    && crate::tui::shimmer::has_truecolor() =>
            {
                let (r, g, b) = self.mode_chip_rgb(std::time::Instant::now());
                Style::new().fg(Color::Rgb(r, g, b))
            }
            _ => chip_settled,
        };
        let approval_settled = match self.approval_mode {
            ApprovalMode::Ask => Theme::mode_chip_ask(),
            ApprovalMode::Auto => Theme::mode_chip_auto(),
            ApprovalMode::Full => Theme::mode_chip_full(),
        };
        let approval_style = match self.approval_blend {
            Some((_, t0))
                if t0.elapsed().as_millis() < MODE_BLEND_MS as u128
                    && crate::tui::shimmer::has_truecolor() =>
            {
                let (r, g, b) = self.approval_chip_rgb(std::time::Instant::now());
                Style::new().fg(Color::Rgb(r, g, b))
            }
            _ => approval_settled,
        };
        let mut spans = vec![Span::styled(mode_chip, chip_style)];
        if self.cfg.ui.approval_chip {
            spans.push(Span::styled(approval_chip, approval_style));
        }
        let pad = (w as usize).saturating_sub(lw as usize + right_len);
        let ctx_cols = cols(&ctx_label) as u16;
        let model_x0 = lw + pad as u16 + ctx_cols + 1;
        let model_cols = cols(&model_label) as u16;
        spans.push(Span::styled(" ".repeat(pad), Theme::base()));
        spans.push(Span::styled(ctx_label.clone(), Theme::dim()));
        self.ctx_click = Some((lw + pad as u16, lw + pad as u16 + ctx_cols));
        spans.push(Span::styled(" ".to_string(), Theme::base()));
        spans.push(Span::styled(model_label, Theme::meta()));
        self.md_click = Some((model_x0, model_x0 + model_cols));
        spans.push(Span::styled(" ".to_string(), Theme::base()));
        let ef_x0 = model_x0 + model_cols + 1;
        let ef_style = if self.model_cfg.effort == EffortLevel::Off || !effort_plan.is_honoured() {
            Theme::dim()
        } else {
            Theme::effort(self.model_cfg.effort)
        };
        spans.push(Span::styled(ef_label.clone(), ef_style));
        self.ef_click = Some((ef_x0, ef_x0 + cols(&ef_label) as u16));
        spans.push(Span::styled(" ".to_string(), Theme::base()));
        spans
    }
}
