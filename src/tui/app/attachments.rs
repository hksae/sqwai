//! User image attachments: paste/drop entry, draft tokens, and background
//! normalization.
//!
//! The composer holds plain text. A clipboard bitmap has no path, so it gets
//! an `[image N]` marker; an image attached by path keeps the path itself as
//! its token. The attachment list is separate. At submit only attachments
//! whose token is still in the text survive. Decoding and resizing run on a
//! worker thread so a large screenshot never blocks input; a failed marker
//! is removed again, while a path stays useful as text.

use super::*;
use crate::providers::ImageAttachment;
use std::path::{Path, PathBuf};

/// Per-message attachment cap.
pub(super) const MAX_IMAGES: usize = 20;

/// A trailing drop run: its byte range and the `(unquoted token, path)` pairs
/// in text order.
type DroppedPaths = (usize, usize, Vec<(String, PathBuf)>);

#[derive(Debug)]
pub(super) struct QueuedPrompt {
    pub text: String,
    pub images: Vec<PendingImage>,
}

#[cfg(test)]
impl From<&str> for QueuedPrompt {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            images: Vec::new(),
        }
    }
}

#[cfg(test)]
impl From<String> for QueuedPrompt {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

#[cfg(test)]
impl PartialEq<String> for QueuedPrompt {
    fn eq(&self, other: &String) -> bool {
        self.text == *other
    }
}

#[derive(Clone, Copy, Debug)]
struct MarkerSpan {
    /// char offset into the composer text, inclusive
    start: usize,
    /// char offset, exclusive
    end: usize,
    marker: u32,
}

/// A drafted image and the token that names it in the composer. A clipboard
/// bitmap has no path, so it gets `[image N]`; an image attached by path keeps
/// the path itself as ordinary editable text and is never renumbered.
#[derive(Clone, Debug)]
pub(super) struct PendingImage {
    pub(super) image: ImageAttachment,
    pub(super) token: String,
    pub(super) marker: Option<u32>,
}

impl PendingImage {
    pub(super) fn from_marker(mut image: ImageAttachment, marker: u32) -> Self {
        image.marker = Some(marker);
        Self {
            image,
            token: format!("[image {marker}]"),
            marker: Some(marker),
        }
    }

    pub(super) fn from_path(mut image: ImageAttachment, token: String) -> Self {
        image.marker = None;
        Self {
            image,
            token,
            marker: None,
        }
    }
}

#[derive(Clone)]
pub(super) struct PendingImageJob {
    id: u64,
    epoch: u64,
    marker: Option<u32>,
    token: String,
}

pub(super) struct ImageJobResult {
    epoch: u64,
    id: u64,
    result: Result<ImageAttachment, String>,
}

/// Marker number under a character column of a rendered line, if any.
pub(super) fn marker_at_char(text: &str, column: usize) -> Option<u32> {
    scan_marker_spans(text)
        .into_iter()
        .find(|span| column >= span.start && column < span.end)
        .map(|span| span.marker)
}

pub(super) fn marker_char_spans(text: &str) -> Vec<(usize, usize)> {
    scan_marker_spans(text)
        .into_iter()
        .map(|span| (span.start, span.end))
        .collect()
}

fn expanded_marker_range(
    start: usize,
    end: usize,
    spans: &[MarkerSpan],
) -> Option<(usize, usize)> {
    let mut hit = false;
    let (mut start, mut end) = (start, end);
    for span in spans {
        if span.start < end && span.end > start {
            hit = true;
            start = start.min(span.start);
            end = end.max(span.end);
        }
    }
    hit.then_some((start, end))
}

fn char_offset(lines: &[String], row: usize, col: usize) -> usize {
    lines
        .iter()
        .take(row)
        .map(|line| line.chars().count() + 1)
        .sum::<usize>()
        + col
}

fn pos_from_offset(lines: &[String], mut offset: usize) -> (usize, usize) {
    for (row, line) in lines.iter().enumerate() {
        let len = line.chars().count();
        if offset <= len {
            return (row, offset);
        }
        offset -= len + 1;
    }
    let row = lines.len().saturating_sub(1);
    (row, lines.get(row).map_or(0, |line| line.chars().count()))
}

fn char_slice(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

fn scan_marker_spans(text: &str) -> Vec<MarkerSpan> {
    let chars: Vec<char> = text.chars().collect();
    let prefix = ['i', 'm', 'a', 'g', 'e', ' '];
    let mut spans = Vec::new();
    let mut i = 0;
    while i + 7 < chars.len() {
        if chars[i] == '['
            && prefix
                .iter()
                .enumerate()
                .all(|(offset, c)| chars.get(i + 1 + offset) == Some(c))
        {
            let mut j = i + 7;
            let digits_start = j;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > digits_start && chars.get(j) == Some(&']') {
                let digits: String = chars[digits_start..j].iter().collect();
                if let Ok(marker) = digits.parse::<u32>() {
                    spans.push(MarkerSpan {
                        start: i,
                        end: j + 1,
                        marker,
                    });
                    i = j + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    spans
}

enum ImageSource {
    File(PathBuf),
    Clipboard {
        width: usize,
        height: usize,
        bytes: Vec<u8>,
    },
}

impl App {
    /// Whether pasted images belong in the main composer rather than a form,
    /// a menu, or an inline question's free-answer field.
    pub(super) fn image_entry_allowed(&self) -> bool {
        self.menu_stack.is_empty() && self.ask_custom_focus.is_none() && !self.is_inline_ask()
    }

    /// Paste into the main composer. Text that is entirely one or more image
    /// paths (a terminal drag&drop, usually quoted) attaches the images and
    /// keeps the paths in the text; anything else is ordinary text.
    pub(super) fn paste_input_text(&mut self, text: &str) {
        self.expand_marker_selection();
        if self.image_entry_allowed()
            && let Some(paths) = self.pasted_image_paths(text)
        {
            self.attach_paths(paths);
            return;
        }
        self.paste_text(text);
        self.repair_markers_after_edit();
    }

    /// Ctrl+V bitmap. Returns false only when this is not the main composer,
    /// so the caller can keep its ordinary text-paste path.
    pub(super) fn attach_clipboard_image(
        &mut self,
        width: usize,
        height: usize,
        bytes: Vec<u8>,
    ) -> bool {
        if !self.image_entry_allowed() {
            return false;
        }
        let Some(marker) = self.new_image_marker() else {
            return true;
        };
        self.expand_marker_selection();
        let token = format!("[image {marker}]");
        self.input.insert_str(&token);
        self.track_job(
            Some(marker),
            token,
            ImageSource::Clipboard {
                width,
                height,
                bytes,
            },
        );
        self.renumber_markers();
        self.dirty = true;
        true
    }

    /// Many terminals deliver a file drop as ordinary characters. When the
    /// composer's trailing token is an absolute, existing image path, attach
    /// the image but leave the path in the text: the model gets the pixels and
    /// the file it can copy or reference. Absolute-only is deliberate: a
    /// manually typed relative path must stay text.
    pub(super) fn maybe_attach_dropped_path(&mut self) {
        if !self.image_entry_allowed() {
            return;
        }
        let text = self.input_text();
        if self.last_drop_text == text {
            return;
        }
        self.last_drop_text = text.clone();
        let Some((_start, _end, paths)) = self.trailing_dropped_paths(&text) else {
            return;
        };
        if self.model_cfg.vision == Some(false) {
            self.status("this model does not accept images", StatusKind::Err);
            return;
        }
        let free =
            MAX_IMAGES.saturating_sub(self.pending_images.len() + self.pending_image_jobs.len());
        let total = paths.len();
        let mut accepted = Vec::new();
        for (token, path) in paths {
            if accepted.len() >= free {
                break;
            }
            if self.has_path_token(&token) {
                continue;
            }
            accepted.push((token, path));
        }
        if accepted.is_empty() {
            if free == 0 && total > 0 {
                self.status(
                    &format!("image limit is {MAX_IMAGES} per message"),
                    StatusKind::Warn,
                );
            }
            return;
        }
        for (token, path) in accepted {
            self.track_job(None, token, ImageSource::File(path));
        }
        if total > free {
            self.status(
                &format!("attached {free} of {total} images — limit is {MAX_IMAGES}"),
                StatusKind::Warn,
            );
        }
        self.dirty = true;
    }

    pub(super) fn poll_image_jobs(&mut self) {
        loop {
            match self.image_jobs_rx.try_recv() {
                Ok(job) => {
                    if job.epoch != self.image_epoch {
                        continue;
                    }
                    let Some(pending) = self
                        .pending_image_jobs
                        .iter()
                        .find(|pending| pending.id == job.id)
                        .cloned()
                    else {
                        continue;
                    };
                    self.pending_image_jobs
                        .retain(|pending| pending.id != job.id);
                    match job.result {
                        Ok(image) => {
                            if self.input_text().contains(&pending.token) {
                                let image = match pending.marker {
                                    Some(marker) => PendingImage::from_marker(image, marker),
                                    None => {
                                        PendingImage::from_path(image, pending.token.clone())
                                    }
                                };
                                self.status(
                                    &format!(
                                        "attached {} · {}×{}",
                                        image.image.name, image.image.width, image.image.height
                                    ),
                                    StatusKind::Ok,
                                );
                                self.pending_images.push(image);
                            }
                        }
                        Err(error) => {
                            // A failed marker token is removed like any other
                            // attachment; a path stays useful as text.
                            if pending.marker.is_some()
                                && self.remove_input_token(&pending.token)
                            {
                                self.dirty = true;
                            }
                            self.status(&error, StatusKind::Err);
                        }
                    }
                    self.renumber_markers();
                    self.dirty = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
    }

    fn live_markers(&self) -> std::collections::HashSet<u32> {
        let mut live = std::collections::HashSet::new();
        live.extend(
            self.pending_images
                .iter()
                .filter_map(|image| image.marker),
        );
        live.extend(
            self.pending_image_jobs
                .iter()
                .filter_map(|job| job.marker),
        );
        live
    }

    fn live_spans(&self, text: &str) -> Vec<MarkerSpan> {
        let live = self.live_markers();
        scan_marker_spans(text)
            .into_iter()
            .filter(|span| live.contains(&span.marker))
            .collect()
    }

    fn cursor_char(&self) -> usize {
        char_offset(self.input.lines(), self.input.cursor().0, self.input.cursor().1)
    }

    fn selection_chars(&self) -> Option<(usize, usize)> {
        let ((r1, c1), (r2, c2)) = self.input.selection_range()?;
        let lines = self.input.lines();
        let a = char_offset(lines, r1, c1);
        let b = char_offset(lines, r2, c2);
        Some(if a <= b { (a, b) } else { (b, a) })
    }

    fn select_chars(&mut self, start: usize, end: usize) {
        let lines = self.input.lines().to_vec();
        let (r1, c1) = pos_from_offset(&lines, start);
        let (r2, c2) = pos_from_offset(&lines, end);
        self.input
            .move_cursor(tui_textarea::CursorMove::Jump(r1 as u16, c1 as u16));
        if start != end {
            self.input.start_selection();
            self.input
                .move_cursor(tui_textarea::CursorMove::Jump(r2 as u16, c2 as u16));
        }
    }

    fn drop_markers(&mut self, markers: &[u32]) {
        let set: std::collections::HashSet<u32> = markers.iter().copied().collect();
        self.pending_images
            .retain(|image| image.marker.is_none_or(|marker| !set.contains(&marker)));
        self.pending_image_jobs
            .retain(|job| job.marker.is_none_or(|marker| !set.contains(&marker)));
    }

    /// Any edit that would touch a live marker deletes the whole token.
    pub(super) fn atomic_erase(&mut self, back: bool) -> bool {
        let text = self.input_text();
        let spans = self.live_spans(&text);
        if spans.is_empty() {
            return false;
        }
        let (start, end) = if self.input.is_selecting() {
            let Some(range) = self.selection_chars() else {
                return false;
            };
            range
        } else {
            let offset = self.cursor_char();
            let len = text.chars().count();
            if back {
                (offset.saturating_sub(1), offset)
            } else {
                (offset, (offset + 1).min(len))
            }
        };
        if start == end {
            return false;
        }
        let Some((start, end)) = expanded_marker_range(start, end, &spans) else {
            return false;
        };
        let removed: Vec<u32> = spans
            .iter()
            .filter(|span| span.start < end && span.end > start)
            .map(|span| span.marker)
            .collect();
        self.select_chars(start, end);
        self.input.delete_str(0);
        self.drop_markers(&removed);
        self.renumber_markers();
        self.dirty = true;
        true
    }

    /// Remove the whole live marker the cursor sits inside, for line/word
    /// kill combos that would otherwise leave a fragment.
    pub(super) fn remove_marker_under_cursor(&mut self) -> bool {
        let text = self.input_text();
        let spans = self.live_spans(&text);
        let offset = self.cursor_char();
        let Some(span) = spans
            .iter()
            .find(|span| offset > span.start && offset < span.end)
            .copied()
        else {
            return false;
        };
        self.select_chars(span.start, span.end);
        self.input.delete_str(0);
        self.drop_markers(&[span.marker]);
        self.renumber_markers();
        self.dirty = true;
        true
    }

    /// Expand a selection so a replacement never leaves half a marker.
    pub(super) fn expand_marker_selection(&mut self) {
        let Some((start, end)) = self.selection_chars() else {
            return;
        };
        if start == end {
            return;
        }
        let spans = self.live_spans(&self.input_text());
        if let Some((start, end)) = expanded_marker_range(start, end, &spans) {
            self.select_chars(start, end);
        }
    }

    /// After any edit, drop drafts whose marker token no longer exists.
    pub(super) fn repair_markers_after_edit(&mut self) {
        let text = self.input_text();
        let present: std::collections::HashSet<u32> = scan_marker_spans(&text)
            .into_iter()
            .map(|span| span.marker)
            .collect();
        let before = self.pending_images.len() + self.pending_image_jobs.len();
        self.pending_images.retain(|image| match image.marker {
            Some(marker) => present.contains(&marker),
            None => text.contains(&image.token),
        });
        self.pending_image_jobs.retain(|job| match job.marker {
            Some(marker) => present.contains(&marker),
            None => text.contains(&job.token),
        });
        if self.pending_images.len() + self.pending_image_jobs.len() != before {
            self.renumber_markers();
        }
    }

    /// Number live markers 1..N in text order. This runs after an attach, a
    /// delete, and at submit; unknown `[image N]` text is left alone.
    pub(super) fn renumber_markers(&mut self) {
        let text = self.input_text();
        let live = self.live_markers();
        if live.is_empty() {
            // Path attachments have no `[image N]` token; they are ordinary
            // text and survive while their path survives.
            self.pending_images.retain(|image| image.marker.is_none());
            self.pending_image_jobs.retain(|job| job.marker.is_none());
            self.image_marker = 0;
            return;
        }
        let spans: Vec<MarkerSpan> = scan_marker_spans(&text)
            .into_iter()
            .filter(|span| live.contains(&span.marker))
            .collect();
        let mut order = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for span in &spans {
            if seen.insert(span.marker) {
                order.push(span.marker);
            }
        }
        if order.is_empty() {
            self.pending_images.retain(|image| image.marker.is_none());
            self.pending_image_jobs.retain(|job| job.marker.is_none());
            self.image_marker = 0;
            return;
        }
        let mapping: std::collections::HashMap<u32, u32> = order
            .iter()
            .enumerate()
            .map(|(index, marker)| (*marker, index as u32 + 1))
            .collect();
        self.pending_images.retain(|image| {
            image
                .marker
                .is_none_or(|marker| mapping.contains_key(&marker))
        });
        self.pending_image_jobs
            .retain(|job| job.marker.is_none_or(|marker| mapping.contains_key(&marker)));
        for image in &mut self.pending_images {
            if let Some(marker) = image.marker
                && let Some(&new) = mapping.get(&marker)
            {
                image.marker = Some(new);
                image.image.marker = Some(new);
            }
        }
        for job in &mut self.pending_image_jobs {
            if let Some(marker) = job.marker
                && let Some(&new) = mapping.get(&marker)
            {
                job.marker = Some(new);
                job.token = format!("[image {new}]");
            }
        }
        self.image_marker = mapping.values().copied().max().unwrap_or(0);

        let mut replacements = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for span in &spans {
            if !seen.insert(span.marker) {
                continue;
            }
            let Some(&new) = mapping.get(&span.marker) else {
                continue;
            };
            let new_token = format!("[image {new}]");
            let old_token = char_slice(&text, span.start, span.end);
            if old_token != new_token {
                replacements.push((span.start, span.end, new_token));
            }
        }
        if replacements.is_empty() {
            return;
        }
        let cursor = self.cursor_char();
        for (start, end, token) in replacements.iter().rev() {
            self.select_chars(*start, *end);
            self.input.insert_str(token);
        }
        let mut shift: isize = 0;
        let mut mapped = None;
        for (start, end, token) in &replacements {
            let delta = token.chars().count() as isize - (*end - *start) as isize;
            if cursor >= *end {
                shift += delta;
            } else if cursor > *start {
                mapped = Some(*start as isize + shift + token.chars().count() as isize);
                break;
            } else {
                mapped = Some(cursor as isize + shift);
                break;
            }
        }
        let new_cursor = mapped.unwrap_or(cursor as isize + shift).max(0) as usize;
        let lines = self.input.lines().to_vec();
        let (row, col) = pos_from_offset(&lines, new_cursor);
        self.input
            .move_cursor(tui_textarea::CursorMove::Jump(row as u16, col as u16));
        self.dirty = true;
    }

    /// True while an image named by this text is still being decoded.
    pub(super) fn has_pending_images_in(&self, text: &str) -> bool {
        self.pending_image_jobs
            .iter()
            .any(|job| job.epoch == self.image_epoch && text.contains(&job.token))
    }

    /// Drafts whose token — `[image N]` or a path — survived editing, in
    /// text order. Used when a turn is queued: the queue keeps the token so
    /// the draft can be rebuilt on the next submit.
    pub(super) fn take_submitted_drafts(&mut self, text: &str) -> Vec<PendingImage> {
        let mut out: Vec<(usize, PendingImage)> = Vec::new();
        for pending in self.pending_images.drain(..) {
            if let Some(pos) = text.find(&pending.token) {
                out.push((pos, pending));
            }
        }
        out.sort_by_key(|(pos, _)| *pos);
        out.into_iter().map(|(_, pending)| pending).collect()
    }

    /// Attachments whose token survived editing. Tests use this shorthand;
    /// the submit path keeps the richer drafts for the queue.
    #[cfg(test)]
    pub(super) fn take_submitted_images(&mut self, text: &str) -> Vec<ImageAttachment> {
        self.take_submitted_drafts(text)
            .into_iter()
            .map(|pending| pending.image)
            .collect()
    }

    /// Whether at least one drafted image still has its token in `text`.
    pub(super) fn has_attached_images(&self, text: &str) -> bool {
        self.pending_images
            .iter()
            .any(|pending| text.contains(&pending.token))
    }

    /// A new composition: markers restart at 1, and late worker results from
    /// the previous composition are ignored by epoch.
    pub(super) fn reset_image_draft(&mut self) {
        self.image_epoch = self.image_epoch.wrapping_add(1);
        self.image_marker = 0;
        self.pending_images.clear();
        self.pending_image_jobs.clear();
        self.last_drop_text.clear();
    }

    fn new_image_marker(&mut self) -> Option<u32> {
        if self.model_cfg.vision == Some(false) {
            self.status("this model does not accept images", StatusKind::Err);
            return None;
        }
        if self.pending_images.len() + self.pending_image_jobs.len() >= MAX_IMAGES {
            self.status(
                &format!("image limit is {MAX_IMAGES} per message"),
                StatusKind::Warn,
            );
            return None;
        }
        self.image_marker += 1;
        Some(self.image_marker)
    }

    fn attach_paths(&mut self, paths: Vec<PathBuf>) {
        if self.model_cfg.vision == Some(false) {
            self.status("this model does not accept images", StatusKind::Err);
            return;
        }
        let free = MAX_IMAGES.saturating_sub(self.pending_images.len() + self.pending_image_jobs.len());
        let total = paths.len();
        let mut jobs = Vec::new();
        for path in paths {
            if jobs.len() >= free {
                break;
            }
            let token = path.to_string_lossy().to_string();
            if self.has_path_token(&token) {
                continue;
            }
            jobs.push((token, path));
        }
        if jobs.is_empty() {
            if free == 0 && total > 0 {
                self.status(
                    &format!("image limit is {MAX_IMAGES} per message"),
                    StatusKind::Warn,
                );
            }
            return;
        }
        let tokens: Vec<String> = jobs
            .iter()
            .map(|(token, _)| quoted_path(token))
            .collect();
        self.expand_marker_selection();
        self.input.insert_str(tokens.join(" "));
        let attached = jobs.len();
        for (token, path) in jobs {
            self.track_job(None, token, ImageSource::File(path));
        }
        if total > attached && attached >= free {
            self.status(
                &format!("attached {attached} of {total} images — limit is {MAX_IMAGES}"),
                StatusKind::Warn,
            );
        }
        self.dirty = true;
    }

    fn has_path_token(&self, token: &str) -> bool {
        self.pending_images
            .iter()
            .any(|pending| pending.marker.is_none() && pending.token == token)
            || self
                .pending_image_jobs
                .iter()
                .any(|job| job.marker.is_none() && job.token == token)
    }

    fn track_job(&mut self, marker: Option<u32>, token: String, source: ImageSource) {
        let epoch = self.image_epoch;
        let id = self.next_image_job;
        self.next_image_job = self.next_image_job.wrapping_add(1);
        self.pending_image_jobs.push(PendingImageJob {
            id,
            epoch,
            marker,
            token,
        });
        let tx = self.image_jobs_tx.clone();
        std::thread::spawn(move || {
            let result = prepare_source(source, marker);
            let _ = tx.send(ImageJobResult { epoch, id, result });
        });
    }

    fn remove_input_token(&mut self, token: &str) -> bool {
        let text = self.input_text();
        let Some(pos) = text.find(token) else {
            return false;
        };
        let before = &text[..pos];
        let row = before.matches('\n').count() as u16;
        let col = before.rsplit('\n').next().unwrap_or("").chars().count() as u16;
        let mut replaced = String::with_capacity(text.len().saturating_sub(token.len()));
        replaced.push_str(before);
        replaced.push_str(&text[pos + token.len()..]);
        self.input = Self::fresh_input(replaced);
        self.input
            .move_cursor(tui_textarea::CursorMove::Jump(row, col));
        true
    }

    /// The trailing run of absolute image paths in the composer, with the byte
    /// range they occupy. Used for drops delivered as typed characters.
    fn trailing_dropped_paths(&self, text: &str) -> Option<DroppedPaths> {
        let trimmed = text.trim_end();
        if trimmed.is_empty() {
            return None;
        }
        let end = trimmed.len();
        let mut cursor = end;
        let mut paths = Vec::new();
        while let Some((token_start, token)) = trailing_token(&text[..cursor]) {
            let path = PathBuf::from(&token);
            // Extension first: an arbitrary absolute path must not cost a
            // filesystem stat on every keystroke.
            if !path.is_absolute() || !is_image_path(&path) || !path.is_file() {
                break;
            }
            paths.push((token, path));
            cursor = token_start;
        }
        if paths.is_empty() {
            return None;
        }
        paths.reverse();
        Some((cursor, end, paths))
    }

    pub(super) fn pasted_image_paths(&self, text: &str) -> Option<Vec<PathBuf>> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        // A single path may contain spaces without quotes (a copied path, a
        // drop from a terminal that does not quote). Try the whole payload
        // first, the way opencode's pasted-attachment resolver does.
        if let Some(path) = self.resolve_image_path(unquote_single(trimmed)) {
            return Some(vec![path]);
        }
        let tokens = split_pasted_paths(trimmed)?;
        let mut paths = Vec::new();
        for token in tokens {
            paths.push(self.resolve_image_path(&token)?);
        }
        Some(paths)
    }

    fn resolve_image_path(&self, token: &str) -> Option<PathBuf> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        let path = PathBuf::from(token);
        let path = if path.is_absolute() {
            path
        } else {
            self.project_root.join(path)
        };
        (is_image_path(&path) && path.is_file()).then_some(path)
    }
}

fn prepare_source(source: ImageSource, marker: Option<u32>) -> Result<ImageAttachment, String> {
    let (bytes, name) = match source {
        ImageSource::File(path) => {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "image".to_string());
            let bytes = std::fs::read(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            (bytes, name)
        }
        ImageSource::Clipboard {
            width,
            height,
            bytes,
        } => (rgba_to_png(width, height, bytes)?, "pasted.png".to_string()),
    };
    let prepared = crate::media::prepare(&bytes, &name)?;
    crate::media::store(&prepared)?;
    let mut image = prepared.attachment;
    image.marker = marker;
    Ok(image)
}

fn rgba_to_png(width: usize, height: usize, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    let image = image::RgbaImage::from_raw(width as u32, height as u32, bytes)
        .ok_or_else(|| "clipboard image has unexpected dimensions".to_string())?;
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut out),
            image::ImageFormat::Png,
        )
        .map_err(|e| format!("cannot encode clipboard image: {e}"))?;
    Ok(out)
}

/// Split a paste into path tokens, honouring single and double quotes. The
/// whole paste must be paths: one stray word and this is ordinary text.
fn split_pasted_paths(text: &str) -> Option<Vec<String>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in text.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => quote = Some(c),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            (None, c) => current.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    (!tokens.is_empty()).then_some(tokens)
}

/// The last whitespace-separated (or quoted) token of `text` and its byte
/// offset. `text` must not end with whitespace; an empty or unquoted tail is
/// returned as-is.
fn trailing_token(text: &str) -> Option<(usize, String)> {
    let text = text.trim_end();
    if text.is_empty() {
        return None;
    }
    let last = text.chars().last()?;
    if last == '"' || last == '\'' {
        let before = &text[..text.len() - last.len_utf8()];
        let open = before.rfind(last)?;
        let token = &text[open + last.len_utf8()..text.len() - last.len_utf8()];
        return (!token.is_empty()).then(|| (open, token.to_string()));
    }
    let start = match text.char_indices().rev().find(|(_, c)| c.is_whitespace()) {
        Some((index, c)) => index + c.len_utf8(),
        None => 0,
    };
    let token = &text[start..];
    if token.starts_with(['"', '\'']) {
        // An opening quote with no closing one is an unfinished drop.
        return None;
    }
    (!token.is_empty()).then(|| (start, token.to_string()))
}

/// A path token as it is inserted into the composer. Quotes only when the
/// path contains whitespace; the stored job token stays unquoted so a quoted
/// and an unquoted spelling of the same file dedupe together.
fn quoted_path(token: &str) -> String {
    if token.chars().any(char::is_whitespace) {
        format!("\"{token}\"")
    } else {
        token.to_string()
    }
}

/// Strip one pair of matching surrounding quotes, if present.
fn unquote_single(text: &str) -> &str {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'') && first == last {
            return &text[1..text.len() - 1];
        }
    }
    text
}

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drop_may_be_one_or_many_quoted_paths() {
        assert_eq!(
            split_pasted_paths(r#""C:\a b\shot.png""#),
            Some(vec![r"C:\a b\shot.png".to_string()])
        );
        assert_eq!(
            split_pasted_paths(r#""a.png" "b.jpg""#),
            Some(vec!["a.png".to_string(), "b.jpg".to_string()])
        );
    }

    #[test]
    fn the_splitter_is_quote_aware_but_not_a_semantic_check() {
        // Whether the tokens are existing image files is decided against the
        // filesystem later; the splitter only tokenizes.
        assert_eq!(
            split_pasted_paths("look at a.png"),
            Some(vec![
                "look".to_string(),
                "at".to_string(),
                "a.png".to_string()
            ])
        );
        assert_eq!(split_pasted_paths(""), None);
        assert_eq!(split_pasted_paths("\"unclosed"), None);
    }

    #[test]
    fn marker_at_char_hits_only_the_token() {
        assert_eq!(marker_at_char("› hi [image 2] x", 6), Some(2));
        assert_eq!(marker_at_char("› hi [image 2] x", 0), None);
        assert_eq!(marker_at_char("› hi", 3), None);
    }

    #[test]
    fn marker_scanner_is_exact_about_the_shape() {
        assert_eq!(marker_char_spans("a [image 12] b"), vec![(2, 12)]);
        assert!(
            marker_char_spans("[imaage 1] [image x] [image ]").is_empty(),
            "only exact [image <digits>] tokens are markers"
        );
    }

    #[test]
    fn trailing_token_understands_quotes_and_plain_tails() {
        assert_eq!(
            trailing_token(r#""C:\a b\x.png""#),
            Some((0, r"C:\a b\x.png".to_string()))
        );
        assert_eq!(
            trailing_token(r#"hello C:\a.png"#),
            Some((6, r"C:\a.png".to_string()))
        );
        assert_eq!(trailing_token("   "), None);
        assert_eq!(trailing_token("\"unclosed"), None);
    }

    #[test]
    fn rgba_clipboard_round_trips_through_png() {
        let rgba = vec![255u8; 4 * 8 * 4];
        let png = rgba_to_png(8, 4, rgba).unwrap();
        assert!(crate::media::is_supported_image(&png));
        assert!(rgba_to_png(8, 4, vec![0; 3]).is_err());
    }
}
