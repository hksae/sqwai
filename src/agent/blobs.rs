//! Layer 1 of §2.5: a content-addressed store of file pre-images.
//!
//! The point of this layer is that it has **no dependency on git at all**. For
//! `write | edit | multi_edit | patch` the file about to change is known in
//! advance, so its bytes are kept before the mutation and `/undo` for that
//! file becomes a copy, not a tree operation. That is what lifts the old
//! restriction "undo is unavailable outside a git repository", and it is what
//! makes reverting a single step possible at all — a tree snapshot cannot say
//! which of several files belonged to which step, and the journal can.
//!
//! Blobs are named by their blake3 hash (§5.10 assigns blake3 to this layer
//! and keeps sha2 for the hashes the journal already records), stored under
//! `.sqwai/checkpoints/blobs/<first two hex>/<hash>` and deduplicated by that
//! name: writing the same content twice costs one existence check.
//!
//! The journal's `file_diff` keeps recording `hash_before` / `hash_after` as
//! sha256 — his scoped-undo matching reads that format — and gains
//! `blob_before` / `blob_after`, which are the blake3 names *in this store*.
//! Two algorithms sound like an accident; it is the alternative that is worse.
//! Renaming the journal's hashes would silently invalidate every record
//! written before the change, and dropping blake3 would contradict §5.10. So
//! the record carries both, and the link is explicit rather than implied.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Content stored above this size is compressed. Below it, zstd's frame header
/// and the CPU cost buy nothing: source files that small are dominated by the
/// filesystem's block size either way.
const COMPRESS_ABOVE_BYTES: usize = 4096;

/// zstd level 3 — its default. Level 19 spends roughly ten times the CPU for a
/// few percent on source text, and this runs in the path of every edit.
const COMPRESS_LEVEL: i32 = 3;

/// Marker of a compressed blob. A plain blob starts with its own bytes, so the
/// store cannot guess; the flag is one byte at the front rather than a second
/// file or a name suffix.
const RAW: u8 = b'0';
const ZSTD: u8 = b'1';

pub fn dir(root: &Path) -> PathBuf {
    root.join(".sqwai").join("checkpoints").join("blobs")
}

/// blake3 of `content`, as the store names it.
pub fn id(content: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(content).to_hex())
}

fn path_for(root: &Path, id: &str) -> PathBuf {
    let hex = id.strip_prefix("blake3:").unwrap_or(id);
    // one level of fan-out: a long session can write thousands of blobs, and
    // some filesystems slow down badly on a single huge directory
    let (shard, _) = hex.split_at(2.min(hex.len()));
    dir(root).join(shard).join(hex)
}

/// Store `content` and return its id. Storing the same content again is a
/// no-op — the id is the name, so identical bytes are one file.
pub fn put(root: &Path, content: &[u8]) -> Result<String> {
    let id = id(content);
    let path = path_for(root, &id);
    if path.exists() {
        return Ok(id);
    }
    let parent = path
        .parent()
        .context("blob path has no parent directory")?
        .to_path_buf();
    std::fs::create_dir_all(&parent).context("creating the blob directory")?;

    let mut body = Vec::with_capacity(content.len() + 1);
    if content.len() > COMPRESS_ABOVE_BYTES {
        body.push(ZSTD);
        body.extend(zstd::encode_all(content, COMPRESS_LEVEL).context("compressing a blob")?);
    } else {
        body.push(RAW);
        body.extend_from_slice(content);
    }

    // Write to a temporary name in the same directory and rename: a blob is
    // named by its own hash, so a half-written file under the final name would
    // be a lie that survives restarts.
    let hex = id.strip_prefix("blake3:").unwrap_or(&id);
    let tmp = parent.join(format!(".{}.{}.tmp", std::process::id(), hex));
    std::fs::write(&tmp, &body).context("writing a blob")?;
    std::fs::rename(&tmp, &path).context("publishing a blob")?;
    Ok(id)
}

/// Read a blob back. Verifies the content against the id it was asked for:
/// this is the data `/undo` writes over a user's file, so a silent mismatch
/// would be the worst possible failure.
pub fn get(root: &Path, id: &str) -> Result<Vec<u8>> {
    let path = path_for(root, id);
    let stored = std::fs::read(&path)
        .with_context(|| format!("blob {id} is not in the store ({})", path.display()))?;
    let (flag, body) = stored.split_first().context("blob is empty")?;
    let content = match *flag {
        ZSTD => zstd::decode_all(body).context("decompressing a blob")?,
        RAW => body.to_vec(),
        other => anyhow::bail!("blob {id} has an unknown storage flag {other:#x}"),
    };
    let actual = self::id(&content);
    let expected = if id.starts_with("blake3:") {
        id.to_string()
    } else {
        format!("blake3:{id}")
    };
    if actual != expected {
        anyhow::bail!("blob {id} does not hash to its name (got {actual})");
    }
    Ok(content)
}

pub fn has(root: &Path, id: &str) -> bool {
    path_for(root, id).exists()
}

/// Remove blobs nothing references any more (§2.5 retention).
///
/// A blob is kept when a journal still names it, or when it is younger than
/// `grace` — the second condition is what stops a race with a write that has
/// stored its pre-image but not yet appended the `file_diff` record.
///
/// Returns how many blobs were removed and how many bytes that freed, because
/// a maintenance routine that reports nothing is a maintenance routine nobody
/// trusts.
pub fn purge(
    root: &Path,
    referenced: &std::collections::HashSet<String>,
    grace: Duration,
) -> Purged {
    let mut purged = Purged::default();
    let Ok(shards) = std::fs::read_dir(dir(root)) else {
        return purged;
    };
    let now = SystemTime::now();
    for shard in shards.flatten() {
        if !shard.path().is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(shard.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // temporary files from an interrupted `put`
            let is_temp = name.starts_with('.');
            let id = format!("blake3:{name}");
            if !is_temp && referenced.contains(&id) {
                purged.kept += 1;
                continue;
            }
            let young = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age < grace);
            if young && !is_temp {
                purged.kept += 1;
                continue;
            }
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            if std::fs::remove_file(&path).is_ok() {
                purged.removed += 1;
                purged.freed_bytes += size;
            }
        }
        // an empty shard directory is noise in `ls`
        let _ = std::fs::remove_dir(shard.path());
    }
    purged
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Purged {
    pub removed: usize,
    pub kept: usize,
    pub freed_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("sqwai-blobs")
            .tempdir()
            .unwrap()
    }

    #[test]
    fn a_blob_round_trips_at_both_sides_of_the_compression_threshold() {
        let dir = root();
        for content in [
            b"fn main() {}".to_vec(),
            // over the threshold, so it takes the compressed path
            "x".repeat(COMPRESS_ABOVE_BYTES + 1).into_bytes(),
            // bytes that are not text at all
            (0u8..=255).cycle().take(9000).collect::<Vec<u8>>(),
            // and the empty file, which has a hash like anything else
            Vec::new(),
        ] {
            let id = put(dir.path(), &content).unwrap();
            assert!(has(dir.path(), &id));
            assert_eq!(get(dir.path(), &id).unwrap(), content, "id {id}");
        }
    }

    /// The id is the name, so the same content cannot occupy two files. This
    /// is what keeps a long session's store proportional to distinct contents
    /// rather than to the number of edits.
    #[test]
    fn identical_content_is_stored_once() {
        let dir = root();
        let a = put(dir.path(), b"same").unwrap();
        let b = put(dir.path(), b"same").unwrap();
        assert_eq!(a, b);
        let files: Vec<_> = walk(&super::dir(dir.path()));
        assert_eq!(files.len(), 1, "{files:?}");

        let c = put(dir.path(), b"different").unwrap();
        assert_ne!(a, c);
        assert_eq!(walk(&super::dir(dir.path())).len(), 2);
    }

    /// `/undo` writes this content over the user's file. Serving something
    /// that does not match the requested id would be worse than failing.
    #[test]
    fn a_corrupted_blob_is_refused_rather_than_served() {
        let dir = root();
        let id = put(dir.path(), b"original").unwrap();
        let path = path_for(dir.path(), &id);
        std::fs::write(&path, [RAW, b'w', b'r', b'o', b'n', b'g']).unwrap();
        let err = get(dir.path(), &id).unwrap_err().to_string();
        assert!(err.contains("does not hash to its name"), "{err}");
    }

    #[test]
    fn a_blob_asked_for_but_never_stored_names_itself_in_the_error() {
        let dir = root();
        let err = get(dir.path(), "blake3:deadbeef").unwrap_err().to_string();
        assert!(err.contains("blake3:deadbeef"), "{err}");
        assert!(err.contains("not in the store"), "{err}");
    }

    /// Retention must not collect a pre-image `/undo` could still need. The
    /// grace window is the second guard: a blob stored microseconds before its
    /// `file_diff` record was appended is not yet referenced by anything.
    #[test]
    fn purge_keeps_what_is_referenced_and_what_is_still_young() {
        use std::collections::HashSet;
        let dir = root();
        let root_path = dir.path();

        let referenced = put(root_path, b"still needed by a journal record").unwrap();
        let orphan = put(root_path, b"nothing points here any more").unwrap();
        let young = put(root_path, b"stored a moment ago").unwrap();

        // age the first two past the window; the third stays young
        for id in [&referenced, &orphan] {
            let path = path_for(root_path, id);
            let old = SystemTime::now() - Duration::from_secs(3600);
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(old).unwrap();
        }

        let keep: HashSet<String> = HashSet::from([referenced.clone()]);
        let report = purge(root_path, &keep, Duration::from_secs(60));

        assert_eq!(report.removed, 1, "only the orphan goes");
        assert!(report.freed_bytes > 0);
        assert!(
            has(root_path, &referenced),
            "a referenced blob was collected"
        );
        assert!(
            has(root_path, &young),
            "a blob younger than the grace window"
        );
        assert!(!has(root_path, &orphan));
    }

    /// A `put` interrupted between writing the temp file and renaming it
    /// leaves a dot-file that nothing will ever reference.
    #[test]
    fn purge_removes_leftover_temporary_files() {
        use std::collections::HashSet;
        let dir = root();
        let root_path = dir.path();
        let id = put(root_path, b"real").unwrap();
        let shard = path_for(root_path, &id).parent().unwrap().to_path_buf();
        let temp = shard.join(".999.tmp");
        std::fs::write(&temp, b"half-written").unwrap();

        let keep = HashSet::from([id.clone()]);
        let report = purge(root_path, &keep, Duration::from_secs(0));
        assert!(!temp.exists(), "the temporary file survived");
        assert!(has(root_path, &id));
        assert_eq!(report.removed, 1);
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
        out
    }
}
