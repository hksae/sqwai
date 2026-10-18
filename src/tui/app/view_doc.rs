//! Segment rendering and incremental transcript chunk assembly.

#![allow(unused_imports)]
use super::view::*;
use super::*;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tui::markdown::{render, wrap_tagged};
use crate::tui::theme::Theme;

/// Expand tabs (`\t` -> 8-column tab stops relative to `start_col`) and apply
/// carriage-return (`\r`) line overwrites BEFORE `truncate_display_width`
/// measures the row. Without this, `truncate_display_width` counts `\t` as 0
/// columns, leaves it in the string, and `wrap_tagged` later expands `\t` to 8
/// spaces — pushing the truncated row 8 columns past `width` and wrapping its
/// tail onto a second line.
fn expand_tool_row_controls(raw: &str, start_col: usize) -> String {
    let overwrite = raw
        .rfind('\r')
        .filter(|&i| raw.as_bytes().get(i + 1) != Some(&b'\n'));
    let text = match overwrite {
        Some(i) => &raw[i + 1..],
        None => raw,
    };
    if !text.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut col = start_col;
    for ch in text.chars() {
        if ch == '\t' {
            let spaces = 8 - (col % 8);
            for _ in 0..spaces {
                out.push(' ');
            }
            col += spaces;
        } else if ch == '\n' || ch == '\r' {
            out.push(' ');
            col += 1;
        } else if ch.is_control() {
            // drop zero-width control chars
        } else {
            col += UnicodeWidthChar::width(ch).unwrap_or(0);
            out.push(ch);
        }
    }
    out
}

impl App {
    /// cache key for a segment's rendered content; changing it forces a repaint.
    /// Byte lengths (O(1)) rather than char counts: counting chars over the
    /// whole transcript on every streamed frame was the long-chat lag.
    pub(super) fn seg_key(&self, seg: &Segment) -> usize {
        match seg {
            Segment::User(t) => t.len(),
            Segment::Assistant { text, .. } => text.len(),
            Segment::Commentary(text) => text.len(),
            Segment::Subagent {
                id,
                task,
                status,
                output,
                expanded,
            } => {
                let mut k = id
                    .wrapping_add(task.len() as u64)
                    .wrapping_add(status.len() as u64)
                    .wrapping_add(output.len() as u64)
                    .wrapping_add(*expanded as u64) as usize;
                if !matches!(status.as_str(), "completed" | "failed") {
                    k = k.wrapping_add(self.spinner_tick * 7);
                }
                k
            }
            Segment::Thinking {
                text,
                expanded,
                live,
                started,
                ..
            } => {
                text.len() * 2
                    + *expanded as usize
                    + usize::from(*live) * 3
                    + if *live {
                        started
                            .map(|t| (t.elapsed().as_millis() / 100) as usize)
                            .unwrap_or(0)
                    } else {
                        0
                    }
            }
            Segment::Tool {
                name,
                args,
                ok,
                output,
                diff,
                expanded,
                flash,
                ask,
                ..
            } => {
                let mut k = name.len()
                    + args.len()
                    + output.len()
                    + diff.as_ref().map(|d| d.len() * 3).unwrap_or(0)
                    + usize::from(*expanded) * 5;
                k = match ok {
                    None => k.wrapping_add(self.spinner_tick * 7),
                    Some(true) => k.wrapping_add(1),
                    Some(false) => k.wrapping_add(2),
                };
                if let Some(ask) = ask {
                    k = k.wrapping_add(ask.questions.len() * 1000 + ask.focus * 101);
                    for (q, p) in ask.picked.iter().enumerate() {
                        for (opt, v) in p.iter().enumerate() {
                            if *v {
                                k = k.wrapping_add(
                                    ((1usize << (opt.min(60))) | 1).wrapping_mul((q + 1) * 100_003),
                                );
                            }
                        }
                    }
                    for c in &ask.custom {
                        k += c.len();
                    }
                    if let Some(hover) = self.ask_hover {
                        k = k.wrapping_add(match hover {
                            AskRow::Option { q, opt } => (q + 1) * 1_000_007 + (opt + 1) * 1_009,
                            AskRow::Custom { q } => (q + 1) * 2_000_033 + 7,
                            AskRow::Confirm => 3_000_037,
                        });
                    }
                    if self.ask_custom_focus.is_some() {
                        k = k.wrapping_add(5_000_021);
                    }
                }
                if let Some(t0) = flash
                    && let Some(done) = ok
                    && t0.elapsed().as_millis() < crate::tui::shimmer::flash_ms(*done) as u128
                    && crate::tui::shimmer::has_truecolor()
                {
                    k = k.wrapping_add(1_000_000 + self.spinner_tick.wrapping_mul(13));
                }
                k
            }
            Segment::Status {
                text,
                kind,
                expanded,
                ..
            } => {
                text.len() * 3
                    + usize::from(*expanded)
                    + match kind {
                        StatusKind::Info => 0,
                        StatusKind::Ok => 7,
                        StatusKind::Warn => 14,
                        StatusKind::Err => 21,
                    }
            }
        }
    }

    fn render_segment_wrapped(
        &mut self,
        id: u64,
        idx: usize,
        w: u16,
        interactive: bool,
    ) -> Vec<(Line<'static>, Option<usize>)> {
        let live = matches!(
            self.segments.get(idx),
            Some(Segment::Assistant { live: true, .. })
        );
        if !live {
            let chunk = self.render_segment(&self.segments, idx, w, interactive);
            let (rows, tags) = wrap_tagged(chunk, w);
            return rows.into_iter().zip(tags).collect();
        }
        let Self {
            segments,
            live_md,
            hl,
            ..
        } = self;
        match &segments[idx] {
            Segment::Assistant { text, .. } => live_md.wrapped_lines(id, idx, text, w, hl),
            _ => Vec::new(),
        }
    }

    fn ask_body_rows(
        &self,
        ask: &AskState,
        idx: usize,
        live: bool,
        w: u16,
    ) -> Vec<(Line<'static>, Option<usize>)> {
        let width = usize::from(w).max(1);
        let mut out: Vec<(Line<'static>, Option<usize>)> = Vec::new();
        let mut row = |spans: Vec<Span<'static>>, hovered: bool| {
            let line = Self::form_hover_band(Line::from(spans), live && hovered, width);
            out.push((line, Some(idx)));
        };
        for (q_idx, q) in ask.questions.iter().enumerate() {
            let focused = live && q_idx == ask.focus;
            let locked = live && (q_idx > ask.focus || !ask.can_reach(q_idx));
            if !q.header.is_empty() {
                row(
                    vec![Span::styled(
                        truncate_display_width(&format!("  {}", q.header), width),
                        if focused { Theme::base() } else { Theme::dim() },
                    )],
                    false,
                );
            }
            row(
                vec![Span::styled(
                    truncate_display_width(&format!("  {}", q.question), width),
                    if locked { Theme::dim() } else { Theme::base() },
                )],
                false,
            );
            for (o_idx, opt) in q.options.iter().enumerate() {
                let picked = ask
                    .picked
                    .get(q_idx)
                    .and_then(|v| v.get(o_idx))
                    .copied()
                    .unwrap_or(false);
                let marker = if q.multiple {
                    if picked { "  [x] " } else { "  [ ] " }
                } else if picked {
                    "  ● "
                } else {
                    "  ○ "
                };
                let mut text = format!("{}. {}", o_idx + 1, opt.label);
                if opt.recommended {
                    text.push_str("  · recommended");
                }
                if let Some(d) = &opt.description {
                    text.push_str(&format!(" — {d}"));
                }
                let text = truncate_display_width(&text, width.saturating_sub(marker.width()));
                row(
                    vec![
                        Span::styled(
                            marker.to_string(),
                            if picked {
                                Theme::brand_ink()
                            } else {
                                Theme::dim()
                            },
                        ),
                        Span::styled(
                            text,
                            if locked { Theme::dim() } else { Theme::base() },
                        ),
                    ],
                    live && !locked
                        && self.ask_hover
                            == Some(AskRow::Option {
                                q: q_idx,
                                opt: o_idx,
                            }),
                );
            }
            if q.allow_free {
                let c = ask.custom.get(q_idx).map(|s| s.as_str()).unwrap_or("");
                let editing = live && self.ask_custom_focus == Some(q_idx);
                let filled = !c.is_empty();
                let marker = if q.multiple {
                    if filled { "  [x] " } else { "  [ ] " }
                } else if filled {
                    "  ● "
                } else {
                    "  ○ "
                };
                let text = if filled {
                    format!(
                        "{}. {c}{}",
                        q.options.len() + 1,
                        if editing { "▌" } else { "" }
                    )
                } else {
                    format!("{}. your answer…", q.options.len() + 1)
                };
                let text = truncate_display_width(&text, width.saturating_sub(marker.width()));
                row(
                    vec![
                        Span::styled(
                            marker.to_string(),
                            if filled {
                                Theme::brand_ink()
                            } else {
                                Theme::dim()
                            },
                        ),
                        Span::styled(
                            text,
                            if (filled || editing) && !locked {
                                Theme::base()
                            } else {
                                Theme::dim()
                            },
                        ),
                    ],
                    live && !locked && self.ask_hover == Some(AskRow::Custom { q: q_idx }),
                );
            }
            if q_idx + 1 < ask.questions.len() {
                row(
                    vec![Span::styled(
                        truncate_display_width("  ──", width),
                        Theme::dim(),
                    )],
                    false,
                );
            }
        }
        let confirm_label = if ask.focus + 1 < ask.questions.len() {
            "   enter to next question"
        } else {
            "   enter to confirm"
        };
        row(
            vec![Span::styled(
                truncate_display_width(confirm_label, width),
                Theme::dim(),
            )],
            live && ask.is_answered(ask.focus) && self.ask_hover == Some(AskRow::Confirm),
        );
        out
    }

    pub(super) fn render_segment(
        &self,
        segs: &[Segment],
        idx: usize,
        w: u16,
        interactive: bool,
    ) -> Vec<(Line<'static>, Option<usize>)> {
        let mut out: Vec<(Line<'static>, Option<usize>)> = Vec::new();
        match &segs[idx] {
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
            Segment::Commentary(text) => {
                let (rows, _) = crate::tui::markdown::wrap_tagged(
                    render(text, w, &self.hl)
                        .into_iter()
                        .map(|l| (l, None))
                        .collect(),
                    w,
                );
                for row in rows {
                    out.push((Line::from(row.spans), Some(idx)));
                }
            }
            Segment::Thinking {
                text,
                expanded,
                started,
                duration_ms,
                live,
            } => {
                let elapsed_ms = if *live {
                    started.map_or(0, |t| t.elapsed().as_millis() as u64)
                } else {
                    *duration_ms
                };
                let restored = !*live && started.is_none() && *duration_ms == 0;
                let time = if restored {
                    String::new()
                } else if elapsed_ms < 1000 {
                    format!(" · 0.{}s", (elapsed_ms % 1000) / 100)
                } else {
                    format!(" · {}s", elapsed_ms / 1000)
                };
                if !*expanded {
                    let label = if !*live {
                        format!("  thought{time}")
                    } else if text.is_empty() {
                        format!("  thinking…{time}")
                    } else {
                        format!("  thinking{time}")
                    };
                    let spans = vec![Span::styled(label, Theme::dim())];
                    out.push((Line::from(spans), Some(idx)));
                } else {
                    for l in render(text, w, &self.hl) {
                        out.push((dim_all(l), Some(idx)));
                    }
                }
            }
            Segment::Subagent {
                id,
                task,
                status,
                output,
                expanded,
            } => {
                let failed = status == "failed";
                let marker = match status.as_str() {
                    "completed" => ("• ".to_string(), tool_dot_style(true)),
                    "failed" => ("• ".to_string(), tool_dot_style(false)),
                    _ => (
                        format!(
                            "{} ",
                            WORKING_SPINNER[self.spinner_tick % WORKING_SPINNER.len()]
                        ),
                        Theme::spinner(),
                    ),
                };
                let name = format!("subagent-{id}");
                let summary_width = usize::from(w)
                    .saturating_sub(2 + name.len() + 2)
                    .min(usize::from(w) / 2);
                let clean_task = task.split_whitespace().collect::<Vec<_>>().join(" ");
                let summary = if clean_task.is_empty() || summary_width == 0 {
                    String::new()
                } else {
                    format!(" ({})", truncate_display_width(&clean_task, summary_width))
                };
                out.push((
                    Line::from(vec![
                        Span::styled(marker.0, marker.1),
                        Span::styled(name, if failed { Theme::err() } else { Theme::meta() }),
                        Span::styled(summary, Theme::dim()),
                    ]),
                    Some(idx),
                ));
                if *expanded {
                    let non_empty: Vec<&str> = output
                        .lines()
                        .map(str::trim_end)
                        .filter(|l| !l.trim().is_empty())
                        .collect();
                    let width = usize::from(w).saturating_sub(2).max(1);
                    if non_empty.is_empty() {
                        if status == "running" {
                            out.push((
                                Line::from(vec![
                                    Span::styled("  ".to_string(), Theme::base()),
                                    Span::styled("thinking…".to_string(), Theme::dim()),
                                ]),
                                Some(idx),
                            ));
                        }
                    } else {
                        let total = non_empty.len();
                        let shown = &non_empty[..total.min(40)];
                        for l in shown {
                            let expanded_l = expand_tool_row_controls(l, 2);
                            let st = if expanded_l.starts_with('+') && !expanded_l.starts_with("+++") {
                                Theme::ok()
                            } else if expanded_l.starts_with('-') && !expanded_l.starts_with("---") {
                                Theme::err()
                            } else if expanded_l.starts_with("@@")
                                || expanded_l.starts_with("[Subagent receipt:")
                            {
                                Theme::accent()
                            } else {
                                Theme::dim()
                            };
                            out.push((
                                Line::from(vec![
                                    Span::styled("  ".to_string(), Theme::base()),
                                    Span::styled(truncate_display_width(&expanded_l, width), st),
                                ]),
                                Some(idx),
                            ));
                        }
                        if total > shown.len() {
                            let more = format!("… {} more lines", total - shown.len());
                            out.push((
                                Line::from(vec![
                                    Span::styled("  ".to_string(), Theme::base()),
                                    Span::styled(truncate_display_width(&more, width), Theme::dim()),
                                ]),
                                Some(idx),
                            ));
                        }
                    }
                }
            }
            Segment::Tool {
                name,
                args,
                ok,
                output,
                diff,
                preview,
                preview_total,
                expanded,
                flash,
                ask,
                ..
            } => {
                let marker = match ok {
                    None => (
                        format!(
                            "{} ",
                            WORKING_SPINNER[self.spinner_tick % WORKING_SPINNER.len()]
                        ),
                        Theme::spinner(),
                    ),
                    Some(done) => ("• ".to_string(), tool_dot_style(*done)),
                };
                let marker_width = 2usize;
                let name_width = name
                    .width()
                    .min(usize::from(w).saturating_sub(marker_width));
                let shown_name = truncate_display_width(name, name_width);
                let available = usize::from(w)
                    .saturating_sub(marker_width + name_width)
                    .saturating_sub(2);
                let edit_counts = if name == "edit" {
                    diff.as_deref().map(edit_change_counts)
                } else {
                    None
                };
                let counts_width = edit_counts
                    .map(|(added, removed)| format!("  +{added} -{removed}").width())
                    .unwrap_or(0);
                let summary_width = available
                    .saturating_sub(counts_width)
                    .min(usize::from(w) / 2);
                let clean_args = args.split_whitespace().collect::<Vec<_>>().join(" ");
                let summary = if ask.is_some() || clean_args.is_empty() || summary_width == 0 {
                    String::new()
                } else {
                    format!(" ({})", truncate_display_width(&clean_args, summary_width))
                };
                let wave = match (ok, flash) {
                    (Some(done), Some(t0)) => {
                        let el = t0.elapsed().as_millis();
                        let span = crate::tui::shimmer::flash_ms(*done) as u128;
                        if el < span && crate::tui::shimmer::has_truecolor() {
                            Some(crate::tui::shimmer::flash_spans(
                                &marker.0,
                                &shown_name,
                                *done,
                                el as f64 / span as f64,
                            ))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                let mut head_spans = match wave {
                    Some(wave) => {
                        let mut spans = wave;
                        spans.push(Span::styled(summary, Theme::dim()));
                        spans
                    }
                    None if ok.is_none() => {
                        let mut spans = vec![Span::styled(marker.0, marker.1)];
                        spans.extend(crate::tui::shimmer::shimmer_spans(
                            &shown_name,
                            self.spinner_tick,
                        ));
                        spans.push(Span::styled(summary, Theme::dim()));
                        spans
                    }
                    None => vec![
                        Span::styled(marker.0, marker.1),
                        Span::styled(shown_name, Theme::meta()),
                        Span::styled(summary, Theme::dim()),
                    ],
                };
                if let Some((added, removed)) = edit_counts {
                    head_spans.push(Span::styled(format!("  +{added}"), Theme::ok()));
                    head_spans.push(Span::styled(format!(" -{removed}"), Theme::err()));
                }
                let head = Line::from(head_spans);
                out.push((head, Some(idx)));
                if let (true, Some(ask)) = (*expanded, ask.as_deref()) {
                    out.extend(self.ask_body_rows(
                        ask,
                        idx,
                        interactive && self.active_ask_seg() == Some(idx),
                        w,
                    ));
                } else if *expanded {
                    let (preview, preview_total) =
                        if preview.is_empty() && (!output.is_empty() || diff.is_some()) {
                            tool_preview(diff.as_deref(), output)
                        } else {
                            (preview.clone(), *preview_total)
                        };
                    let shown: Vec<String> = preview.clone();
                    let total = preview_total;
                    let width = usize::from(w).saturating_sub(2).max(1);
                    for l in &shown {
                        let expanded_l = expand_tool_row_controls(l, 2);
                        let st = if expanded_l.starts_with('+') && !expanded_l.starts_with("+++") {
                            Theme::ok()
                        } else if expanded_l.starts_with('-') && !expanded_l.starts_with("---") {
                            Theme::err()
                        } else if expanded_l.starts_with("@@") {
                            Theme::accent()
                        } else {
                            Theme::dim()
                        };
                        out.push((
                            Line::from(vec![
                                Span::styled("  ".to_string(), Theme::base()),
                                Span::styled(truncate_display_width(&expanded_l, width), st),
                            ]),
                            Some(idx),
                        ));
                    }
                    if total > shown.len() {
                        let more = format!("… {} more lines", total - shown.len());
                        out.push((
                            Line::from(vec![
                                Span::styled("  ".to_string(), Theme::base()),
                                Span::styled(truncate_display_width(&more, width), Theme::dim()),
                            ]),
                            Some(idx),
                        ));
                    }
                }
            }
            Segment::Status {
                text,
                kind,
                expanded,
                ..
            } => {
                let st = match kind {
                    StatusKind::Info => Theme::dim(),
                    StatusKind::Ok => Theme::ok(),
                    StatusKind::Warn => Theme::warn(),
                    StatusKind::Err => Theme::err(),
                };
                let parts: Vec<&str> = text.split('\n').collect();
                let wide = UnicodeWidthStr::width(parts[0]) > (w as usize).saturating_sub(2);
                let collapsed =
                    matches!(kind, StatusKind::Err) && !expanded && (parts.len() > 1 || wide);
                let show: &[&str] = if collapsed { &parts[..1] } else { &parts[..] };
                for part in show {
                    let row = if collapsed {
                        let more = parts.len() - 1;
                        let suffix = if more > 0 {
                            format!("… ({} more)", more)
                        } else {
                            "…".to_string()
                        };
                        let head = truncate_display_width(
                            part,
                            (w as usize).saturating_sub(4 + suffix.chars().count()),
                        );
                        format!("  {head} {suffix}")
                    } else {
                        format!("  {part}")
                    };
                    out.push((Line::from(vec![Span::styled(row, st)]), Some(idx)));
                }
            }
        }
        out
    }

    pub(super) fn rebuild_cache(&mut self, width: u16) {
        let t0 = std::time::Instant::now();
        let w = width.saturating_sub(2).max(10);
        let mut chunks: Vec<RowChunk> = Vec::new();
        let mut fresh: Vec<bool> = Vec::new();
        let mut struct_ord = 0u64;
        macro_rules! struct_row {
            ($line:expr, $tag:expr) => {{
                chunks.push(struct_chunk(struct_ord, $line, $tag, w));
                fresh.push(true);
                struct_ord += 1;
            }};
        }
        macro_rules! blank_row {
            () => {{
                chunks.push(blank_chunk(struct_ord));
                fresh.push(true);
                struct_ord += 1;
            }};
        }
        if self.active_subagent.is_none() {
            blank_row!();
            for l in self.empty_mark_lines() {
                struct_row!(l, None);
            }
        }
        for idx in 0..self.segments.len() {
            let seg = &self.segments[idx];
            match seg {
                Segment::User(_) => {
                    blank_row!();
                }
                Segment::Assistant { text, live } => {
                    if *live && text.trim().is_empty() {
                        continue;
                    }
                    blank_row!();
                }
                Segment::Commentary(_)
                | Segment::Thinking { .. }
                | Segment::Subagent { .. }
                | Segment::Tool { .. }
                | Segment::Status { .. } => {
                    blank_row!();
                }
            }

            let key = self.seg_key(seg);
            let meta = self
                .seg_meta
                .get(idx)
                .copied()
                .unwrap_or(SegMeta { id: 0, rev: 0 });
            let render_w = w;
            let hit = matches!(self.seg_cache.get(&meta.id), Some(entry)
                if entry.rev == meta.rev && entry.width == render_w && entry.key == key);
            if hit {
                chunks.push((AsmTag::Seg(meta.id), Vec::new()));
                fresh.push(false);
                continue;
            }
            self.test_renders += 1;
            let rows = self.render_segment_wrapped(meta.id, idx, render_w, true);
            self.seg_cache.insert(
                meta.id,
                SegCacheEntry {
                    rev: meta.rev,
                    width: render_w,
                    key,
                    rows: rows.clone(),
                },
            );
            chunks.push((AsmTag::Seg(meta.id), rows));
            fresh.push(true);
        }
        if self.streaming {
            let line = working_line(
                self.thinking_open,
                self.turn_started.map(|at| at.elapsed().as_secs()),
                self.spinner_tick,
            );
            blank_row!();
            struct_row!(line, None);
        }
        let _ = struct_ord;
        self.prune_seg_cache();
        self.merge_chunks(chunks, fresh);
        self.cache_w = width;
        self.last_rebuild_us = t0.elapsed().as_micros();
    }

    pub(super) fn prune_seg_cache(&mut self) {
        const SEG_CACHE_CAP: usize = 2000;
        if !self.live_md.is_empty() {
            let segments = &self.segments;
            let seg_meta = &self.seg_meta;
            let subagent_chats = &self.subagent_chats;
            let subagent_meta = &self.subagent_meta;
            self.live_md.retain(|id| {
                let in_main = seg_meta.iter().enumerate().any(|(idx, m)| {
                    m.id == id
                        && matches!(segments.get(idx), Some(Segment::Assistant { live: true, .. }))
                });
                if in_main {
                    return true;
                }
                subagent_chats.iter().any(|(chat_id, chat)| {
                    let metas = subagent_meta.get(chat_id);
                    chat.iter().enumerate().any(|(idx, seg)| {
                        matches!(seg, Segment::Assistant { live: true, .. })
                            && metas.and_then(|m| m.get(idx)).is_some_and(|m| m.id == id)
                    })
                })
            });
        }
        if self.subagent_meta.is_empty()
            && self.seg_cache.len() <= self.seg_meta.len()
            && self.seg_cache.len() <= SEG_CACHE_CAP
        {
            return;
        }
        let mut live = std::collections::HashSet::with_capacity(self.seg_meta.len() + 8);
        for meta in &self.seg_meta {
            live.insert(meta.id);
        }
        for chat_meta in self.subagent_meta.values() {
            for meta in chat_meta {
                live.insert(meta.id);
            }
        }
        self.seg_cache.retain(|id, _| live.contains(id));
        if self.seg_cache.len() > SEG_CACHE_CAP {
            let keep_from = self.seg_meta.len().saturating_sub(SEG_CACHE_CAP);
            let evict: Vec<u64> = self.seg_meta[..keep_from].iter().map(|m| m.id).collect();
            for id in evict {
                self.seg_cache.remove(&id);
            }
        }
    }

    fn chunk_rows(&self, tag: AsmTag) -> Vec<(Line<'static>, Option<usize>)> {
        match tag {
            AsmTag::Seg(id) => self
                .seg_cache
                .get(&id)
                .map(|e| e.rows.clone())
                .unwrap_or_default(),
            AsmTag::Struct(_) => Vec::new(),
        }
    }

    fn range_eq(&self, at: usize, len: usize, rows: &[(Line<'static>, Option<usize>)]) -> bool {
        if rows.len() != len {
            return false;
        }
        let end = at.checked_add(len);
        let (Some(old_l), Some(old_t)) = (
            end.and_then(|e| self.cache_lines.get(at..e)),
            end.and_then(|e| self.cache_rowseg.get(at..e)),
        ) else {
            return false;
        };
        old_l
            .iter()
            .zip(old_t.iter())
            .zip(rows.iter())
            .all(|((l, t), (nl, nt))| l == nl && t == nt)
    }

    fn splice_chunks(&mut self, built: Vec<RowChunk>, fresh: Vec<bool>) {
        let mut at = 0usize;
        for (i, (tag, rows)) in built.into_iter().enumerate() {
            let old_len = self.asm_lens.get(i).copied().unwrap_or(0);
            if !fresh.get(i).copied().unwrap_or(false) {
                at += old_len;
                continue;
            }
            if matches!(tag, AsmTag::Struct(_)) && self.range_eq(at, old_len, &rows) {
                at += old_len;
                continue;
            }
            let new_len = rows.len();
            let (ls, ts): (Vec<Line>, Vec<Option<usize>>) = rows.into_iter().unzip();
            self.cache_lines.splice(at..at + old_len, ls);
            self.cache_rowseg.splice(at..at + old_len, ts);
            if let Some(lens) = self.asm_lens.get_mut(i) {
                *lens = new_len;
            }
            at += new_len;
        }
    }

    fn concat_chunks(&mut self, built: Vec<RowChunk>, fresh: Vec<bool>) {
        let mut tags = Vec::with_capacity(built.len());
        let mut lens = Vec::with_capacity(built.len());
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut rowseg: Vec<Option<usize>> = Vec::new();
        for (i, (tag, rows)) in built.into_iter().enumerate() {
            let rows = if fresh.get(i).copied().unwrap_or(false) {
                rows
            } else {
                self.chunk_rows(tag)
            };
            tags.push(tag);
            lens.push(rows.len());
            for (l, t) in rows {
                lines.push(l);
                rowseg.push(t);
            }
        }
        self.cache_lines = lines;
        self.cache_rowseg = rowseg;
        self.asm_tags = tags;
        self.asm_lens = lens;
    }

    pub(super) fn merge_chunks(&mut self, built: Vec<RowChunk>, fresh: Vec<bool>) {
        debug_assert_eq!(built.len(), fresh.len());
        self.last_fresh = fresh.iter().filter(|b| **b).count();
        let new_tags: Vec<AsmTag> = built.iter().map(|(t, _)| *t).collect();
        let total: usize = self.asm_lens.iter().sum();
        let consistent = self.asm_lens.len() == self.asm_tags.len()
            && total == self.cache_lines.len()
            && total == self.cache_rowseg.len();
        if consistent && new_tags == self.asm_tags {
            let total_segs = new_tags
                .iter()
                .filter(|t| matches!(t, AsmTag::Seg(_)))
                .count();
            let fresh_segs = built
                .iter()
                .zip(fresh.iter())
                .filter(|((tag, _), f)| **f && matches!(tag, AsmTag::Seg(_)))
                .count();
            if fresh_segs * 2 <= total_segs.max(1) {
                self.splice_chunks(built, fresh);
                self.last_merge = MergeKind::Splice;
            } else {
                self.concat_chunks(built, fresh);
                self.last_merge = MergeKind::Concat;
            }
            return;
        }
        if consistent
            && new_tags.len() > self.asm_tags.len()
            && new_tags[..self.asm_tags.len()] == self.asm_tags[..]
        {
            let tail_start = self.asm_tags.len();
            for (n, ((_, rows), tag)) in built
                .into_iter()
                .skip(tail_start)
                .zip(new_tags[tail_start..].iter())
                .enumerate()
            {
                let i = tail_start + n;
                let rows = if fresh.get(i).copied().unwrap_or(false) {
                    rows
                } else {
                    self.chunk_rows(*tag)
                };
                let len = rows.len();
                let (ls, ts): (Vec<Line>, Vec<Option<usize>>) = rows.into_iter().unzip();
                self.cache_lines.extend(ls);
                self.cache_rowseg.extend(ts);
                self.asm_tags.push(*tag);
                self.asm_lens.push(len);
            }
            self.last_merge = MergeKind::Append;
            return;
        }
        self.concat_chunks(built, fresh);
        self.last_merge = MergeKind::Concat;
    }

    fn seg_time_key(&self, seg: &Segment) -> usize {
        match seg {
            Segment::Subagent { status, .. }
                if !matches!(status.as_str(), "completed" | "failed") =>
            {
                self.spinner_tick.wrapping_mul(7).wrapping_add(1)
            }
            Segment::Thinking {
                live: true,
                started,
                ..
            } => started
                .map(|t| (t.elapsed().as_millis() / 100) as usize)
                .unwrap_or(0)
                .wrapping_mul(11)
                .wrapping_add(2),
            Segment::Tool { ok: None, .. } => self.spinner_tick.wrapping_mul(7).wrapping_add(3),
            Segment::Tool {
                ok: Some(done),
                flash: Some(t0),
                ..
            } if t0.elapsed().as_millis() < crate::tui::shimmer::flash_ms(*done) as u128
                && crate::tui::shimmer::has_truecolor() =>
            {
                1_000_000usize.wrapping_add(self.spinner_tick.wrapping_mul(13))
            }
            _ => 0,
        }
    }

    pub(super) fn transcript_fp(&self, width: u16, segs: &[Segment], meta: &[SegMeta]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        width.hash(&mut h);
        self.theme_rev.hash(&mut h);
        self.streaming.hash(&mut h);
        self.thinking_open.hash(&mut h);
        for (i, m) in meta.iter().enumerate() {
            m.id.hash(&mut h);
            m.rev.hash(&mut h);
            if let Some(seg) = segs.get(i) {
                let tk = self.seg_time_key(seg);
                if tk != 0 {
                    tk.hash(&mut h);
                }
            }
        }
        self.ask_hover.hash(&mut h);
        self.ask_custom_focus.hash(&mut h);
        h.finish()
    }

    pub(super) fn update_live_chrome_rows(&mut self, width: u16) {
        if self.active_subagent.is_some() {
            return;
        }
        if self.segments.is_empty() && self.cache_lines.len() >= 2 {
            if let Some(first_mark) = self.empty_mark_lines().into_iter().next() {
                self.cache_lines[1] = first_mark;
            }
        }
        if self.streaming && !self.cache_lines.is_empty() {
            let w = width.saturating_sub(2).max(10);
            let line = working_line(
                self.thinking_open,
                self.turn_started.map(|at| at.elapsed().as_secs()),
                self.spinner_tick,
            );
            let (rows, _) = crate::tui::markdown::wrap_tagged_uncounted(vec![(line, None)], w);
            if let Some(&old_len) = self.asm_lens.last()
                && old_len == rows.len()
                && self.cache_lines.len() >= old_len
            {
                let start = self.cache_lines.len() - old_len;
                for (i, r) in rows.into_iter().enumerate() {
                    self.cache_lines[start + i] = r;
                }
            }
        }
    }
}
