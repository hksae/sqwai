//! PDF validation, page-range parsing, and slicing.
//!
//! A PDF is a document, not text: scans have no text layer, tables and charts
//! live as layout. Providers accept it as a native document/file, so sqwai
//! stores the original bytes and, for a page range, a sliced PDF containing
//! only those pages. Slicing with `lopdf` keeps text and images intact and
//! needs no external renderer.

use crate::providers::DocumentAttachment;

/// Raw source cap for a PDF, before provider request limits.
pub const MAX_PDF_BYTES: u64 = 20 * 1024 * 1024;
/// Pages a single read may request, matching Claude Code's tool budget.
pub const MAX_PAGES_PER_READ: u32 = 20;
/// A whole PDF may be read without `pages` only up to this size.
pub const MAX_WHOLE_PAGES: u32 = 10;
/// Conservative context cost per page (text + page image) for pressure and display.
pub const TOKENS_PER_PAGE: u64 = 2500;

#[derive(Debug)]
pub struct PreparedDocument {
    pub attachment: DocumentAttachment,
    pub bytes: Vec<u8>,
}

pub fn is_pdf(bytes: &[u8]) -> bool {
    bytes.windows(5).take(1024).any(|window| window == b"%PDF-")
}

/// Parse a 1-indexed page spec such as `"1-5"`, `"3"`, or `"1-5,8,10-12"`.
pub fn parse_ranges(spec: &str, total_pages: u32) -> Result<Vec<u32>, String> {
    let mut pages: Vec<u32> = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (start, end) = match part.split_once('-') {
            Some((start, end)) => (parse_page(start, part)?, parse_page(end, part)?),
            None => {
                let page = parse_page(part, part)?;
                (page, page)
            }
        };
        if start > end {
            return Err(format!(
                "invalid range '{part}': start page is after end page"
            ));
        }
        if end > total_pages {
            return Err(format!("page {end} is outside this PDF (1-{total_pages})"));
        }
        for page in start..=end {
            if !pages.contains(&page) {
                pages.push(page);
            }
        }
    }
    pages.sort_unstable();
    if pages.is_empty() {
        return Err("pages is empty".to_string());
    }
    if pages.len() > MAX_PAGES_PER_READ as usize {
        return Err(format!(
            "at most {MAX_PAGES_PER_READ} pages per read; {spec} selects {}",
            pages.len()
        ));
    }
    Ok(pages)
}

fn parse_page(raw: &str, part: &str) -> Result<u32, String> {
    let raw = raw.trim();
    let page: u32 = raw
        .parse()
        .map_err(|_| format!("invalid page number '{raw}' in '{part}'"))?;
    if page == 0 {
        return Err(format!("pages are 1-indexed; '{part}' contains 0"));
    }
    Ok(page)
}

/// `[1,2,3,5]` -> `"1-3,5"`.
pub fn range_label(pages: &[u32]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut index = 0;
    while index < pages.len() {
        let start = pages[index];
        let mut end = start;
        while index + 1 < pages.len() && pages[index + 1] == end + 1 {
            index += 1;
            end = pages[index];
        }
        if start == end {
            out.push(start.to_string());
        } else {
            out.push(format!("{start}-{end}"));
        }
        index += 1;
    }
    out.join(",")
}

/// Validate the PDF and produce either the original bytes (a whole read) or a
/// sliced document for the requested pages.
pub fn prepare(source: &[u8], name: &str, pages: Option<&str>) -> Result<PreparedDocument, String> {
    if source.is_empty() {
        return Err("PDF file is empty".to_string());
    }
    if !is_pdf(source) {
        return Err("not a PDF (missing the %PDF- header)".to_string());
    }
    if source.len() as u64 > MAX_PDF_BYTES {
        return Err(format!(
            "PDF is {:.1} MB — the limit is {} MB",
            source.len() as f64 / (1024.0 * 1024.0),
            MAX_PDF_BYTES / (1024 * 1024)
        ));
    }
    let mut document =
        lopdf::Document::load_mem(source).map_err(|e| format!("cannot parse PDF: {e}"))?;
    if document.is_encrypted() {
        return Err("password-protected PDFs are not supported".to_string());
    }
    let total_pages = document.get_pages().len() as u32;
    if total_pages == 0 {
        return Err("PDF has no pages".to_string());
    }

    let (bytes, selected_pages, range) = match pages.map(str::trim).filter(|s| !s.is_empty()) {
        None => {
            if total_pages > MAX_WHOLE_PAGES {
                return Err(format!(
                    "PDF has {total_pages} pages; pass pages (e.g. \"1-5\", max \
                     {MAX_PAGES_PER_READ} per read) instead of reading it whole"
                ));
            }
            (source.to_vec(), total_pages, None)
        }
        Some(spec) => {
            let selected = parse_ranges(spec, total_pages)?;
            let label = range_label(&selected);
            if selected.len() == total_pages as usize {
                (source.to_vec(), total_pages, Some(label))
            } else {
                let keep: std::collections::HashSet<u32> = selected.iter().copied().collect();
                let remove: Vec<u32> = document
                    .get_pages()
                    .keys()
                    .copied()
                    .filter(|page| !keep.contains(page))
                    .collect();
                document.delete_pages(&remove);
                document.prune_objects();
                document.renumber_objects();
                let mut out = Vec::new();
                document
                    .save_to(&mut out)
                    .map_err(|e| format!("cannot slice PDF: {e}"))?;
                if out.len() as u64 > MAX_PDF_BYTES {
                    return Err(format!(
                        "selected pages are {:.1} MB — the limit is {} MB",
                        out.len() as f64 / (1024.0 * 1024.0),
                        MAX_PDF_BYTES / (1024 * 1024)
                    ));
                }
                (out, selected.len() as u32, Some(label))
            }
        }
    };

    let attachment = DocumentAttachment {
        id: crate::media::hex_digest(&bytes),
        name: crate::media::clean_name(name),
        media_type: "application/pdf".to_string(),
        bytes: bytes.len() as u64,
        pages: selected_pages,
        total_pages,
        range,
    };
    Ok(PreparedDocument { attachment, bytes })
}

pub fn document_tokens(pages: u32) -> u64 {
    u64::from(pages) * TOKENS_PER_PAGE
}

pub fn tokens_label(pages: u32) -> String {
    let tokens = document_tokens(pages);
    if tokens >= 1000 {
        format!("~{:.1}k", tokens as f64 / 1000.0)
    } else {
        format!("~{tokens}")
    }
}

#[cfg(test)]
pub fn test_pdf(pages: u32) -> Vec<u8> {
    use lopdf::content::{Content, Operation};
    use lopdf::{Document, Object, Stream, dictionary};

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Courier",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });
    let content = Content {
        operations: vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![20.into(), 20.into()]),
            Operation::new("Tj", vec![Object::string_literal("page")]),
            Operation::new("ET", vec![]),
        ],
    };
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
    let mut kids = Vec::new();
    for _ in 0..pages {
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        kids.push(page_id.into());
    }
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(kids),
            "Count" => i64::from(pages),
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_ranges_are_one_indexed_and_ordered() {
        assert_eq!(parse_ranges("3", 5).unwrap(), vec![3]);
        assert_eq!(parse_ranges("1-3,5", 5).unwrap(), vec![1, 2, 3, 5]);
        assert_eq!(parse_ranges("5,1-2", 5).unwrap(), vec![1, 2, 5]);
        assert!(parse_ranges("0-2", 5).is_err());
        assert!(parse_ranges("3-1", 5).is_err());
        assert!(parse_ranges("1-6", 5).is_err());
        assert!(parse_ranges("1-21", 30).is_err());
        assert_eq!(range_label(&[1, 2, 3, 5]), "1-3,5");
    }

    #[test]
    fn a_small_pdf_can_be_read_whole() {
        let prepared = prepare(&test_pdf(4), "doc.pdf", None).unwrap();
        assert_eq!(prepared.attachment.total_pages, 4);
        assert_eq!(prepared.attachment.pages, 4);
        assert!(prepared.attachment.range.is_none());
        assert_eq!(prepared.bytes, test_pdf(4));
    }

    #[test]
    fn a_large_pdf_requires_pages() {
        let error = prepare(&test_pdf(12), "doc.pdf", None).unwrap_err();
        assert!(error.contains("pass pages"), "{error}");
    }

    #[test]
    fn a_page_range_is_sliced_and_parses_back() {
        let prepared = prepare(&test_pdf(12), "doc.pdf", Some("2-4")).unwrap();
        assert_eq!(prepared.attachment.total_pages, 12);
        assert_eq!(prepared.attachment.pages, 3);
        assert_eq!(prepared.attachment.range.as_deref(), Some("2-4"));
        let sliced = lopdf::Document::load_mem(&prepared.bytes).unwrap();
        assert_eq!(sliced.get_pages().len(), 3);
    }

    #[test]
    fn a_non_pdf_is_rejected() {
        assert!(prepare(b"hello", "x.pdf", None).is_err());
    }
}
