//! `view_document`: hand a local PDF to the model as a document.
//!
//! A PDF is not text: scans have no text layer, and tables and charts live as
//! layout. Providers accept it natively (Anthropic `document`, Responses
//! `input_file`, Chat Completions `file`), so the original bytes travel as a
//! document attachment. Page ranges are sliced with `lopdf` instead of being
//! rendered, which keeps text and images intact.

use super::{Outcome, ToolCtx};
use crate::media;
use crate::pdf;
use serde_json::Value;

pub(super) fn view(ctx: &mut ToolCtx, args: &Value) -> Outcome {
    if !ctx.pdf {
        return Outcome::err(
            "this model does not accept PDF documents (pdf is not enabled: set `pdf = true` \
             in the model's config entry, or its catalog entry has not declared it); convert \
             the PDF to text or switch to a document-capable model",
        );
    }
    let raw = args["path"]
        .as_str()
        .or_else(|| args["file_path"].as_str())
        .unwrap_or_default()
        .trim();
    if raw.is_empty() {
        return Outcome::err("view_document needs a `path` to a PDF inside the project");
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
    let pages = if let Some(spec) = args["pages"].as_str() {
        spec.trim().to_string()
    } else if let Some(page) = args["pages"].as_u64() {
        page.to_string()
    } else {
        String::new()
    };
    let pages = (!pages.is_empty()).then_some(pages.as_str());
    let prepared = match pdf::prepare(&bytes, &path.to_string_lossy(), pages) {
        Ok(prepared) => prepared,
        Err(e) => return Outcome::err(e),
    };
    let attachment = prepared.attachment.clone();
    if ctx.visible_documents.contains(&attachment.id) {
        return Outcome::ok(format!(
            "already in context · {} — no second copy attached",
            page_label(&attachment)
        ));
    }
    if let Err(e) = media::store_document(&attachment, &prepared.bytes) {
        return Outcome::err(e);
    }
    // The call line already names the tool, the path and the pages; repeat
    // only what it does not say.
    let summary = format!(
        "{} · {} · {} · {} tokens",
        page_label(&attachment),
        attachment.media_type,
        size_label(attachment.bytes),
        pdf::tokens_label(attachment.pages)
    );
    Outcome::ok(summary).with_documents(vec![attachment])
}

fn page_label(document: &crate::providers::DocumentAttachment) -> String {
    match &document.range {
        Some(range) => format!("pages {range} of {}", document.total_pages),
        None => format!(
            "{} page{}",
            document.pages,
            if document.pages == 1 { "" } else { "s" }
        ),
    }
}

fn size_label(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn project(pages: u32) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc.pdf"), pdf::test_pdf(pages)).unwrap();
        dir
    }

    #[test]
    fn views_a_small_document_and_stores_it() {
        let dir = project(3);
        let mut ctx = ToolCtx::new(dir.path()).with_pdf(true);
        let out = view(&mut ctx, &json!({"path": "doc.pdf"}));
        assert!(out.ok, "{}", out.output);
        assert_eq!(out.documents.len(), 1);
        let document = &out.documents[0];
        assert_eq!((document.pages, document.total_pages), (3, 3));
        assert_eq!(document.media_type, "application/pdf");
        assert_eq!(document.name, "doc.pdf");
        assert!(out.output.contains("3 pages"), "{}", out.output);
        assert!(out.output.contains("tokens"), "{}", out.output);
        assert!(
            !out.output.contains("doc.pdf"),
            "the call line already names the file: {}",
            out.output
        );
        assert!(crate::media::load_document(document).is_ok());
    }

    #[test]
    fn a_page_range_slices_the_document() {
        let dir = project(6);
        let mut ctx = ToolCtx::new(dir.path()).with_pdf(true);
        let out = view(&mut ctx, &json!({"path": "doc.pdf", "pages": "2,4-5"}));
        assert!(out.ok, "{}", out.output);
        let document = &out.documents[0];
        assert_eq!((document.pages, document.total_pages), (3, 6));
        assert_eq!(document.range.as_deref(), Some("2,4-5"));
        assert!(out.output.contains("pages 2,4-5 of 6"), "{}", out.output);
        let bytes = crate::media::load_document(document).unwrap();
        let sliced = lopdf::Document::load_mem(&bytes).unwrap();
        assert_eq!(sliced.get_pages().len(), 3);
    }

    #[test]
    fn repeat_view_returns_a_pointer_not_a_copy() {
        let dir = project(2);
        let mut ctx = ToolCtx::new(dir.path()).with_pdf(true);
        let first = view(&mut ctx, &json!({"path": "doc.pdf"}));
        assert!(first.ok);
        ctx.note_visible_document(&first.documents[0].id);
        let second = view(&mut ctx, &json!({"path": "doc.pdf"}));
        assert!(second.ok);
        assert!(second.documents.is_empty());
        assert!(
            second.output.contains("already in context"),
            "{}",
            second.output
        );
    }

    #[test]
    fn a_model_without_pdf_capability_is_blocked_before_any_request() {
        let dir = project(2);
        let mut ctx = ToolCtx::new(dir.path());
        let out = view(&mut ctx, &json!({"path": "doc.pdf"}));
        assert!(!out.ok);
        assert!(out.documents.is_empty());
        assert!(out.output.contains("does not accept PDF"), "{}", out.output);
    }

    #[test]
    fn a_large_document_requires_pages() {
        let dir = project(12);
        let mut ctx = ToolCtx::new(dir.path()).with_pdf(true);
        let out = view(&mut ctx, &json!({"path": "doc.pdf"}));
        assert!(!out.ok);
        assert!(out.output.contains("pass pages"), "{}", out.output);
    }

    #[test]
    fn read_points_at_view_document() {
        let dir = project(2);
        let mut ctx = ToolCtx::new(dir.path());
        let out = super::super::fs::read(&mut ctx, "doc.pdf", &json!({}));
        assert!(!out.ok);
        assert!(out.output.contains("view_document"), "{}", out.output);
    }

    #[test]
    fn paths_outside_the_project_are_refused() {
        let outside = tempfile::tempdir().unwrap();
        let document = outside.path().join("secret.pdf");
        std::fs::write(&document, pdf::test_pdf(1)).unwrap();
        let dir = project(1);
        let mut ctx = ToolCtx::new(dir.path()).with_pdf(true);
        let out = view(&mut ctx, &json!({"path": document.to_string_lossy()}));
        assert!(!out.ok);
        assert!(out.output.contains("escapes the project"), "{}", out.output);
    }
}
