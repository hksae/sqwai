//! User image attachments: paste/drop entry, `[image N]` markers, and
//! background normalization.
//!
//! The composer holds plain text. Markers name attachments in the draft; the
//! attachment list is separate. At submit only attachments whose marker is
//! still in the text survive. Decoding and resizing run on a worker thread so
//! a large screenshot never blocks input; a failure removes the token again.

use super::*;
use crate::providers::ImageAttachment;
use std::path::{Path, PathBuf};

/// Per-message attachment cap.
pub(super) const MAX_IMAGES: usize = 20;

#[derive(Debug)]
pub(super) struct QueuedPrompt {
    pub text: String,
    pub images: Vec<ImageAttachment>,
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

pub(super) struct PendingImageJob {
    epoch: u64,
    token: String,
}

pub(super) struct ImageJobResult {
    epoch: u64,
    token: String,
    result: Result<ImageAttachment, String>,
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
    /// paths (a terminal drag&drop, usually quoted) becomes markers; anything
    /// else is ordinary text.
    pub(super) fn paste_input_text(&mut self, text: &str) {
        if self.image_entry_allowed()
            && let Some(paths) = self.pasted_image_paths(text)
        {
            self.attach_paths(paths);
            return;
        }
        self.paste_text(text);
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
        let token = format!("[image {marker}]");
        self.input.insert_str(&token);
        self.track_job(
            marker,
            token,
            ImageSource::Clipboard {
                width,
                height,
                bytes,
            },
        );
        self.dirty = true;
        true
    }

    pub(super) fn poll_image_jobs(&mut self) {
        loop {
            match self.image_jobs_rx.try_recv() {
                Ok(job) => {
                    if job.epoch != self.image_epoch {
                        continue;
                    }
                    self.pending_image_jobs
                        .retain(|pending| pending.token != job.token);
                    match job.result {
                        Ok(image) => {
                            if self.input_text().contains(&job.token) {
                                self.status(
                                    &format!(
                                        "attached {} · {}×{}",
                                        image.name, image.width, image.height
                                    ),
                                    StatusKind::Ok,
                                );
                                self.pending_images.push(image);
                                self.dirty = true;
                            }
                        }
                        Err(error) => {
                            if self.remove_input_token(&job.token) {
                                self.dirty = true;
                            }
                            self.status(&error, StatusKind::Err);
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
    }

    /// True while an image named by this text is still being decoded.
    pub(super) fn has_pending_images_in(&self, text: &str) -> bool {
        self.pending_image_jobs
            .iter()
            .any(|job| job.epoch == self.image_epoch && text.contains(&job.token))
    }

    /// Attachments whose markers survived editing, for the message about to
    /// be stored. Drains the draft; call after the vision gate.
    pub(super) fn take_submitted_images(&mut self, text: &str) -> Vec<ImageAttachment> {
        let mut out = Vec::new();
        for image in self.pending_images.drain(..) {
            let Some(marker) = image.marker else {
                continue;
            };
            if text.contains(&format!("[image {marker}]")) {
                out.push(image);
            }
        }
        out
    }

    /// Whether at least one drafted image still has its marker in `text`.
    pub(super) fn has_marked_images(&self, text: &str) -> bool {
        self.pending_images.iter().any(|image| {
            image
                .marker
                .is_some_and(|marker| text.contains(&format!("[image {marker}]")))
        })
    }

    /// A new composition: markers restart at 1, and late worker results from
    /// the previous composition are ignored by epoch.
    pub(super) fn reset_image_draft(&mut self) {
        self.image_epoch = self.image_epoch.wrapping_add(1);
        self.image_marker = 0;
        self.pending_images.clear();
        self.pending_image_jobs.clear();
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
        let accepted: Vec<PathBuf> = paths.into_iter().take(free).collect();
        if accepted.is_empty() {
            self.status(
                &format!("image limit is {MAX_IMAGES} per message"),
                StatusKind::Warn,
            );
            return;
        }
        let mut tokens = Vec::with_capacity(accepted.len());
        let mut jobs = Vec::with_capacity(accepted.len());
        for path in accepted {
            self.image_marker += 1;
            let marker = self.image_marker;
            tokens.push(format!("[image {marker}]"));
            jobs.push((marker, path));
        }
        self.input.insert_str(tokens.join(" "));
        for (marker, path) in jobs {
            let token = format!("[image {marker}]");
            self.track_job(marker, token, ImageSource::File(path));
        }
        if total > free {
            self.status(
                &format!("attached {free} of {total} images — limit is {MAX_IMAGES}"),
                StatusKind::Warn,
            );
        }
        self.dirty = true;
    }

    fn track_job(&mut self, marker: u32, token: String, source: ImageSource) {
        let epoch = self.image_epoch;
        self.pending_image_jobs.push(PendingImageJob {
            epoch,
            token: token.clone(),
        });
        let tx = self.image_jobs_tx.clone();
        std::thread::spawn(move || {
            let result = prepare_source(source, marker);
            let _ = tx.send(ImageJobResult {
                epoch,
                token,
                result,
            });
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

    pub(super) fn pasted_image_paths(&self, text: &str) -> Option<Vec<PathBuf>> {
        let tokens = split_pasted_paths(text)?;
        let mut paths = Vec::new();
        for token in tokens {
            let path = PathBuf::from(&token);
            let path = if path.is_absolute() {
                path
            } else {
                self.project_root.join(path)
            };
            if !path.is_file() || !is_image_path(&path) {
                return None;
            }
            paths.push(path);
        }
        Some(paths)
    }
}

fn prepare_source(source: ImageSource, marker: u32) -> Result<ImageAttachment, String> {
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
    image.marker = Some(marker);
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
    fn rgba_clipboard_round_trips_through_png() {
        let rgba = vec![255u8; 4 * 8 * 4];
        let png = rgba_to_png(8, 4, rgba).unwrap();
        assert!(crate::media::is_supported_image(&png));
        assert!(rgba_to_png(8, 4, vec![0; 3]).is_err());
    }
}
