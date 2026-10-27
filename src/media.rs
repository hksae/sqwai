//! Image normalization and the content-addressed media store.
//!
//! `view_image` decodes a local image once, normalizes it to a bounded
//! rendition, and stores the bytes under the app-data directory beside the
//! session transcripts. The transcript keeps only metadata + a hash; provider
//! adapters read the blob back and base64-encode it when a request is built.
//! Keeping pixels out of the session JSON matters: sessions are text files,
//! journal/shadow/compaction walk them, and a base64 blob would ride through
//! every one of those paths.

use crate::providers::{DocumentAttachment, ImageAttachment};
use base64::Engine as _;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, metadata::Orientation};
use sha2::{Digest, Sha256};
use std::io::Cursor;
use std::path::PathBuf;

/// Raw file cap before decoding starts.
pub const MAX_SOURCE_BYTES: u64 = 25 * 1024 * 1024;
/// Decompression-bomb guard, checked from the header before a full decode.
pub const MAX_SOURCE_PIXELS: u64 = 40_000_000;
/// Long edge of the stored rendition. Fits Anthropic's standard tier and
/// OpenAI's high-detail patch budget without server-side downscaling.
pub const MAX_EDGE: u32 = 1568;
/// Floor for a deliberately small rendition: below this a screenshot stops
/// answering either question — layout or text.
pub const MIN_VIEW_EDGE: u32 = 128;
/// Stored-rendition cap; matches the strictest documented provider limit
/// (5 MB base64 on Bedrock/Vertex) with headroom.
pub const MAX_RENDITION_BYTES: usize = 5 * 1024 * 1024;

pub struct Prepared {
    pub attachment: ImageAttachment,
    pub bytes: Vec<u8>,
}

pub fn is_supported_image(bytes: &[u8]) -> bool {
    image::guess_format(bytes)
        .map(is_supported_format)
        .unwrap_or(false)
}

fn is_supported_format(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
    )
}

/// Decode, orient, bound and re-encode one image at the full budget.
pub fn prepare(source: &[u8], name: &str) -> Result<Prepared, String> {
    prepare_at(source, name, MAX_EDGE)
}

/// The same, bounded to a requested long edge. Cost is quadratic in the edge,
/// so a layout check at 392 px is ~16× cheaper than the full rendition; the
/// request can only lower the cap, never raise it above `MAX_EDGE`.
pub fn prepare_at(source: &[u8], name: &str, max_edge: u32) -> Result<Prepared, String> {
    let max_edge = max_edge.clamp(MIN_VIEW_EDGE, MAX_EDGE);
    if source.is_empty() {
        return Err("image file is empty".to_string());
    }
    if source.len() as u64 > MAX_SOURCE_BYTES {
        return Err(format!(
            "image is {:.1} MB — the limit is {} MB",
            source.len() as f64 / (1024.0 * 1024.0),
            MAX_SOURCE_BYTES / (1024 * 1024)
        ));
    }
    let format = image::guess_format(source)
        .map_err(|_| "not a recognized image (png, jpeg, webp, gif)".to_string())?;
    if !is_supported_format(format) {
        return Err("unsupported image format — png, jpeg, webp or gif".to_string());
    }
    let reader = ImageReader::with_format(Cursor::new(source), format);
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| format!("cannot decode image: {e}"))?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_SOURCE_PIXELS {
        return Err(format!(
            "image is {width}×{height} — the limit is 40 megapixels"
        ));
    }
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img =
        DynamicImage::from_decoder(decoder).map_err(|e| format!("cannot decode image: {e}"))?;
    img.apply_orientation(orientation);
    let img = bound_long_edge(img, max_edge);
    let (bytes, media_type) = encode(img, format)?;
    let attachment = ImageAttachment {
        id: hex_digest(&bytes),
        name: clean_name(name),
        media_type: media_type.to_string(),
        width: 0,
        height: 0,
        bytes: bytes.len() as u64,
        marker: None,
    };
    // Dimensions are read back from the encoded rendition so metadata always
    // describes exactly the bytes the provider will see.
    let (width, height) = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_dimensions()
        .map_err(|e| e.to_string())?;
    Ok(Prepared {
        attachment: ImageAttachment {
            width,
            height,
            ..attachment
        },
        bytes,
    })
}

fn bound_long_edge(img: DynamicImage, max_edge: u32) -> DynamicImage {
    let (width, height) = (img.width(), img.height());
    let long = width.max(height);
    if long <= max_edge {
        return img;
    }
    let scale = f64::from(max_edge) / f64::from(long);
    let width = ((f64::from(width) * scale).round() as u32).max(1);
    let height = ((f64::from(height) * scale).round() as u32).max(1);
    img.resize(width, height, image::imageops::FilterType::Lanczos3)
}

fn shrink(img: DynamicImage) -> DynamicImage {
    let width = ((img.width() as f64 * 0.8).round() as u32).max(1);
    let height = ((img.height() as f64 * 0.8).round() as u32).max(1);
    img.resize(width, height, image::imageops::FilterType::Lanczos3)
}

/// Screenshots stay lossless PNG; photos become JPEG. Alpha can only survive
/// PNG, so images that carry it shrink instead of switching formats.
fn encode(
    img: DynamicImage,
    source_format: ImageFormat,
) -> Result<(Vec<u8>, &'static str), String> {
    let alpha = img.color().has_alpha();
    let mut use_png = alpha || matches!(source_format, ImageFormat::Png | ImageFormat::Gif);
    let mut img = img;
    for _ in 0..4 {
        let (bytes, media_type) = if use_png {
            (encode_png(&img)?, "image/png")
        } else {
            (encode_jpeg(&img, 85)?, "image/jpeg")
        };
        if bytes.len() <= MAX_RENDITION_BYTES {
            return Ok((bytes, media_type));
        }
        if use_png && !alpha {
            // An opaque PNG over the cap is usually a photo; JPEG is the
            // cheaper escape hatch before losing resolution.
            use_png = false;
        } else {
            img = shrink(img);
        }
    }
    Err("image cannot be encoded under 5 MB".to_string())
}

fn encode_png(img: &DynamicImage) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .map_err(|e| format!("cannot encode png: {e}"))?;
    Ok(out)
}

fn encode_jpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let rgb = img.to_rgb8();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    encoder
        .encode_image(&rgb)
        .map_err(|e| format!("cannot encode jpeg: {e}"))?;
    Ok(out)
}

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn clean_name(name: &str) -> String {
    let raw = std::path::Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return "image".to_string();
    }
    if cleaned.chars().count() > 80 {
        let mut out: String = cleaned.chars().take(79).collect();
        out.push('…');
        out
    } else {
        cleaned.to_string()
    }
}

/// Content-addressed write. Returns `Ok(())` when the blob is already there:
/// the same image viewed twice produces the same hash and one file.
pub fn store(prepared: &Prepared) -> Result<(), String> {
    store_bytes(
        &prepared.attachment.id,
        &prepared.attachment.media_type,
        &prepared.bytes,
    )
}

pub fn store_document(attachment: &DocumentAttachment, bytes: &[u8]) -> Result<(), String> {
    store_bytes(&attachment.id, &attachment.media_type, bytes)
}

fn store_bytes(id: &str, media_type: &str, bytes: &[u8]) -> Result<(), String> {
    let dir = store_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create media store: {e}"))?;
    let path = dir.join(format!("{id}.{}", ext_for(media_type)?));
    if path.exists() {
        return Ok(());
    }
    let tmp = dir.join(format!(".{id}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| format!("cannot write media blob: {e}"))?;
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        if !path.exists() {
            return Err("cannot move media blob into the store".to_string());
        }
    }
    Ok(())
}

fn blob_path(id: &str, media_type: &str) -> Result<PathBuf, String> {
    if !valid_id(id) {
        return Err("invalid media id".to_string());
    }
    Ok(store_dir()?.join(format!("{id}.{}", ext_for(media_type)?)))
}

pub fn load(attachment: &ImageAttachment) -> Result<Vec<u8>, String> {
    let path = blob_path(&attachment.id, &attachment.media_type)
        .map_err(|_| format!("invalid media id for {}", attachment.name))?;
    std::fs::read(&path).map_err(|e| {
        format!(
            "image '{}' is missing from the media store ({e}); view it again",
            attachment.name
        )
    })
}

/// Open a stored rendition in the system image viewer. Used by transcript
/// clicks; the blob is host-owned, so no user path is launched here.
pub fn open_blob(id: &str, media_type: &str, name: &str) -> Result<(), String> {
    let path = blob_path(id, media_type)
        .map_err(|_| format!("invalid media id for {name}"))?;
    if !path.exists() {
        return Err(format!("image '{name}' is missing from the media store"));
    }
    open_path(&path)
}

#[cfg(windows)]
fn open_path(path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let file: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let verb: Vec<u16> = "open"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if (result as usize) <= 32 {
        return Err(format!(
            "cannot open the image viewer for {}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_path(path: &std::path::Path) -> Result<(), String> {
    open_with("open", path)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_path(path: &std::path::Path) -> Result<(), String> {
    open_with("xdg-open", path)
}

#[cfg(not(windows))]
fn open_with(command: &str, path: &std::path::Path) -> Result<(), String> {
    std::process::Command::new(command)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot open the image viewer: {e}"))
}

pub fn load_document(document: &DocumentAttachment) -> Result<Vec<u8>, String> {
    let path = blob_path(&document.id, &document.media_type)
        .map_err(|_| format!("invalid media id for {}", document.name))?;
    std::fs::read(&path).map_err(|e| {
        format!(
            "document '{}' is missing from the media store ({e}); read it again",
            document.name
        )
    })
}

pub fn base64_document(document: &DocumentAttachment) -> Result<String, String> {
    let bytes = load_document(document)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

pub fn data_uri_document(document: &DocumentAttachment) -> Result<String, String> {
    Ok(format!(
        "data:{};base64,{}",
        document.media_type,
        base64_document(document)?
    ))
}

pub fn base64_data(attachment: &ImageAttachment) -> Result<String, String> {
    let bytes = load(attachment)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

pub fn data_uri(attachment: &ImageAttachment) -> Result<String, String> {
    Ok(format!(
        "data:{};base64,{}",
        attachment.media_type,
        base64_data(attachment)?
    ))
}

fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Delete blobs no longer named by any session transcript. Called when a
/// session is deleted; the age guard keeps a blob written moments ago safe
/// even if the session that references it is still being saved.
#[cfg(not(test))]
pub fn sweep_orphans(sessions_dir: &std::path::Path) -> usize {
    let Ok(dir) = store_dir() else {
        return 0;
    };
    sweep_unreferenced(
        &dir,
        sessions_dir,
        std::time::Duration::from_secs(7 * 24 * 3600),
    )
}

fn sweep_unreferenced(
    media_dir: &std::path::Path,
    sessions_dir: &std::path::Path,
    older_than: std::time::Duration,
) -> usize {
    let Ok(entries) = std::fs::read_dir(media_dir) else {
        return 0;
    };
    let referenced = referenced_ids(sessions_dir);
    let now = std::time::SystemTime::now();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        if !matches!(ext, Some("png" | "jpg" | "webp" | "pdf")) {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !valid_id(id) || referenced.contains(id) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() < older_than {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn referenced_ids(sessions_dir: &std::path::Path) -> std::collections::HashSet<String> {
    let mut ids = std::collections::HashSet::new();
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return ids;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        collect_image_ids(&value, &mut ids);
    }
    ids
}

fn collect_image_ids(value: &serde_json::Value, ids: &mut std::collections::HashSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if map.contains_key("media_type")
                && let Some(id) = map.get("id").and_then(|v| v.as_str())
            {
                ids.insert(id.to_string());
            }
            for child in map.values() {
                collect_image_ids(child, ids);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                collect_image_ids(child, ids);
            }
        }
        _ => {}
    }
}

fn ext_for(media_type: &str) -> Result<&'static str, String> {
    match media_type {
        "image/png" => Ok("png"),
        "image/jpeg" => Ok("jpg"),
        "image/webp" => Ok("webp"),
        "application/pdf" => Ok("pdf"),
        other => Err(format!("unsupported media type '{other}'")),
    }
}

pub fn store_dir() -> Result<PathBuf, String> {
    #[cfg(test)]
    {
        Ok(test_dir().to_path_buf())
    }
    #[cfg(not(test))]
    {
        Ok(crate::config::data_dir()
            .map_err(|e| e.to_string())?
            .join("media"))
    }
}

/// A small deterministic PNG for tests, not stored anywhere.
#[cfg(test)]
pub fn test_png(width: u32, height: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(width, height, |x, y| {
        image::Rgba([(x % 255) as u8, (y % 255) as u8, 80, 255])
    });
    let mut out = Vec::new();
    DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .expect("encode test png");
    out
}

/// A stored attachment for wire/counter tests.
#[cfg(test)]
pub fn test_image(width: u32, height: u32, name: &str) -> ImageAttachment {
    let prepared = prepare(&test_png(width, height), name).expect("prepare test image");
    store(&prepared).expect("store test image");
    prepared.attachment
}

/// A stored PDF attachment for wire/counter tests.
#[cfg(test)]
pub fn test_document(pages: u32, name: &str) -> DocumentAttachment {
    let source = crate::pdf::test_pdf(pages);
    let prepared = crate::pdf::prepare(&source, name, None).expect("prepare test document");
    store_document(&prepared.attachment, &prepared.bytes).expect("store test document");
    prepared.attachment
}

#[cfg(test)]
fn test_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().expect("temp media dir"))
        .path()
}

/// Anthropic visual tokens: 28×28 patches, bounded by the standard tier
/// (1568 px long edge, 1568 visual tokens). The server applies the same cap;
/// estimating it lets `/context` tell the truth.
pub fn anthropic_tokens(width: u32, height: u32) -> u64 {
    if width == 0 || height == 0 {
        return 0;
    }
    let (mut w, mut h) = (f64::from(width), f64::from(height));
    let long = w.max(h);
    if long > f64::from(MAX_EDGE) {
        let scale = f64::from(MAX_EDGE) / long;
        w *= scale;
        h *= scale;
    }
    let tokens = (w / 28.0).ceil() * (h / 28.0).ceil();
    // The standard tier caps visual tokens at 1568; the server downsizes to
    // fit. Ceil-rounding means a naive rescale lands a few tokens above the
    // cap, so clamp to the documented ceiling instead.
    const MAX_VISUAL_TOKENS: f64 = 1568.0;
    tokens.min(MAX_VISUAL_TOKENS) as u64
}

/// OpenAI patch tokens: 32×32 patches. Our 1568 px rendition is below the
/// high-detail patch budget (2500), so the extra resizing step never fires.
pub fn openai_tokens(width: u32, height: u32) -> u64 {
    u64::from(width).div_ceil(32) * u64::from(height).div_ceil(32)
}

/// One number for compaction pressure: never undercount an image.
pub fn pressure_tokens(width: u32, height: u32) -> u64 {
    anthropic_tokens(width, height).max(openai_tokens(width, height))
}

/// A compact human-facing cost, e.g. `~1.2k` or `~900`.
pub fn tokens_label(width: u32, height: u32) -> String {
    let tokens = anthropic_tokens(width, height);
    if tokens >= 1000 {
        format!("~{:.1}k", tokens as f64 / 1000.0)
    } else {
        format!("~{tokens}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn png(width: u32, height: u32) -> Vec<u8> {
        let img = RgbaImage::from_fn(width, height, |x, y| {
            Rgba([(x % 255) as u8, (y % 255) as u8, 80, 255])
        });
        let mut out = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    /// The knob the model needs to spend less: the cap lowers, and a request
    /// above the budget still lands on the budget rather than upscaling.
    #[test]
    fn prepare_at_lowers_the_cap_and_never_raises_it() {
        let bytes = png(2000, 1000);
        let small = prepare_at(&bytes, "wide.png", 400).unwrap();
        assert_eq!((small.attachment.width, small.attachment.height), (400, 200));
        assert_ne!(small.attachment.id, prepare(&bytes, "wide.png").unwrap().attachment.id);
        assert!(small.bytes.len() < prepare(&bytes, "wide.png").unwrap().bytes.len());

        let over = prepare_at(&bytes, "wide.png", 9000).unwrap();
        assert_eq!(over.attachment.width, 1568);
        let under = prepare_at(&bytes, "wide.png", 8).unwrap();
        assert_eq!(under.attachment.width, 128);
    }

    #[test]
    fn test_png_helper_is_decodable() {
        let prepared = prepare(&test_png(20, 10), "fixture.png").unwrap();
        assert_eq!(
            (prepared.attachment.width, prepared.attachment.height),
            (20, 10)
        );
    }

    #[test]
    fn bounds_long_edge_and_reports_final_dimensions() {
        let bytes = png(2000, 1000);
        let prepared = prepare(&bytes, "wide.png").unwrap();
        assert_eq!(prepared.attachment.width, 1568);
        assert_eq!(prepared.attachment.height, 784);
        assert_eq!(prepared.attachment.media_type, "image/png");
        assert_eq!(prepared.attachment.name, "wide.png");
        assert_eq!(prepared.attachment.id.len(), 64);
        assert_eq!(prepared.attachment.bytes, prepared.bytes.len() as u64);
    }

    #[test]
    fn small_images_are_not_enlarged() {
        let prepared = prepare(&png(64, 32), "small.png").unwrap();
        assert_eq!(
            (prepared.attachment.width, prepared.attachment.height),
            (64, 32)
        );
    }

    #[test]
    fn store_is_content_addressed_and_loadable() {
        let prepared = prepare(&png(120, 80), "shot.png").unwrap();
        store(&prepared).unwrap();
        store(&prepared).unwrap();
        let loaded = load(&prepared.attachment).unwrap();
        assert_eq!(loaded, prepared.bytes);
        let uri = data_uri(&prepared.attachment).unwrap();
        assert!(uri.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn rejects_non_images() {
        assert!(prepare(b"hello", "notes.txt").is_err());
        assert!(!is_supported_image(b"hello"));
    }

    #[test]
    fn token_estimates_match_the_documented_formulas() {
        assert_eq!(anthropic_tokens(1280, 720), 46 * 26);
        assert_eq!(openai_tokens(1280, 720), 40 * 23);
        assert_eq!(pressure_tokens(1280, 720), 46 * 26);
        // A square at the edge cap is downscaled by Anthropic's token budget.
        assert_eq!(anthropic_tokens(1568, 1568), 1568);
    }

    #[test]
    fn sweep_removes_only_old_unreferenced_blobs() {
        let media = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let referenced = prepare(&test_png(8, 8), "a.png").unwrap();
        let orphan = prepare(&test_png(9, 9), "b.png").unwrap();
        let kept_document = crate::pdf::prepare(&crate::pdf::test_pdf(1), "k.pdf", None).unwrap();
        let orphan_document = crate::pdf::prepare(&crate::pdf::test_pdf(2), "o.pdf", None).unwrap();
        for prepared in [&referenced, &orphan] {
            std::fs::write(
                media.path().join(format!("{}.png", prepared.attachment.id)),
                &prepared.bytes,
            )
            .unwrap();
        }
        for prepared in [&kept_document, &orphan_document] {
            std::fs::write(
                media.path().join(format!("{}.pdf", prepared.attachment.id)),
                &prepared.bytes,
            )
            .unwrap();
        }
        std::fs::write(
            sessions.path().join("s.json"),
            serde_json::json!({
                "messages": [{"images": [{
                    "id": referenced.attachment.id,
                    "media_type": "image/png",
                }], "documents": [{
                    "id": kept_document.attachment.id,
                    "media_type": "application/pdf",
                }]}],
            })
            .to_string(),
        )
        .unwrap();
        let removed = sweep_unreferenced(media.path(), sessions.path(), std::time::Duration::ZERO);
        assert_eq!(removed, 2);
        assert!(
            media
                .path()
                .join(format!("{}.png", referenced.attachment.id))
                .exists()
        );
        assert!(
            !media
                .path()
                .join(format!("{}.png", orphan.attachment.id))
                .exists()
        );
        assert!(
            media
                .path()
                .join(format!("{}.pdf", kept_document.attachment.id))
                .exists()
        );
        assert!(
            !media
                .path()
                .join(format!("{}.pdf", orphan_document.attachment.id))
                .exists()
        );
    }

    #[test]
    fn rejects_path_like_ids() {
        let mut prepared = prepare(&png(8, 8), "x.png").unwrap();
        prepared.attachment.id = "../../escape".to_string();
        assert!(load(&prepared.attachment).is_err());
    }
}
