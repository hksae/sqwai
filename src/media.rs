//! Image normalization and the content-addressed media store.
//!
//! `view_image` decodes a local image once, normalizes it to a bounded
//! rendition, and stores the bytes under the app-data directory beside the
//! session transcripts. The transcript keeps only metadata + a hash; provider
//! adapters read the blob back and base64-encode it when a request is built.
//! Keeping pixels out of the session JSON matters: sessions are text files,
//! journal/shadow/compaction walk them, and a base64 blob would ride through
//! every one of those paths.

use crate::providers::ImageAttachment;
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

/// Decode, orient, bound and re-encode one image. Re-encoding is also the
/// metadata scrub: EXIF (including GPS) never survives into the rendition.
pub fn prepare(source: &[u8], name: &str) -> Result<Prepared, String> {
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
    let img = bound_long_edge(img);
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

fn bound_long_edge(img: DynamicImage) -> DynamicImage {
    let (width, height) = (img.width(), img.height());
    let long = width.max(height);
    if long <= MAX_EDGE {
        return img;
    }
    let scale = f64::from(MAX_EDGE) / f64::from(long);
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

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn clean_name(name: &str) -> String {
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
    let dir = store_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create media store: {e}"))?;
    let path = dir.join(format!(
        "{}.{}",
        prepared.attachment.id,
        ext_for(&prepared.attachment.media_type)?
    ));
    if path.exists() {
        return Ok(());
    }
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        prepared.attachment.id,
        std::process::id()
    ));
    std::fs::write(&tmp, &prepared.bytes).map_err(|e| format!("cannot write media blob: {e}"))?;
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        if !path.exists() {
            return Err("cannot move media blob into the store".to_string());
        }
    }
    Ok(())
}

pub fn load(attachment: &ImageAttachment) -> Result<Vec<u8>, String> {
    if !valid_id(&attachment.id) {
        return Err(format!("invalid media id for {}", attachment.name));
    }
    let path = store_dir()?.join(format!(
        "{}.{}",
        attachment.id,
        ext_for(&attachment.media_type)?
    ));
    std::fs::read(&path).map_err(|e| {
        format!(
            "image '{}' is missing from the media store ({e}); view it again",
            attachment.name
        )
    })
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
        if !matches!(ext, Some("png" | "jpg" | "webp")) {
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
        for prepared in [&referenced, &orphan] {
            std::fs::write(
                media.path().join(format!("{}.png", prepared.attachment.id)),
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
                }]}],
            })
            .to_string(),
        )
        .unwrap();
        let removed = sweep_unreferenced(media.path(), sessions.path(), std::time::Duration::ZERO);
        assert_eq!(removed, 1);
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
    }

    #[test]
    fn rejects_path_like_ids() {
        let mut prepared = prepare(&png(8, 8), "x.png").unwrap();
        prepared.attachment.id = "../../escape".to_string();
        assert!(load(&prepared.attachment).is_err());
    }
}
