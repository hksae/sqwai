//! `view_image`: hand a local image file to the model as pixels.
//!
//! `read` is a text tool and refuses binary files. This is the one route for
//! an agent to see a screenshot, photo or diagram: the file is decoded,
//! normalized to a bounded rendition, stored by content hash, and attached to
//! the tool result. Repeat views of the same pixels return a pointer while the
//! earlier copy is still visible in context.

use super::{Outcome, ToolCtx};
use crate::media;
use serde_json::Value;

pub(super) fn view(ctx: &mut ToolCtx, args: &Value) -> Outcome {
    if !ctx.vision {
        return Outcome::err(
            "this model does not accept images (vision = false in its catalog entry); \
             switch to a vision model or describe the image in text",
        );
    }
    let raw = args["path"]
        .as_str()
        .or_else(|| args["file_path"].as_str())
        .unwrap_or_default()
        .trim();
    if raw.is_empty() {
        return Outcome::err("view_image needs a `path` to an image inside the project");
    }
    let path = match ctx.resolve(raw) {
        Ok(path) => path,
        Err(e) => return Outcome::err(e),
    };
    if !path.exists() {
        return Outcome::err(format!("file not found: {}", path.display()));
    }
    if !path.is_file() {
        return Outcome::err(format!("not a file: {}", path.display()));
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => return Outcome::err(format!("cannot read {}: {e}", path.display())),
    };
    let edge = args["max_edge"]
        .as_u64()
        .map(|v| (v as u32).clamp(media::MIN_VIEW_EDGE, media::MAX_EDGE))
        .unwrap_or(media::MAX_EDGE);
    let prepared = match media::prepare_at(&bytes, &path.to_string_lossy(), edge) {
        Ok(prepared) => prepared,
        Err(e) => return Outcome::err(e),
    };
    let attachment = prepared.attachment.clone();
    if ctx.visible_images.contains(&attachment.id) {
        return Outcome::ok(format!(
            "already in context · {}×{} — no second copy attached",
            attachment.width, attachment.height
        ));
    }
    if let Err(e) = media::store(&prepared) {
        return Outcome::err(e);
    }
    // The call line already names the tool and the path; repeat only what it
    // does not say.
    let summary = format!(
        "{}×{} · {} · {} KB · {} tokens",
        attachment.width,
        attachment.height,
        attachment.media_type,
        attachment.bytes.div_ceil(1024),
        media::tokens_label(attachment.width, attachment.height)
    );
    Outcome::ok(summary).with_images(vec![attachment])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.png"), crate::media::test_png(120, 80)).unwrap();
        dir
    }

    #[test]
    fn views_an_image_and_stores_it() {
        let dir = project();
        let mut ctx = ToolCtx::new(dir.path());
        let out = view(&mut ctx, &json!({"path": "shot.png"}));
        assert!(out.ok, "{}", out.output);
        assert_eq!(out.images.len(), 1);
        let image = &out.images[0];
        assert_eq!((image.width, image.height), (120, 80));
        assert_eq!(image.name, "shot.png");
        assert!(out.output.contains("120×80"), "{}", out.output);
        assert!(out.output.contains("tokens"), "{}", out.output);
        assert!(!out.output.contains("view_image:"), "{}", out.output);
        assert!(
            !out.output.contains("shot.png"),
            "the call line already names the file: {}",
            out.output
        );
        assert!(crate::media::load(image).is_ok());
    }

    /// A cheap overview must not be mistaken for the full copy already in
    /// context: the renditions differ, so both travel.
    #[test]
    fn a_small_edge_is_its_own_rendition() {
        let dir = project();
        std::fs::write(
            dir.path().join("big.png"),
            crate::media::test_png(1600, 900),
        )
        .unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        let overview = view(&mut ctx, &json!({"path": "big.png", "max_edge": 400}));
        assert!(overview.ok, "{}", overview.output);
        assert_eq!((overview.images[0].width, overview.images[0].height), (400, 225));
        ctx.note_visible_image(&overview.images[0].id);

        let full = view(&mut ctx, &json!({"path": "big.png"}));
        assert!(full.ok, "{}", full.output);
        assert_eq!(full.images.len(), 1, "the full copy must still travel");
        assert_eq!(full.images[0].width, 1568);
    }

    #[test]
    fn repeat_view_returns_a_pointer_not_a_copy() {
        let dir = project();
        let mut ctx = ToolCtx::new(dir.path());
        let first = view(&mut ctx, &json!({"path": "shot.png"}));
        assert!(first.ok);
        ctx.note_visible_image(&first.images[0].id);
        let second = view(&mut ctx, &json!({"path": "shot.png"}));
        assert!(second.ok);
        assert!(second.images.is_empty());
        assert!(
            second.output.contains("already in context"),
            "{}",
            second.output
        );
    }

    #[test]
    fn a_declared_text_only_model_is_blocked_before_any_request() {
        let dir = project();
        let mut ctx = ToolCtx::new(dir.path()).with_vision(false);
        let out = view(&mut ctx, &json!({"path": "shot.png"}));
        assert!(!out.ok);
        assert!(out.images.is_empty());
        assert!(
            out.output.contains("does not accept images"),
            "{}",
            out.output
        );
    }

    #[test]
    fn read_points_at_view_image() {
        let dir = project();
        let mut ctx = ToolCtx::new(dir.path());
        let out = super::super::fs::read(&mut ctx, "shot.png", &json!({}));
        assert!(!out.ok);
        assert!(out.output.contains("view_image"), "{}", out.output);
    }

    #[test]
    fn paths_outside_the_project_are_refused() {
        let outside = tempfile::tempdir().unwrap();
        let image = outside.path().join("secret.png");
        std::fs::write(&image, crate::media::test_png(8, 8)).unwrap();
        let dir = project();
        let mut ctx = ToolCtx::new(dir.path());
        let out = view(&mut ctx, &json!({"path": image.to_string_lossy()}));
        assert!(!out.ok);
        assert!(out.output.contains("escapes the project"), "{}", out.output);
    }
}
