//! File tool handlers. Every path passes through `ToolCtx::resolve`
//! (project-jail), mutations snapshot first, edits require a prior read.

use super::{FileDiff, Kind, Outcome, ToolCtx};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

fn err<T>(msg: impl Into<String>) -> Result<T, String> {
    Err(msg.into())
}

/// resolve + require existence for read-like ops
fn existing(ctx: &ToolCtx, p: &str) -> Result<PathBuf, String> {
    let p = ctx.resolve(p)?;
    if !p.exists() {
        return err(format!("file not found: {}", p.display()));
    }
    Ok(p)
}

/// Same, for ops that are about to change the file: the path jail lets
/// `.sqwai/spill/` be opened but not written, so a result the model plants in
/// the spill dir cannot come back later as its own past evidence.
fn existing_writable(ctx: &ToolCtx, p: &str) -> Result<PathBuf, String> {
    let p = ctx.resolve_writable(p)?;
    if !p.exists() {
        return err(format!("file not found: {}", p.display()));
    }
    Ok(p)
}

/// Spilled tool output: oversized results land in `.sqwai/spill/` instead of
/// dying in a truncation marker. The model gets the path plus a standing
/// instruction to grep it or page it — never to re-read it whole. It used to go
/// to `sqwai-spill/` at the project root, where it turned up in `ls` among the
/// source directories and needed its own ignore rule; inside the host state it
/// is one disposable place, and the path jail lets it be read but not written.
pub(super) fn spill_output(ctx: &ToolCtx, tool: &str, text: &str) -> Option<String> {
    const RETENTION_SECS: u64 = 7 * 24 * 3600;
    static SPILL_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = ctx.spill_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let id = SPILL_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // best-effort retention: drop week-old spills once on first spill (and
    // periodically every 32 spills) instead of scanning `read_dir` every call
    if id.is_multiple_of(32)
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        let now = std::time::SystemTime::now();
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|mtime| now.duration_since(mtime).ok())
                .is_some_and(|age| age.as_secs() > RETENTION_SECS);
            if old {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let name = format!("{tool}-{}-{id}.txt", std::process::id());
    let path = dir.join(&name);
    if std::fs::write(&path, text).is_err() {
        return None;
    }
    Some(format!(".sqwai/spill/{name}"))
}

/// guard shared by destructive writes: overwriting an existing file and
/// replace_all edits proceed only on a fresh read — blind mass changes
/// are the one failure exact matching cannot catch.
fn require_read(ctx: &ToolCtx, p: &Path) -> Result<(), String> {
    match ctx.read_state(p) {
        super::ReadState::Current => Ok(()),
        super::ReadState::Unread => err(format!(
            "edit denied: {} was not read in this turn — call read first",
            p.display()
        )),
        super::ReadState::Stale => err(format!(
            "edit denied: {} changed since you read it — read it again before editing, \
             or the edit will be based on content that is gone",
            p.display()
        )),
    }
}

/// freshness-only gate for self-validating writes: single exact edits and
/// edits match-or-error before touching disk, so they need no
/// prior read — but working from a stale read is still refused, stale
/// knowledge pointing at moved code.
fn require_fresh(ctx: &ToolCtx, p: &Path) -> Result<(), String> {
    match ctx.read_state(p) {
        super::ReadState::Stale => err(format!(
            "edit denied: {} changed since you read it — read it again before editing, \
             or the edit will be based on content that is gone",
            p.display()
        )),
        super::ReadState::Current | super::ReadState::Unread => Ok(()),
    }
}

fn is_binary(buf: &[u8]) -> bool {
    buf.iter().take(8000).any(|&b| b == 0)
}

/// unified diff of two file contents
pub(crate) fn make_diff(old: &str, new: &str) -> String {
    let d = similar::TextDiff::from_lines(old, new);
    let out = d
        .unified_diff()
        .context_radius(2)
        .header("before", "after")
        .to_string();
    const MAX_DIFF_LINES: usize = 400;
    let lines: Vec<&str> = out.lines().collect();
    if lines.len() > MAX_DIFF_LINES {
        let head = lines[..MAX_DIFF_LINES].join("\n");
        return format!("{head}\n… diff truncated ({} lines)", lines.len());
    }
    out
}

/// (+added/-removed) line counts from a unified diff body
pub(crate) fn content_hash(content: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(content))
}

pub(crate) fn file_diff(
    path: &Path,
    root: &Path,
    before: Option<&[u8]>,
    after: &[u8],
    mode: &str,
    checkpoint: Option<String>,
    diff: &str,
) -> FileDiff {
    let (added, removed) = diff_counts(diff);
    // Layer 1: keep the bytes this edit is about to replace, so a
    // single step can be reverted without a tree snapshot and without git.
    // A store that cannot be written must not block the edit — the same
    // decision the git snapshot already makes a few lines up.
    let blobs = crate::agent::blobs::dir(root);
    let blob_before = before.and_then(|bytes| crate::agent::blobs::put(root, bytes).ok());
    let blob_after = crate::agent::blobs::put(root, after).ok();
    if blob_before.is_none() && before.is_some() {
        crate::providers::log_http(&format!(
            "blob store unavailable at {}: this edit is not revertible on its own",
            blobs.display()
        ));
    }
    FileDiff {
        path: rel_label(root, path),
        added,
        removed,
        hash_before: before.map(content_hash),
        hash_after: content_hash(after),
        mode: mode.to_string(),
        checkpoint,
        blob_before,
        blob_after,
    }
}

pub(crate) fn diff_counts(diff: &str) -> (usize, usize) {
    let mut add = 0usize;
    let mut rem = 0usize;
    for l in diff.lines().skip(2) {
        // skip the ---/+++ headers
        if let Some(rest) = l.strip_prefix('+') {
            if !rest.starts_with("++") {
                add += 1;
            }
        } else if let Some(rest) = l.strip_prefix('-')
            && !rest.starts_with("--")
        {
            rem += 1;
        }
    }
    (add, rem)
}

pub(super) fn read(ctx: &mut ToolCtx, raw: &str, args: &serde_json::Value) -> Outcome {
    // Fused multi-file read (`paths: ["src/a.rs", "src/b.rs", ...]`).
    if let Some(arr) = args.get("paths").and_then(|v| v.as_array()) {
        let mut path_list: Vec<String> = Vec::new();
        if !raw.trim().is_empty() {
            path_list.push(raw.trim().to_string());
        }
        for v in arr.iter().filter_map(|x| x.as_str()) {
            let trimmed = v.trim();
            if !trimmed.is_empty() && !path_list.iter().any(|p| p == trimmed) {
                path_list.push(trimmed.to_string());
            }
        }
        if path_list.len() > 16 {
            return Outcome::err("read accepts at most 16 paths in one call");
        }
        if path_list.len() == 1 {
            return read_single(ctx, &path_list[0], args);
        }
        if !path_list.is_empty() {
            let mut combined = Vec::with_capacity(path_list.len());
            let mut any_ok = false;
            for p_str in &path_list {
                let res = read_single(ctx, p_str, args);
                if res.ok {
                    any_ok = true;
                    combined.push(format!("=== {p_str} ===\n{}", res.output.trim_end()));
                } else {
                    combined.push(format!(
                        "=== {p_str} [ERROR] ===\n{}",
                        res.output.trim_end()
                    ));
                }
            }
            let body = combined.join("\n\n");
            return if any_ok {
                Outcome::ok(body)
            } else {
                Outcome::err(body)
            };
        }
    }

    read_single(ctx, raw, args)
}

fn extract_requested_symbols(args: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(s) = args.get("symbol").and_then(|v| v.as_str()) {
        let t = s.trim();
        if !t.is_empty() {
            out.push(t.to_string());
        }
    }
    if let Some(arr) = args.get("symbols").and_then(|v| v.as_array()) {
        for v in arr.iter().filter_map(|x| x.as_str()) {
            let t = v.trim();
            if !t.is_empty() && !out.iter().any(|x| x == t) {
                out.push(t.to_string());
            }
        }
    } else if let Some(s) = args.get("symbols").and_then(|v| v.as_str()) {
        for part in s.split(',') {
            let t = part.trim();
            if !t.is_empty() && !out.iter().any(|x| x == t) {
                out.push(t.to_string());
            }
        }
    }
    out
}

fn read_single(ctx: &mut ToolCtx, raw: &str, args: &serde_json::Value) -> Outcome {
    let raw_trimmed = raw
        .trim()
        .trim_start_matches("./")
        .trim_start_matches(".\\");
    let p = match existing(ctx, raw) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    let bytes = match fs::metadata(&p) {
        Ok(meta)
            if ctx.tools.read.max_file_bytes > 0 && meta.len() > ctx.tools.read.max_file_bytes =>
        {
            return Outcome::err(format!(
                "file is {} bytes — [tools.read].max_file_bytes is {}",
                meta.len(),
                ctx.tools.read.max_file_bytes
            ));
        }
        _ => match fs::read(&p) {
            Ok(b) => b,
            Err(e) => return Outcome::err(format!("read failed: {e}")),
        },
    };
    if crate::media::is_supported_image(&bytes) {
        return Outcome::err(format!(
            "image file — use view_image(path=\"{raw_trimmed}\") to see it"
        ));
    }
    if crate::pdf::is_pdf(&bytes) {
        return Outcome::err(format!(
            "PDF document — use view_document(path=\"{raw_trimmed}\") to read its pages"
        ));
    }
    if is_binary(&bytes) {
        return Outcome::err("binary file — cannot display");
    }
    // The BOM is an encoding marker, not content: what the model is shown here
    // is what it will anchor `edit` on (#214 L26). The freshness hash below
    // stays over the raw bytes, so the guard still sees the file as it is.
    let decoded = String::from_utf8_lossy(&bytes);
    let (text, _had_bom) = split_bom(&decoded);
    let window_hash = content_hash(&bytes);
    let total_lines = text.lines().count();
    let window = ctx.tools.read.window.clamp(1, 10_000);

    // Symbol-targeted read (`symbol` / `symbols: ["fn_a", "StructB"]`) via tree-sitter AST:
    // extracts the exact line spans of matching functions/types/methods in one step
    // without requiring a prior `outline` round-trip.
    let symbols = extract_requested_symbols(args);
    if !symbols.is_empty() {
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        let ranges = super::outline::find_symbol_ranges(text, ext, &symbols);
        if !ranges.is_empty() {
            let lines_vec: Vec<&str> = text.lines().collect();
            let mut out = String::new();
            for (sig, start, end) in &ranges {
                let capped_end = (*end).min(start + window - 1).min(lines_vec.len());
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!(
                    "// Symbol: {sig} (lines {start}–{capped_end} of {total_lines})\n"
                ));
                for (i, line) in lines_vec
                    .iter()
                    .enumerate()
                    .take(capped_end)
                    .skip(start.saturating_sub(1))
                {
                    out.push_str(&format!("{:>6}\t{line}\n", i + 1));
                }
                ctx.note_read_window(&p, *start, capped_end, window_hash.clone());
            }
            ctx.mark_read(&p);
            return Outcome::ok(out);
        }
        // Fallback if no AST declaration matched: return structure_hint so the model sees valid symbols.
        let hint = structure_hint(ctx, raw).unwrap_or_default();
        return Outcome::err(format!(
            "symbol(s) [{}] not found in {raw}{hint}",
            symbols.join(", ")
        ));
    }

    let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = (args["limit"].as_u64().unwrap_or(window as u64) as usize).min(window);
    // repeat reads of an unchanged window collapse to a stub: re-reading
    // the same files in circles was the top context burner, and the
    // freshness guard (require_read) already forces a fresh read when the
    // file actually moved. Coverage counts, not equality — a live session
    // asked for 846+80, then 846+15, then 855+60 of the same file, and an
    // exact-match guard let all three through.
    let wanted_from = offset.min(total_lines.max(1));
    let wanted_to = offset
        .saturating_add(limit)
        .saturating_sub(1)
        .min(total_lines);
    if wanted_to >= wanted_from
        && let Some((a, b)) = ctx.read_range_served(&p, &window_hash, wanted_from, wanted_to)
    {
        return Outcome::ok(format!(
            "…(unchanged since your last read: lines {a}–{b} of {} already cover {wanted_from}–{wanted_to}, see above — no need to re-read)",
            rel_label(&ctx.root, &p),
        ));
    }
    // Delta read: a whole-file re-read whose only changes since the model
    // last saw it whole are its own edits. The base result and every edit
    // diff are still on the wire (checked via `visible_calls`), so the file
    // itself would only repeat bytes. A ranged read below still serves real
    // lines; a bash/external change breaks the ledger's hash chain and falls
    // through to the file.
    // Only a bare whole-file request gets the pointer. An explicit
    // offset/limit is the model asking for exact lines — even when the range
    // happens to cover the file — and is always served from the file.
    let explicit_range = args.get("offset").is_some() || args.get("limit").is_some();
    let mut pointer = None;
    if !explicit_range
        && wanted_from <= 1
        && wanted_to >= total_lines
        && let Some(entry) = ctx
            .read_ledger
            .delta_for(&p, &window_hash, ctx.visible_calls.as_ref())
    {
        let base = entry.base();
        let mut out = format!(
            "…({} — {} lines, {} B. No full re-read: since your {} at {} ",
            rel_label(&ctx.root, &p),
            total_lines,
            bytes.len(),
            base.kind().label(),
            base.at(),
        );
        if entry.edits().is_empty() {
            if base.is_write() {
                out.push_str(
                    "the file is unchanged; the content is exactly what you sent to \
                     `write`. Use offset/limit if you need exact lines.)",
                );
            } else {
                out.push_str(
                    "the file is unchanged; that read result already has the full \
                     content. Use offset/limit if you need exact lines.)",
                );
            }
        } else {
            out.push_str("it changed only by your own edits:\n");
            for edit in entry.edits() {
                out.push_str(&format!(
                    "  - {} {} +{}/-{}\n",
                    edit.at(),
                    edit.tool(),
                    edit.added(),
                    edit.removed(),
                ));
            }
            out.push_str(
                "Those changes are in your own edit calls above (the old/new strings \
                 you sent); each result names where it landed. Use offset/limit if you \
                 need exact current lines.)",
            );
        }
        pointer = Some(out);
    }
    if let Some(out) = pointer {
        // The pointer is a read for the edit guard, but it is not a served
        // line window: a later ranged read must still get real lines.
        ctx.mark_read(&p);
        ctx.read_ledger.note_pointer(&p);
        return Outcome::ok(out);
    }
    let mut out = String::new();
    let first_no = offset;
    let mut last_no = offset.saturating_sub(1);
    let mut truncated = false;
    for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        if out.len() > 300_000 {
            truncated = true;
            break;
        }
        out.push_str(&format!("{:>6}\t{line}\n", i + 1));
        last_no = i + 1;
    }
    if !truncated && last_no < total_lines {
        truncated = true;
    }
    if total_lines == 0 {
        out.push_str("(empty file)\n");
    } else if truncated {
        match spill_output(ctx, "read", text) {
            Some(rel) => out.push_str(&format!(
                "\n…(showing lines {first_no}–{last_no} of {total_lines} total — full output saved to {rel}: use grep to search it or read with offset/limit for specific sections, do not re-read the whole file)"
            )),
            None => out.push_str("\n…(output truncated)"),
        }
    } else {
        out.push_str(&format!(
            "\n(showing lines {first_no}–{last_no} of {total_lines} total)"
        ));
    }
    // A big file gets its map with the first window. The read description has
    // always said "outline for structure before targeted reading" and was
    // ignored — 23 windows of one 5300-line file, scattered and overlapping,
    // because the model had no idea where anything lived. Top-level outline,
    // clipped, once per file: a few KB against twenty round trips.
    let served_before = ctx.read_window_count(&p, &window_hash);
    if served_before == 0 && total_lines >= STRUCTURE_HINT_MIN {
        if let Some(hint) = structure_hint(ctx, raw) {
            out.push_str(&hint);
        }
    } else if served_before >= SLICING_NUDGE_AFTER {
        out.push_str(&format!(
            "\n[host: this is window {} of this file — `outline` gives its map and `grep` with context finds a symbol in one call]",
            served_before + 1
        ));
    }
    ctx.mark_read(&p);
    ctx.note_read_window(&p, first_no, last_no, window_hash.clone());
    // A real full read re-establishes the model's complete picture: that
    // result becomes the base for the next delta. Partial windows keep the
    // old base — they do not replace it.
    if !truncated && wanted_from <= 1 && wanted_to >= total_lines {
        ctx.read_ledger
            .note_full_read(&p, &window_hash, ctx.current_call_id.clone());
    }
    Outcome::ok(out)
}

/// Files at or above this line count get their outline attached to the first
/// read of the session.
const STRUCTURE_HINT_MIN: usize = 400;
/// After this many windows of the same file, point at the cheaper tools.
const SLICING_NUDGE_AFTER: usize = 3;
/// The attached map is a locator, not a substitute for `outline`.
const STRUCTURE_HINT_MAX_BYTES: usize = 3_000;

fn structure_hint(ctx: &mut ToolCtx, raw: &str) -> Option<String> {
    let listed = super::outline::outline(
        ctx,
        &serde_json::json!({"path": raw, "max_depth": 1, "skip_deps": true}),
    );
    if !listed.ok {
        return None;
    }
    let body = listed.output.trim_end();
    if body.is_empty() || body.ends_with("(0 items)") {
        return None;
    }
    let clipped = if body.len() > STRUCTURE_HINT_MAX_BYTES {
        let cut = body
            .char_indices()
            .take_while(|(i, _)| *i < STRUCTURE_HINT_MAX_BYTES)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(STRUCTURE_HINT_MAX_BYTES);
        format!("{}\n…(truncated)", &body[..cut])
    } else {
        body.to_string()
    };
    Some(format!(
        "\n\nstructure of this file, so you can target reads instead of scanning it:\n{clipped}"
    ))
}

/// A UTF-8 BOM is an encoding marker, not content. Read with it, line 1 shows
/// the model a stray U+FEFF and an `edit` anchored at the start of the file
/// cannot match what the host just displayed (#214 L26). File text therefore
/// crosses the model boundary without the marker, and the marker is written
/// back when the file had one — a read-edit-write cycle leaves the file
/// byte-for-byte what the encoding asked for.
const BOM: &str = "\u{feff}";

/// Split a leading BOM off text that came off disk.
fn split_bom(text: &str) -> (&str, bool) {
    match text.strip_prefix(BOM) {
        Some(rest) => (rest, true),
        None => (text, false),
    }
}

/// The bytes to write: `edited` as the model saw it, plus the marker it never
/// saw when the file carried one.
fn restore_bom(edited: String, had_bom: bool) -> String {
    if had_bom {
        format!("{BOM}{edited}")
    } else {
        edited
    }
}

pub(super) fn write_file(ctx: &mut ToolCtx, raw: &str, content: &str) -> Outcome {
    let exists = {
        // probe without failing when missing
        ctx.resolve_writable(raw)
            .map(|p| p.exists())
            .unwrap_or(false)
    };
    // previous contents, kept for the post-facto diff
    let mut prev: Option<String> = None;
    // the encoding marker the file carried: a whole-file rewrite must not
    // silently drop it, because the model never sees the BOM and so can have no
    // opinion about it (#214 L26)
    let mut had_bom = false;
    if exists {
        let p = match existing_writable(ctx, raw) {
            Ok(p) => p,
            Err(e) => return Outcome::err(e),
        };
        if let Err(e) = require_read(ctx, &p) {
            return Outcome::err(e);
        }
        prev = fs::read_to_string(&p).ok();
        if let Some(text) = prev.as_deref() {
            had_bom = split_bom(text).1;
        }
    }
    let p = match ctx.resolve_writable(raw) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    if let Some(parent) = p.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        return Outcome::err(format!("mkdir failed: {e}"));
    }
    let body = restore_bom(content.to_string(), had_bom && !content.starts_with(BOM));
    // checkpoint before mutation
    let checkpoint = Some(checkpoint(ctx, &p, "write"));
    if let Err(e) = fs::write(&p, &body) {
        return Outcome::err(format!("write failed: {e}"));
    }
    ctx.mark_read(&p);
    // the model authored the whole file: its own words are the new base, so
    // a re-read is answerable with a pointer to this write
    ctx.read_ledger.note_write(
        &p,
        &content_hash(body.as_bytes()),
        ctx.current_call_id.clone(),
    );
    let label = rel_label(&ctx.root, &p);
    let diff = match prev.as_deref() {
        // compared without the marker on either side, or every write to a BOM'd
        // file would report line 1 as changed
        Some(before) => make_diff(split_bom(before).0, content),
        // a created file is all-added: the TUI expanded row and the
        // write-path lints (AF) see the same diff shape as an edit
        None => make_diff("", content),
    };
    let metadata = file_diff(
        &p,
        &ctx.root,
        prev.as_deref().map(str::as_bytes),
        body.as_bytes(),
        "write",
        checkpoint,
        &diff,
    );
    match prev {
        Some(_) => Outcome::ok(format!(
            "wrote {label} (+{}/-{})",
            metadata.added, metadata.removed
        ))
        .with_diff(diff)
        .with_file_diff(metadata),
        None => Outcome::ok(format!(
            "created {} ({} lines)",
            label,
            content.lines().count()
        ))
        .with_diff(diff)
        .with_file_diff(metadata),
    }
}

fn normalize_to_crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn normalize_to_lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// Normalize typographic Unicode quotes, dashes, and spaces to ASCII equivalents
/// (OpenCode `normalizeForMatch` convention) so model-emitted ASCII punctuation
/// matches source files containing curly quotes, en/em-dashes, or NBSPs.
pub(crate) fn normalize_typographic(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// Visual column width of leading whitespace (`\t` counts as 4 columns).
fn indent_cols(line: &str) -> usize {
    line.chars()
        .take_while(|&c| c == ' ' || c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

/// Split `s` into lines while remembering whether the text uses CRLF.
fn split_lines_lossless(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = s.split('\n').collect();
    // If `s` ends with `\n`, `split('\n')` leaves a trailing `""` element;
    // keep it out of the line list and track trailing newline separately.
    if s.ends_with('\n') {
        lines.pop();
    }
    lines
        .into_iter()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

/// Replace a line range `start..end` in `content` with `replacement_lines`,
/// preserving the file's newline style (`\r\n` vs `\n`) and final newline.
fn splice_line_range(
    content: &str,
    content_lines: &[&str],
    start: usize,
    end: usize,
    replacement_lines: &[String],
) -> String {
    let eol = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let trailing_nl = content.ends_with('\n');
    let mut out_lines: Vec<&str> = Vec::with_capacity(
        content_lines.len().saturating_sub(end - start) + replacement_lines.len(),
    );
    out_lines.extend_from_slice(&content_lines[..start]);
    for r in replacement_lines {
        out_lines.push(r.as_str());
    }
    out_lines.extend_from_slice(&content_lines[end..]);
    let mut joined = out_lines.join(eol);
    if trailing_nl && !out_lines.is_empty() {
        joined.push_str(eol);
    }
    joined
}

/// Stage 2–4 fallback matching (Aider / OpenCode style):
/// 1. Line-trimmed match (ignores trailing spaces/tabs on each line, plus
///    optional typographic quote/dash/space normalization).
/// 2. Relative-indentation match (preserves relative indentation inside the
///    block while adapting to the file's outer indentation prefix, including
///    tab-vs-space normalization).
/// 3. Outer blank-line trim retry if the model included extra leading/trailing
///    blank lines around `old_string`.
/// Only succeeds when the match is unambiguous (count == 1).
fn try_whitespace_or_indent_replace(
    content: &str,
    old: &str,
    new: &str,
) -> Result<Option<String>, String> {
    let content_lines = split_lines_lossless(content);
    let old_lines = split_lines_lossless(old);
    let new_lines = split_lines_lossless(new);
    if let Some(res) =
        try_whitespace_or_indent_lines(content, &content_lines, &old_lines, &new_lines)?
    {
        return Ok(Some(res));
    }

    // Retry with leading/trailing empty lines stripped from old_lines (and
    // matching leading/trailing empty lines stripped from new_lines) when the
    // model wrapped `old_string` with an extra blank boundary line.
    let lead_blank = old_lines.iter().take_while(|l| l.trim().is_empty()).count();
    let trail_blank = old_lines
        .iter()
        .rev()
        .take_while(|l| l.trim().is_empty())
        .count();
    if (lead_blank > 0 || trail_blank > 0) && lead_blank + trail_blank < old_lines.len() {
        let trimmed_old = &old_lines[lead_blank..old_lines.len() - trail_blank];
        let new_lead = new_lines
            .iter()
            .take(lead_blank)
            .take_while(|l| l.trim().is_empty())
            .count();
        let new_trail = new_lines
            .iter()
            .rev()
            .take(trail_blank)
            .take_while(|l| l.trim().is_empty())
            .count();
        let trimmed_new = if new_lead + new_trail <= new_lines.len() {
            &new_lines[new_lead..new_lines.len() - new_trail]
        } else {
            &new_lines[..]
        };
        if let Some(res) =
            try_whitespace_or_indent_lines(content, &content_lines, trimmed_old, trimmed_new)?
        {
            return Ok(Some(res));
        }
    }
    Ok(None)
}

fn try_whitespace_or_indent_lines(
    content: &str,
    content_lines: &[&str],
    old_lines: &[&str],
    new_lines: &[&str],
) -> Result<Option<String>, String> {
    if old_lines.is_empty() || old_lines.len() > content_lines.len() {
        return Ok(None);
    }
    let window = old_lines.len();

    // Pass A1: match ignoring trailing whitespace on each line.
    let mut trimmed_hits: Vec<usize> = Vec::new();
    for i in 0..=(content_lines.len() - window) {
        let ok = (0..window).all(|k| {
            content_lines[i + k].trim_end_matches([' ', '\t'])
                == old_lines[k].trim_end_matches([' ', '\t'])
        });
        if ok {
            trimmed_hits.push(i);
        }
    }
    if trimmed_hits.len() > 1 {
        return err(format!(
            "old_string appears {} times — provide more surrounding context or set replace_all=true",
            trimmed_hits.len()
        ));
    }
    if let [hit] = trimmed_hits.as_slice() {
        let rep: Vec<String> = new_lines.iter().map(|s| (*s).to_string()).collect();
        return Ok(Some(splice_line_range(
            content,
            content_lines,
            *hit,
            *hit + window,
            &rep,
        )));
    }

    // Pass A2: match ignoring trailing whitespace + typographic quote/dash/space variants.
    let norm_old: Vec<String> = old_lines
        .iter()
        .map(|l| normalize_typographic(l.trim_end_matches([' ', '\t'])))
        .collect();
    let mut typo_hits: Vec<usize> = Vec::new();
    for i in 0..=(content_lines.len() - window) {
        let ok = (0..window).all(|k| {
            normalize_typographic(content_lines[i + k].trim_end_matches([' ', '\t'])) == norm_old[k]
        });
        if ok {
            typo_hits.push(i);
        }
    }
    if typo_hits.len() > 1 {
        return err(format!(
            "old_string appears {} times — provide more surrounding context or set replace_all=true",
            typo_hits.len()
        ));
    }
    if let [hit] = typo_hits.as_slice() {
        let rep: Vec<String> = new_lines.iter().map(|s| (*s).to_string()).collect();
        return Ok(Some(splice_line_range(
            content,
            content_lines,
            *hit,
            *hit + window,
            &rep,
        )));
    }

    // Pass B1: relative-indentation match (Aider `match_but_for_leading_whitespace`).
    if !old_lines.iter().any(|l| !l.trim().is_empty()) {
        return Ok(None);
    }
    let min_old_indent = old_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let min_new_indent = new_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let strip_base = min_old_indent.min(min_new_indent);

    let outdented_old: Vec<&str> = old_lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                ""
            } else {
                &l[strip_base.min(l.len() - l.trim_start().len())..]
            }
        })
        .collect();
    let outdented_new: Vec<&str> = new_lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                ""
            } else {
                &l[strip_base.min(l.len() - l.trim_start().len())..]
            }
        })
        .collect();

    let mut indent_hits: Vec<(usize, String)> = Vec::new();
    for i in 0..=(content_lines.len() - window) {
        let mut prefix_opt: Option<&str> = None;
        let mut matched = true;
        for k in 0..window {
            let c_line = content_lines[i + k].trim_end_matches([' ', '\t']);
            let o_line = outdented_old[k].trim_end_matches([' ', '\t']);
            if o_line.trim().is_empty() {
                if !c_line.trim().is_empty() {
                    matched = false;
                    break;
                }
                continue;
            }
            if c_line.trim_start() != o_line.trim_start() {
                matched = false;
                break;
            }
            let Some(prefix) = c_line.strip_suffix(o_line) else {
                matched = false;
                break;
            };
            if !prefix.chars().all(|ch| ch == ' ' || ch == '\t') {
                matched = false;
                break;
            }
            match prefix_opt {
                None => prefix_opt = Some(prefix),
                Some(prev) if prev != prefix => {
                    matched = false;
                    break;
                }
                Some(_) => {}
            }
        }
        if matched && let Some(prefix) = prefix_opt {
            indent_hits.push((i, prefix.to_string()));
        }
    }
    if indent_hits.len() > 1 {
        return err(format!(
            "old_string appears {} times — provide more surrounding context or set replace_all=true",
            indent_hits.len()
        ));
    }
    if let [(hit, prefix)] = indent_hits.as_slice() {
        let rep: Vec<String> = outdented_new
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    String::new()
                } else {
                    format!("{prefix}{l}")
                }
            })
            .collect();
        return Ok(Some(splice_line_range(
            content,
            content_lines,
            *hit,
            *hit + window,
            &rep,
        )));
    }

    // Pass B2: column-based relative indentation + typographic normalization
    // (handles tab-indented files when the model sent spaces, or vice versa).
    let base_old_cols = old_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| indent_cols(l))
        .min()
        .unwrap_or(0);
    let base_new_cols = new_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| indent_cols(l))
        .min()
        .unwrap_or(0);
    let strip_cols = base_old_cols.min(base_new_cols);

    let mut col_hits: Vec<(usize, usize, bool)> = Vec::new();
    for i in 0..=(content_lines.len() - window) {
        let mut delta_cols: Option<usize> = None;
        let mut uses_tabs = false;
        let mut matched = true;
        for k in 0..window {
            let c_line = content_lines[i + k].trim_end_matches([' ', '\t']);
            let o_line = old_lines[k].trim_end_matches([' ', '\t']);
            if o_line.trim().is_empty() {
                if !c_line.trim().is_empty() {
                    matched = false;
                    break;
                }
                continue;
            }
            if normalize_typographic(c_line.trim_start())
                != normalize_typographic(o_line.trim_start())
            {
                matched = false;
                break;
            }
            let c_cols = indent_cols(c_line);
            let o_rel_cols = indent_cols(o_line).saturating_sub(strip_cols);
            if c_cols < o_rel_cols {
                matched = false;
                break;
            }
            if c_line.starts_with('\t') {
                uses_tabs = true;
            }
            let d = c_cols - o_rel_cols;
            match delta_cols {
                None => delta_cols = Some(d),
                Some(prev) if prev != d => {
                    matched = false;
                    break;
                }
                Some(_) => {}
            }
        }
        if matched && let Some(d) = delta_cols {
            col_hits.push((i, d, uses_tabs));
        }
    }
    if col_hits.len() > 1 {
        return err(format!(
            "old_string appears {} times — provide more surrounding context or set replace_all=true",
            col_hits.len()
        ));
    }
    if let [(hit, delta_cols, uses_tabs)] = col_hits.as_slice() {
        let rep: Vec<String> = new_lines
            .iter()
            .map(|l| {
                let trimmed = l.trim_end_matches([' ', '\t']);
                if trimmed.trim().is_empty() {
                    String::new()
                } else {
                    let rel_cols = indent_cols(trimmed).saturating_sub(strip_cols);
                    let total_cols = delta_cols + rel_cols;
                    let indent_prefix = if *uses_tabs {
                        format!(
                            "{}{}",
                            "\t".repeat(total_cols / 4),
                            " ".repeat(total_cols % 4)
                        )
                    } else {
                        " ".repeat(total_cols)
                    };
                    format!("{indent_prefix}{}", trimmed.trim_start())
                }
            })
            .collect();
        return Ok(Some(splice_line_range(
            content,
            content_lines,
            *hit,
            *hit + window,
            &rep,
        )));
    }
    Ok(None)
}

/// Find the closest matching line window in `content` when `old` fails to
/// match, returning a numbered snippet so the model can fix the edit without
/// issuing a separate `read` call.
fn find_similar_window(content: &str, old: &str) -> Option<String> {
    let content_lines: Vec<&str> = content.lines().collect();
    let old_trimmed: Vec<&str> = old
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if content_lines.is_empty() || old_trimmed.is_empty() {
        return None;
    }
    let window = old.lines().count().max(1).min(content_lines.len());
    let mut best_score = 0usize;
    let mut best_start = 0usize;
    for i in 0..=(content_lines.len() - window) {
        let slice = &content_lines[i..i + window];
        let score = old_trimmed
            .iter()
            .filter(|&&needle| slice.iter().any(|&cand| cand.trim() == needle))
            .count();
        if score > best_score {
            best_score = score;
            best_start = i;
        }
    }
    if best_score == 0 {
        // Fallback: search for any line containing the longest non-empty line of `old`.
        let anchor = old_trimmed.iter().max_by_key(|l| l.len())?;
        if anchor.len() < 4 {
            return None;
        }
        let pos = content_lines.iter().position(|l| l.contains(anchor))?;
        best_start = pos.saturating_sub(1);
    } else if best_score * 2 < old_trimmed.len() && best_score < 2 {
        return None;
    }
    let from = best_start.saturating_sub(2);
    let to = (best_start + window + 2)
        .min(content_lines.len())
        .min(from + 12);
    let mut snippet = format!("\nClosest lines in file (lines {}–{}):\n", from + 1, to);
    for (idx, line) in content_lines.iter().enumerate().take(to).skip(from) {
        snippet.push_str(&format!("{:>6}\t{}\n", idx + 1, line));
    }
    Some(snippet.trim_end().to_string())
}

pub(super) fn apply_one(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<String, String> {
    if old.is_empty() {
        return err("old_string must not be empty");
    }
    if old == new {
        return err("old_string and new_string are identical — no changes to apply");
    }
    let (target_old, target_new) = if content.matches(old).count() > 0 {
        (
            std::borrow::Cow::Borrowed(old),
            std::borrow::Cow::Borrowed(new),
        )
    } else {
        let crlf_old = normalize_to_crlf(old);
        if content.matches(&crlf_old).count() > 0 {
            (
                std::borrow::Cow::Owned(crlf_old),
                std::borrow::Cow::Owned(normalize_to_crlf(new)),
            )
        } else {
            let lf_old = normalize_to_lf(old);
            if content.matches(&lf_old).count() > 0 {
                (
                    std::borrow::Cow::Owned(lf_old),
                    std::borrow::Cow::Owned(normalize_to_lf(new)),
                )
            } else {
                (
                    std::borrow::Cow::Borrowed(old),
                    std::borrow::Cow::Borrowed(new),
                )
            }
        }
    };

    let count = content.matches(target_old.as_ref()).count();
    if count == 0 {
        if !replace_all && let Some(replaced) = try_whitespace_or_indent_replace(content, old, new)?
        {
            return Ok(replaced);
        }
        let mut msg = String::from("old_string not found in file");
        if !new.trim().is_empty()
            && (content.contains(new)
                || content.contains(&normalize_to_lf(new))
                || content.contains(&normalize_to_crlf(new)))
        {
            msg.push_str(" — note: new_string is already present in the file");
        }
        if let Some(similar) = find_similar_window(content, old) {
            msg.push_str(&similar);
        }
        return err(msg);
    }
    if count > 1 && !replace_all {
        return err(format!(
            "old_string appears {count} times — provide more surrounding context or set replace_all=true"
        ));
    }
    Ok(if replace_all {
        content.replace(target_old.as_ref(), target_new.as_ref())
    } else {
        content.replacen(target_old.as_ref(), target_new.as_ref(), 1)
    })
}

/// The 1-based line span the change occupies in the file's new numbering, or
/// `None` when nothing was left behind at that place (a pure delete).
fn changed_span(before: &str, after: &str) -> Option<(usize, usize)> {
    let bl: Vec<&str> = before.lines().collect();
    let al: Vec<&str> = after.lines().collect();
    let pre = bl.iter().zip(al.iter()).take_while(|(x, y)| x == y).count();
    let shared = (bl.len() - pre).min(al.len() - pre);
    let suf = (0..shared)
        .take_while(|k| bl[bl.len() - 1 - k] == al[al.len() - 1 - k])
        .count();
    let (from, to) = (pre + 1, al.len() - suf);
    (from <= to).then_some((from, to))
}

/// Where a single edit landed, in the file's new line numbers, with the first
/// line of what went in plus 1–2 surrounding boundary lines with their new
/// line numbers so consecutive edits further down never require a re-read.
fn edit_landing(before: &str, after: &str) -> String {
    let al: Vec<&str> = after.lines().collect();
    let Some((from, to)) = changed_span(before, after) else {
        let common = before
            .lines()
            .zip(al.iter().copied())
            .take_while(|(x, y)| x == y)
            .count();
        let line_no = common + 1;
        let mut out = format!("→ at line {line_no}");
        if let Some(next_line) = al.get(common) {
            out.push_str(&format!("\n{:>6}\t{}", line_no, next_line));
        }
        return out;
    };
    let head = al.get(from - 1).copied().unwrap_or_default().trim();
    let head = if head.chars().count() > 60 {
        format!("{}…", head.chars().take(60).collect::<String>())
    } else {
        head.to_string()
    };
    let mut out = if from == to {
        format!("→ line {from}: \"{head}\"")
    } else {
        format!("→ lines {from}–{to}: \"{head}\"")
    };
    let mut ctx_lines = String::new();
    if from > 1
        && let Some(prev_line) = al.get(from - 2)
    {
        ctx_lines.push_str(&format!("\n{:>6}\t{}", from - 1, prev_line));
    }
    for post_idx in to..(to + 2).min(al.len()) {
        if let Some(post_line) = al.get(post_idx) {
            ctx_lines.push_str(&format!("\n{:>6}\t{}", post_idx + 1, post_line));
        }
    }
    out.push_str(&ctx_lines);
    out
}

pub(super) fn edit(
    ctx: &mut ToolCtx,
    raw: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Outcome {
    let p = match existing_writable(ctx, raw) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    // mass replacement without a fresh read is blind destruction; a single
    // exact edit is self-validating (match-or-error before touching disk)
    // and only needs the file to be non-stale
    if replace_all {
        if let Err(e) = require_read(ctx, &p) {
            return Outcome::err(e);
        }
    } else if let Err(e) = require_fresh(ctx, &p) {
        return Outcome::err(e);
    }
    let original = match fs::read_to_string(&p) {
        Ok(c) => c,
        Err(e) => return Outcome::err(format!("read failed: {e}")),
    };
    // #214 L26: the marker is matched and diffed away from, so an anchor taken
    // from line 1 of what `read` showed really lands, and put back on the bytes
    // handed to the filesystem, so the file keeps the encoding it had.
    let (content, had_bom) = split_bom(&original);
    let edited = match apply_one(content, old, new, replace_all) {
        Ok(u) => u,
        Err(e) => return Outcome::err(e),
    };
    let updated = restore_bom(edited, had_bom);
    let checkpoint_id = Some(checkpoint(ctx, &p, "edit"));
    let hash_before = content_hash(original.as_bytes());
    if let Err(e) = fs::write(&p, &updated) {
        return Outcome::err(format!("write failed: {e}"));
    }
    ctx.mark_read(&p);
    let (shown, _) = split_bom(&updated);
    let diff = make_diff(content, shown);
    let (add, rem) = diff_counts(&diff);
    ctx.read_ledger.note_edit(
        &p,
        "edit",
        add,
        rem,
        &hash_before,
        &content_hash(updated.as_bytes()),
        ctx.current_call_id.clone(),
    );
    let metadata = file_diff(
        &p,
        &ctx.root,
        // the pre- and post-images are the file's bytes, marker and all: this
        // is what the blob store and `/undo` restore, not the model's view
        Some(original.as_bytes()),
        updated.as_bytes(),
        "edit",
        checkpoint_id,
        &diff,
    );
    let out_msg = format!(
        "edited {} (+{add}/-{rem}) {}",
        rel_label(&ctx.root, &p),
        edit_landing(content, shown)
    );
    Outcome::ok(out_msg)
        .with_diff(diff)
        .with_file_diff(metadata)
}

pub(super) fn ls(ctx: &mut ToolCtx, raw: &str) -> Outcome {
    let p = match ctx.resolve(raw) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    if !p.is_dir() {
        return Outcome::err(format!("not a directory: {}", p.display()));
    }
    let mut rows: Vec<(bool, u64, String)> = Vec::new();
    let rd = match fs::read_dir(&p) {
        Ok(r) => r,
        Err(e) => return Outcome::err(format!("readdir failed: {e}")),
    };
    for e in rd.flatten() {
        let meta = e.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        rows.push((is_dir, size, e.file_name().to_string_lossy().into_owned()));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(&b.2)));
    let _ = Kind::ReadOnly; // kind metadata lives in the registry
    let body: Vec<String> = rows
        .iter()
        .map(|(d, s, n)| {
            if *d {
                format!("{n}/")
            } else {
                format!("{n} ({s} B)")
            }
        })
        .collect();
    Outcome::ok(if body.is_empty() {
        "(empty directory)".into()
    } else {
        body.join("\n")
    })
}

/// Split comma-separated glob patterns at brace depth 0 (`"*.rs, *.toml"` -> `["*.rs", "*.toml"]`),
/// while preserving brace expansions like `"*.{ts,tsx}"` as a single pattern.
fn split_glob_patterns(raw: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, ch) in raw.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let slice = raw[start..idx].trim();
                if !slice.is_empty() {
                    parts.push(slice);
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    let tail = raw[start..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }
    if parts.is_empty() {
        vec![raw.trim()]
    } else {
        parts
    }
}

fn sanitize_search_path(path: Option<&str>) -> Option<&str> {
    path.map(str::trim)
        .filter(|s| !s.is_empty() && *s != "undefined" && *s != "null")
}

pub(super) fn glob(ctx: &mut ToolCtx, pattern: &str, base: Option<&str>) -> Outcome {
    use globset::{GlobBuilder, GlobSetBuilder};
    use ignore::WalkBuilder;

    let base_dir = match sanitize_search_path(base) {
        Some(b) => match ctx.resolve(b) {
            Ok(p) => p,
            Err(e) => return Outcome::err(e),
        },
        None => ctx.root.clone(),
    };
    let sub_patterns = split_glob_patterns(pattern);
    let mut set_builder = GlobSetBuilder::new();
    let mut basename_builder = GlobSetBuilder::new();
    let mut has_basename = false;
    for pat in sub_patterns {
        match GlobBuilder::new(pat).literal_separator(true).build() {
            Ok(g) => {
                set_builder.add(g);
            }
            Err(e) => return Outcome::err(format!("bad glob pattern: {e}")),
        }
        if !pat.contains('/')
            && !pat.contains('\\')
            && let Ok(bg) = GlobBuilder::new(pat).build()
        {
            basename_builder.add(bg);
            has_basename = true;
        }
    }
    let glob = match set_builder.build() {
        Ok(gs) => gs,
        Err(e) => return Outcome::err(format!("bad glob pattern: {e}")),
    };
    let basename_glob = if has_basename {
        basename_builder.build().ok()
    } else {
        None
    };
    let mut hits: Vec<(std::time::SystemTime, String)> = Vec::new();
    // host-owned state and .git internals are never listed, while dot-config
    // directories (.github/, .cargo/) remain visible.
    let host_owned = ctx.host_state_dir();
    for entry in WalkBuilder::new(&base_dir).hidden(false).build().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path == host_owned || path.starts_with(&host_owned) {
            continue;
        }
        let rel = if base_dir.is_file() {
            path.strip_prefix(&ctx.root).unwrap_or(path)
        } else {
            path.strip_prefix(&base_dir).unwrap_or(path)
        };
        if rel
            .components()
            .any(|c| c.as_os_str() == ".git" || c.as_os_str() == ".sqwai")
        {
            continue;
        }
        let matched = glob.is_match(rel)
            || basename_glob.as_ref().is_some_and(|bg| {
                path.file_name()
                    .is_some_and(|name| bg.is_match(Path::new(name)))
            });
        if matched {
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::UNIX_EPOCH);
            hits.push((mtime, rel.to_string_lossy().replace('\\', "/")));
        }
    }
    // Newest modified files first so active work surfaces before the 300-file cap.
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let truncated = hits.len() > 300;
    let mut out: Vec<String> = hits.into_iter().take(300).map(|(_, p)| p).collect();
    if truncated {
        out.push("…(more results truncated)".into());
    }
    Outcome::ok(if out.is_empty() {
        "no matches".into()
    } else {
        out.join("\n")
    })
}

pub(super) fn grep(
    ctx: &mut ToolCtx,
    pattern: &str,
    path: Option<&str>,
    include: Option<&str>,
    context: usize,
) -> Outcome {
    use globset::{GlobBuilder, GlobSetBuilder};
    use ignore::WalkBuilder;

    let re = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => return Outcome::err(format!("bad regex: {e}")),
    };
    let multiline_re = regex::RegexBuilder::new(pattern)
        .multi_line(true)
        .build()
        .ok();
    let base_dir = match sanitize_search_path(path) {
        Some(b) => match ctx.resolve(b) {
            Ok(p) => p,
            Err(e) => return Outcome::err(e),
        },
        None => ctx.root.clone(),
    };
    let inc = match include.map(str::trim).filter(|s| !s.is_empty()) {
        Some(g) => {
            let mut builder = GlobSetBuilder::new();
            for pat in split_glob_patterns(g) {
                match GlobBuilder::new(pat).build() {
                    Ok(gb) => {
                        builder.add(gb);
                    }
                    Err(e) => return Outcome::err(format!("bad include glob pattern: {e}")),
                }
            }
            match builder.build() {
                Ok(set) => Some(set),
                Err(e) => return Outcome::err(format!("bad include glob pattern: {e}")),
            }
        }
        None => None,
    };

    fn show_line(line: &str) -> String {
        const MAX_LINE_BYTES: usize = 1024;
        let trimmed = line.trim_end();
        if trimmed.len() > MAX_LINE_BYTES {
            let mut cut = MAX_LINE_BYTES;
            while cut > 0 && !trimmed.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}…(line truncated)", &trimmed[..cut])
        } else {
            trimmed.to_string()
        }
    }

    // same host-owned skip as glob above (see comment there)
    let host_owned = ctx.root.join(".sqwai");
    // …with one exception, in the same spelling `resolve` hands back: a
    // truncated result points the model at `.sqwai/spill/<file>` and tells it
    // to grep that file, so the skip must not refuse the very path the host
    // named. Only a walk *named* into the spill dir sees what it names — a
    // sweep from the project root still never reaches host state. Same
    // asymmetry `ctx::resolve` grants `read`.
    let spill = host_owned.join("spill");
    let spill_named = base_dir.starts_with(&spill);

    let mut candidates: Vec<(std::time::SystemTime, String, std::path::PathBuf)> = Vec::new();
    for entry in WalkBuilder::new(&base_dir).hidden(false).build().flatten() {
        let entry_path = entry.path();
        if !entry_path.is_file() {
            continue;
        }
        if entry_path.starts_with(&host_owned) && !(spill_named && entry_path.starts_with(&spill)) {
            continue;
        }
        let rel_check = entry_path.strip_prefix(&ctx.root).unwrap_or(entry_path);
        if rel_check.components().any(|c| c.as_os_str() == ".git")
            || (!spill_named && rel_check.components().any(|c| c.as_os_str() == ".sqwai"))
        {
            continue;
        }
        let disp = entry_path.strip_prefix(&ctx.root).unwrap_or(entry_path);
        let shown = disp.display().to_string().replace('\\', "/");
        if let Some(f) = &inc {
            let name = entry_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !f.is_match(&name) && !f.is_match(&shown) {
                continue;
            }
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        candidates.push((mtime, shown, entry_path.to_path_buf()));
    }
    // Search newest modified files first so active files surface before the 200-match cap.
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    const SHOWN_MATCH_CAP: usize = 200;
    const SPILL_MATCH_CAP: usize = 2_000;
    let mut out = String::new();
    let mut spill_buf = String::new();
    let mut total_matches = 0usize;
    let mut byte_truncated = false;

    'outer: for (_, shown, file_path) in &candidates {
        // The whole file is scanned even when some of its bytes are not valid
        // UTF-8. Collecting `Lines` into `Result<_, _>` turned the first
        // invalid byte into an empty line list, so a cp1251 source reported
        // "no matches" for a pattern sitting on its third line (#214 M18).
        // An unreadable file is skipped, not fatal: one locked path must not
        // end the search.
        let Ok(raw) = fs::read(file_path) else {
            continue;
        };
        if raw.contains(&0) {
            continue; // binary-ish
        }
        let text = String::from_utf8_lossy(&raw);
        // Fast whole-file pre-check avoids line splitting when a file has 0 matches.
        if let Some(mre) = &multiline_re
            && !mre.is_match(&text)
        {
            continue;
        }
        let lines: Vec<&str> = text.lines().collect();
        let mut shown_hits: Vec<usize> = Vec::new();
        let mut spill_hits: Vec<usize> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if re.is_match(line) {
                total_matches += 1;
                if total_matches <= SHOWN_MATCH_CAP {
                    shown_hits.push(i);
                }
                if total_matches <= SPILL_MATCH_CAP {
                    spill_hits.push(i);
                } else {
                    break;
                }
            }
        }
        if spill_hits.is_empty() {
            continue;
        }

        // Format into `spill_buf` (up to 2,000 matches) and `out` (first 200 matches).
        let format_hits = |target: &mut String, hits: &[usize]| -> bool {
            let mut last_printed: Option<usize> = None;
            for &hit in hits {
                let start = hit.saturating_sub(context);
                let end = (hit + context + 1).min(lines.len());
                if let Some(prev) = last_printed
                    && start > prev + 1
                {
                    target.push_str("--\n");
                }
                for (i, line) in lines.iter().enumerate().take(end).skip(start) {
                    if last_printed.is_some_and(|p| i <= p) {
                        continue;
                    }
                    let sep = if i == hit { ':' } else { '-' };
                    target.push_str(&format!("{}{sep}{}: {}\n", shown, i + 1, show_line(line)));
                    last_printed = Some(i);
                }
                if target.len() > 300_000 {
                    return true;
                }
            }
            false
        };

        if !shown_hits.is_empty() && format_hits(&mut out, &shown_hits) {
            out.push_str("…(output truncated)\n");
            byte_truncated = true;
            break 'outer;
        }
        let _ = format_hits(&mut spill_buf, &spill_hits);
        if total_matches >= SPILL_MATCH_CAP {
            break 'outer;
        }
    }

    if !byte_truncated && total_matches > SHOWN_MATCH_CAP {
        match spill_output(ctx, "grep", &spill_buf) {
            Some(rel) => out.push_str(&format!(
                "…(more matches truncated: first 200 shown ({total_matches} total), full list in {rel} — grep it with a narrower pattern instead of re-running broad)\n"
            )),
            None => out.push_str("…(more matches truncated)\n"),
        }
    }
    Outcome::ok(if out.is_empty() {
        json_out_no_matches(pattern)
    } else {
        out
    })
}

fn json_out_no_matches(pattern: &str) -> String {
    format!("no matches for /{pattern}/")
}

fn checkpoint(ctx: &mut ToolCtx, p: &Path, what: &str) -> String {
    let label = format!("{what} {}", rel_label(&ctx.root, p));
    let id = crate::agent::checkpoints::checkpoint_for_mutation(
        &ctx.root,
        ctx.shadow_store,
        ctx.checkpoint_chain(),
        &label,
    );
    ctx.checkpoints.push((id.clone(), label));
    id
}

fn rel_label(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

// keep json import used even if helpers change
#[allow(unused)]
fn _touch() -> serde_json::Value {
    json!(null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_edits_do_not_require_rereading() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "hello world").unwrap();

        let mut ctx = ToolCtx::new(dir.path());
        let read_outcome = read(&mut ctx, "test.txt", &json!({}));
        assert!(read_outcome.ok);

        let edit1 = edit(&mut ctx, "test.txt", "world", "rust", false);
        assert!(edit1.ok, "first edit failed: {}", edit1.output);

        let edit2 = edit(&mut ctx, "test.txt", "rust", "sqwai", false);
        assert!(edit2.ok, "second edit failed: {}", edit2.output);
    }

    #[test]
    fn repeat_read_of_unchanged_window_collapses_to_stub() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        let args = serde_json::json!({});
        let first = read(&mut ctx, "a.txt", &args);
        assert!(first.ok);
        assert!(first.output.contains("one"), "{}", first.output);
        // same window, unchanged file: stub instead of the bytes again
        let second = read(&mut ctx, "a.txt", &args);
        assert!(second.ok);
        assert!(
            second.output.contains("unchanged since your last read"),
            "{}",
            second.output
        );
        // after an edit a whole-file read is the delta pointer, not the bytes
        let edited = edit(&mut ctx, "a.txt", "two", "TWO", false);
        assert!(edited.ok, "{}", edited.output);
        let third = read(&mut ctx, "a.txt", &args);
        assert!(third.ok);
        assert!(third.output.contains("No full re-read"), "{}", third.output);
        assert!(third.output.contains("edit +1/-1"), "{}", third.output);
        assert!(!third.output.contains("TWO"), "{}", third.output);
        // exact lines are still real, and a second whole request escapes the
        // pointer so the model can never be trapped by it
        let ranged = read(&mut ctx, "a.txt", &json!({"offset": 1, "limit": 3}));
        assert!(ranged.ok);
        assert!(ranged.output.contains("TWO"), "{}", ranged.output);
        let fourth = read(&mut ctx, "a.txt", &args);
        assert!(fourth.ok);
        assert!(fourth.output.contains("five"), "{}", fourth.output);
    }

    #[test]
    fn whole_file_delta_survives_a_new_user_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = ToolCtx::new(dir.path());
        first.current_call_id = Some("w1".into());
        let written = write_file(&mut first, "main.js", "one\ntwo\nthree\nfour\nfive\n");
        assert!(written.ok, "{}", written.output);

        // a write alone is already a full base
        let mut after_write = ToolCtx::new(dir.path());
        after_write.read_ledger = first.read_ledger.clone();
        let write_pointer = read(&mut after_write, "main.js", &json!({}));
        assert!(write_pointer.ok);
        assert!(
            write_pointer.output.contains("No full re-read")
                && write_pointer.output.contains("your write at")
                && write_pointer
                    .output
                    .contains("exactly what you sent to `write`"),
            "{}",
            write_pointer.output
        );

        first.current_call_id = Some("e1".into());
        let edited = edit(&mut first, "main.js", "two", "TWO", false);
        assert!(edited.ok, "{}", edited.output);

        // the next user submit gets a fresh ToolCtx; only the session ledger
        // crosses the run boundary
        let mut next = ToolCtx::new(dir.path());
        next.read_ledger = first.read_ledger.clone();
        next.current_call_id = Some("r1".into());
        let full = read(&mut next, "main.js", &json!({}));
        assert!(full.ok);
        assert!(full.output.contains("No full re-read"), "{}", full.output);
        assert!(full.output.contains("your write at"), "{}", full.output);
        assert!(full.output.contains("edit +1/-1"), "{}", full.output);
        assert!(!full.output.contains("TWO"), "{}", full.output);

        // an explicit offset/limit is the model asking for lines, even when it
        // covers the small file: no pointer
        let explicit_full = read(&mut next, "main.js", &json!({"offset": 1, "limit": 10}));
        assert!(explicit_full.ok);
        assert!(
            explicit_full.output.contains("TWO"),
            "{}",
            explicit_full.output
        );
        assert!(
            !explicit_full.output.contains("No full re-read"),
            "{}",
            explicit_full.output
        );

        // the pointer satisfies the edit guard and continues the chain
        let again = edit(&mut next, "main.js", "three", "THREE", false);
        assert!(again.ok, "{}", again.output);

        // explicit lines are real, not a pointer
        let ranged = read(&mut next, "main.js", &json!({"offset": 1, "limit": 3}));
        assert!(ranged.ok);
        assert!(ranged.output.contains("THREE"), "{}", ranged.output);
    }

    #[test]
    fn external_change_breaks_the_delta_chain() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        ctx.current_call_id = Some("w1".into());
        let written = write_file(&mut ctx, "a.txt", "one\ntwo\n");
        assert!(written.ok, "{}", written.output);
        ctx.current_call_id = Some("e1".into());
        let edited = edit(&mut ctx, "a.txt", "two", "TWO", false);
        assert!(edited.ok, "{}", edited.output);

        // a bash/editor write: not in the ledger's hash chain
        std::fs::write(dir.path().join("a.txt"), "one\nTWO\nexternal\n").unwrap();
        let mut next = ToolCtx::new(dir.path());
        next.read_ledger = ctx.read_ledger.clone();
        let full = read(&mut next, "a.txt", &json!({}));
        assert!(full.ok);
        assert!(!full.output.contains("No full re-read"), "{}", full.output);
        assert!(full.output.contains("external"), "{}", full.output);
    }

    #[test]
    fn delta_pointer_requires_visible_base_and_edit_results() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        ctx.current_call_id = Some("w1".into());
        let written = write_file(&mut ctx, "a.txt", "one\ntwo\n");
        assert!(written.ok, "{}", written.output);
        ctx.current_call_id = Some("e1".into());
        let edited = edit(&mut ctx, "a.txt", "two", "TWO", false);
        assert!(edited.ok, "{}", edited.output);

        let mut next = ToolCtx::new(dir.path());
        next.read_ledger = ctx.read_ledger.clone();
        // the write result is still visible, the edit result was cleared
        next.visible_calls = Some(std::collections::HashSet::from(["w1".to_string()]));
        let full = read(&mut next, "a.txt", &json!({}));
        assert!(full.ok);
        assert!(!full.output.contains("No full re-read"), "{}", full.output);
        assert!(full.output.contains("TWO"), "{}", full.output);
    }

    #[test]
    fn partial_read_does_not_create_a_delta_base() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        let partial = read(&mut ctx, "a.txt", &json!({"offset": 1, "limit": 2}));
        assert!(partial.ok);
        let edited = edit(&mut ctx, "a.txt", "two", "TWO", false);
        assert!(edited.ok, "{}", edited.output);
        let full = read(&mut ctx, "a.txt", &json!({}));
        assert!(full.ok);
        assert!(!full.output.contains("No full re-read"), "{}", full.output);
        assert!(full.output.contains("TWO"), "{}", full.output);
    }

    /// Coverage, not equality: the live session's five looks at one region were
    /// five different windows, and an exact-match guard served all of them.
    #[test]
    fn a_window_covered_by_an_earlier_read_collapses_to_a_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), body).unwrap();
        let mut ctx = ToolCtx::new(dir.path());

        let wide = read(&mut ctx, "big.txt", &json!({"offset": 1, "limit": 15}));
        assert!(wide.ok);
        assert!(wide.output.contains("line 7"), "{}", wide.output);

        // inside the earlier window: no bytes, just where to look
        let inside = read(&mut ctx, "big.txt", &json!({"offset": 5, "limit": 4}));
        assert!(
            inside.output.contains("unchanged since your last read")
                && inside.output.contains("1–15"),
            "a covered window must collapse: {}",
            inside.output
        );
        assert!(!inside.output.contains("line 7"), "must not re-send lines");

        // past the covered range it serves normally, and the record extends
        let beyond = read(&mut ctx, "big.txt", &json!({"offset": 16, "limit": 5}));
        assert!(beyond.output.contains("line 18"), "{}", beyond.output);
        let now_covered = read(&mut ctx, "big.txt", &json!({"offset": 14, "limit": 4}));
        assert!(
            now_covered
                .output
                .contains("unchanged since your last read"),
            "14–17 is covered by 1–15 plus 16–20: {}",
            now_covered.output
        );
    }

    /// The map comes with the first window of a big file, once.
    #[test]
    fn a_first_read_of_a_large_file_carries_its_outline() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=450).map(|i| format!("fn f{i}() {{}}\n")).collect();
        std::fs::write(dir.path().join("many.rs"), &body).unwrap();
        let mut ctx = ToolCtx::new(dir.path());

        let first = read(&mut ctx, "many.rs", &json!({"offset": 1, "limit": 30}));
        assert!(
            first.output.contains("structure of this file"),
            "the first window must carry the map: {}",
            first.output
        );
        assert!(
            first.output.contains("f400") || first.output.contains("…(truncated)"),
            "the map must reach past the served window: {}",
            first.output
        );

        let second = read(&mut ctx, "many.rs", &json!({"offset": 200, "limit": 30}));
        assert!(
            !second.output.contains("structure of this file"),
            "the map is attached once per file"
        );
        // and after enough windows, the host points at the cheaper tools
        for offset in [300, 350, 400] {
            read(&mut ctx, "many.rs", &json!({"offset": offset, "limit": 20}));
        }
        let nudged = read(&mut ctx, "many.rs", &json!({"offset": 100, "limit": 20}));
        assert!(
            nudged.output.contains("window") && nudged.output.contains("`outline`"),
            "the slicing nudge should fire after several windows: {}",
            nudged.output
        );
    }

    #[test]
    fn grep_invalid_include_glob_returns_error_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        let outcome = grep(&mut ctx, "test", None, Some("[unclosed"), 0);
        assert!(!outcome.ok);
        assert!(outcome.output.contains("bad include glob pattern"));
    }

    /// #214 M18: a single undecodable line used to erase the rest of the
    /// file. `rd.lines().collect::<Result<_, _>>()` returned `Err` on the
    /// first invalid byte and `unwrap_or_default` turned that into an empty
    /// line list, so the tool answered "no matches" about a pattern sitting
    /// further down — the quietest possible failure.
    #[test]
    fn grep_scans_past_a_line_that_is_not_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = b"first line\n".to_vec();
        // the second line is cp1251 text: fine in its own codepage, invalid
        // UTF-8 from its very first byte
        bytes.extend_from_slice(&[0xd0, 0xef, 0xff, 0xf1, 0xfe, 0xff, b':', b' ', 0x80, b'\n']);
        bytes.extend_from_slice(b"needle after the bad line\n");
        std::fs::write(dir.path().join("mixed.txt"), &bytes).unwrap();
        std::fs::write(dir.path().join("bin.dat"), b"needle\x00after\n").unwrap();
        let mut ctx = ToolCtx::new(dir.path());

        let hit = grep(&mut ctx, "needle", None, None, 0);
        assert!(hit.ok, "{}", hit.output);
        assert!(
            hit.output.contains("needle after the bad line"),
            "a non-UTF-8 line must not end the scan: {}",
            hit.output
        );

        // the bad line itself stays reachable, shown lossily as context
        let lossy = grep(&mut ctx, "after", None, None, 1);
        assert!(
            lossy.output.contains('\u{fffd}'),
            "invalid bytes should decode to the replacement character: {}",
            lossy.output
        );

        // and a NUL still means binary: skipped, not scanned
        let bin = grep(&mut ctx, "needle", None, Some("*.dat"), 0);
        assert!(
            bin.output.contains("no matches"),
            "binary content must stay out of results: {}",
            bin.output
        );
    }

    #[test]
    fn grep_long_line_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let long_line = format!("match_{}", "a".repeat(3000));
        std::fs::write(dir.path().join("long.txt"), &long_line).unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        let outcome = grep(&mut ctx, "match_", None, None, 0);
        assert!(outcome.ok);
        assert!(outcome.output.contains("…(line truncated)"));
        assert!(outcome.output.len() < 2000);
    }

    #[test]
    fn grep_context_shows_surrounding_lines_like_grep_c() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("code.txt"),
            "line one\nline two\nMATCH here\nline four\nline five\n",
        )
        .unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        let outcome = grep(&mut ctx, "MATCH", None, None, 1);
        assert!(outcome.ok);
        // match line with `:`, context lines with `-`
        assert!(
            outcome.output.contains("code.txt:3: MATCH here"),
            "{}",
            outcome.output
        );
        assert!(outcome.output.contains("code.txt-2:"), "{}", outcome.output);
        assert!(outcome.output.contains("code.txt-4:"), "{}", outcome.output);
        // default stays context-free
        let plain = grep(&mut ctx, "MATCH", None, None, 0);
        assert!(plain.ok);
        assert!(!plain.output.contains("code.txt-2:"), "{}", plain.output);
    }

    #[test]
    fn host_owned_state_is_invisible_to_grep_and_glob() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".sqwai").join("plans")).unwrap();
        std::fs::write(
            dir.path().join(".sqwai").join("plans").join("secret.json"),
            "host_secret_marker_xyz",
        )
        .unwrap();
        std::fs::write(dir.path().join("visible.txt"), "plain content").unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        // grep must not match inside host-owned state (pre-fix: leaked on
        // Windows where hidden(true) checks attributes, not dotfiles)
        let outcome = grep(&mut ctx, "host_secret_marker_xyz", None, None, 0);
        assert!(outcome.ok);
        assert!(!outcome.output.contains(".sqwai"), "{}", outcome.output);
        // glob must not list host-owned files either
        let listed = glob(&mut ctx, "**/*", None);
        assert!(listed.ok);
        assert!(!listed.output.contains(".sqwai"), "{}", listed.output);
        assert!(listed.output.contains("visible.txt"), "{}", listed.output);
    }

    /// The spill dir stays out of every sweep, but the one file a truncated
    /// result named is searchable — the note telling the model to `grep` it
    /// was a promise the tool could not keep.
    #[test]
    fn the_file_a_spill_note_named_is_the_one_grep_may_open() {
        let dir = tempfile::tempdir().unwrap();
        let spill = dir.path().join(".sqwai").join("spill");
        std::fs::create_dir_all(&spill).unwrap();
        std::fs::write(spill.join("read-big.txt"), "spilled_marker_xyz\n").unwrap();
        let mut ctx = ToolCtx::new(dir.path());

        let sweep = grep(&mut ctx, "spilled_marker_xyz", None, None, 0);
        assert!(
            sweep.output.contains("no matches"),
            "a project-wide sweep must still not see the spill dir: {}",
            sweep.output
        );

        let named = grep(
            &mut ctx,
            "spilled_marker_xyz",
            Some(".sqwai/spill/read-big.txt"),
            None,
            0,
        );
        assert!(
            named.output.contains(":1: spilled_marker_xyz"),
            "the file the host pointed at must be greppable: {}",
            named.output
        );

        // and naming one file does not turn the spill dir into a listing
        let other = grep(
            &mut ctx,
            "spilled_marker_xyz",
            Some(".sqwai/spill"),
            None,
            0,
        );
        assert!(!other.ok, "the spill dir itself stays closed");
    }

    #[test]
    fn host_state_dir_is_canonical() {
        // the walk yields canonical paths while the root may sit behind a
        // symlink (macOS `/var` -> `/private/var`): a raw-root prefix check
        // would silently never fire
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::new(dir.path());
        assert_eq!(
            ctx.host_state_dir(),
            dir.path().canonicalize().unwrap().join(".sqwai")
        );
    }

    #[test]
    fn apply_one_crlf_matching() {
        // File has CRLF, replacement pattern has LF
        let crlf_content = "line1\r\nline2\r\nline3\r\n";
        let res = apply_one(crlf_content, "line2\n", "modified\n", false).unwrap();
        assert_eq!(res, "line1\r\nmodified\r\nline3\r\n");

        // Multi-line replacement
        let res_multi = apply_one(crlf_content, "line1\nline2\n", "replaced\n", false).unwrap();
        assert_eq!(res_multi, "replaced\r\nline3\r\n");

        // File has LF, replacement pattern has CRLF
        let lf_content = "line1\nline2\nline3\n";
        let res_lf = apply_one(lf_content, "line2\r\n", "modified\r\n", false).unwrap();
        assert_eq!(res_lf, "line1\nmodified\nline3\n");
    }

    #[test]
    fn apply_one_tolerates_trailing_whitespace_and_relative_indentation() {
        // 1. Trailing whitespace on lines in the file
        let content = "fn foo() {   \n    let x = 1;  \n}\n";
        let edited = apply_one(
            content,
            "fn foo() {\n    let x = 1;\n}",
            "fn foo() {\n    let x = 2;\n}",
            false,
        )
        .unwrap();
        assert_eq!(edited, "fn foo() {\n    let x = 2;\n}\n");

        // 2. Relative indentation: model omitted 4 spaces of outer indentation
        let indented = "impl S {\n    fn calc(&self) {\n        if true {\n            work();\n        }\n    }\n}\n";
        let edited_indent = apply_one(
            indented,
            "fn calc(&self) {\n    if true {\n        work();\n    }\n}",
            "fn calc(&self) {\n    if false {\n        work();\n    }\n}",
            false,
        )
        .unwrap();
        assert_eq!(
            edited_indent,
            "impl S {\n    fn calc(&self) {\n        if false {\n            work();\n        }\n    }\n}\n"
        );
    }

    #[test]
    fn apply_one_reports_already_applied_and_closest_lines_on_mismatch() {
        let content = "line 1\nfn compute_value(a: i32) -> i32 {\n    a + 42\n}\nline 5\n";
        let err_msg = apply_one(
            content,
            "fn compute_value(a: i32) -> i32 {\n    a + 1\n}",
            "fn compute_value(a: i32) -> i32 {\n    a + 42\n}",
            false,
        )
        .unwrap_err();
        assert!(
            err_msg.contains("old_string not found in file"),
            "{err_msg}"
        );
        assert!(
            err_msg.contains("new_string is already present"),
            "{err_msg}"
        );
        assert!(
            err_msg.contains("Closest lines in file"),
            "must show closest snippet with line numbers: {err_msg}"
        );
        assert!(err_msg.contains("a + 42"), "{err_msg}");
    }

    #[test]
    fn grep_200_match_cap_formats_and_spills_first_200_matches() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=250).map(|i| format!("hit_line_{i}\n")).collect();
        std::fs::write(dir.path().join("many_hits.txt"), body).unwrap();
        let mut ctx = ToolCtx::new(dir.path());

        let outcome = grep(&mut ctx, "hit_line_", None, None, 0);
        assert!(outcome.ok, "{}", outcome.output);
        assert!(
            outcome.output.contains("many_hits.txt:1: hit_line_1"),
            "first match must be present: {}",
            outcome.output
        );
        assert!(
            outcome.output.contains("many_hits.txt:200: hit_line_200"),
            "200th match must be formatted before truncation: {}",
            outcome.output
        );
        assert!(
            outcome.output.contains("first 200 shown"),
            "{}",
            outcome.output
        );
    }

    #[test]
    fn glob_and_grep_support_bare_filenames_path_includes_and_dot_config_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
        std::fs::write(
            dir.path().join(".github/workflows/ci.yml"),
            "name: CI_MARKER\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        std::fs::write(dir.path().join("src/nested/mod.rs"), "pub fn inner() {}\n").unwrap();
        let mut ctx = ToolCtx::new(dir.path());

        // Bare filename glob `*.rs` finds nested `src/nested/mod.rs`
        let g = glob(&mut ctx, "*.rs", None);
        assert!(g.ok);
        assert!(g.output.contains("src/nested/mod.rs"), "{}", g.output);

        // Dot-config `.github/workflows/ci.yml` is visible to glob and grep
        let g_ci = glob(&mut ctx, "ci.yml", None);
        assert!(g_ci.ok);
        assert!(
            g_ci.output.contains(".github/workflows/ci.yml"),
            "{}",
            g_ci.output
        );

        // Path-based include in grep matches relative path
        let gr = grep(&mut ctx, "CI_MARKER", None, Some(".github/**/*.yml"), 0);
        assert!(gr.ok);
        assert!(gr.output.contains("CI_MARKER"), "{}", gr.output);
    }

    #[test]
    fn read_supports_fused_paths_and_treesitter_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let src_a = "//! header\n\npub fn alpha() -> i32 {\n    10\n}\n\n/// Doc for beta\npub fn beta(x: i32) -> i32 {\n    x + 20\n}\n";
        let src_b = "pub struct Config {\n    pub port: u16,\n}\n";
        std::fs::write(dir.path().join("a.rs"), src_a).unwrap();
        std::fs::write(dir.path().join("b.rs"), src_b).unwrap();

        let mut ctx = ToolCtx::new(dir.path());

        // 1. Symbol-targeted read via tree-sitter extracts only `beta` (+ its doc comment).
        let sym_out = read(
            &mut ctx,
            "a.rs",
            &json!({"file_path": "a.rs", "symbols": ["beta"]}),
        );
        assert!(sym_out.ok, "{}", sym_out.output);
        assert!(
            sym_out
                .output
                .contains("// Symbol: pub fn beta(x: i32) -> i32")
        );
        assert!(sym_out.output.contains("Doc for beta"));
        assert!(sym_out.output.contains("x + 20"));
        assert!(!sym_out.output.contains("pub fn alpha"));

        // 2. Fused multi-file read (`paths: ["a.rs", "b.rs"]`) reads both files and marks them read.
        let mut ctx2 = ToolCtx::new(dir.path());
        let multi_out = read(&mut ctx2, "", &json!({"paths": ["a.rs", "b.rs"]}));
        assert!(multi_out.ok, "{}", multi_out.output);
        assert!(multi_out.output.contains("=== a.rs ==="));
        assert!(multi_out.output.contains("=== b.rs ==="));
        assert!(multi_out.output.contains("pub struct Config"));
        // Overwriting `b.rs` succeeds because fused `read` registered `b.rs` in `ctx2.reads`.
        let w = write_file(&mut ctx2, "b.rs", "pub struct Config {}\n");
        assert!(w.ok, "fused read must mark all paths as read: {}", w.output);

        // 3. Qualified `Type::method` disambiguation (`Bar::new` vs `Foo::new`).
        let multi_impl = "pub struct Foo;\nimpl Foo {\n    pub fn new() -> Self {\n        Foo\n    }\n}\n\npub struct Bar;\nimpl Bar {\n    pub fn new() -> Self {\n        Bar\n    }\n}\n";
        std::fs::write(dir.path().join("impls.rs"), multi_impl).unwrap();
        let qual_out = read(
            &mut ctx2,
            "impls.rs",
            &json!({"file_path": "impls.rs", "symbol": "Bar::new"}),
        );
        assert!(qual_out.ok, "{}", qual_out.output);
        assert!(qual_out.output.contains("Bar"), "{}", qual_out.output);
        assert!(
            !qual_out.output.contains("        Foo"),
            "must only return Bar::new, not Foo::new:\n{}",
            qual_out.output
        );

        // 4. TypeScript arrow-function components & hooks (`export const Button = ...`, `const useAuth = ...`).
        let tsx_src = "import { Props } from './types';\nexport const Button = (props: Props) => {\n    return <button>{props.label}</button>;\n};\n\nconst useAuth = () => {\n    return true;\n};\n";
        std::fs::write(dir.path().join("comp.tsx"), tsx_src).unwrap();
        std::fs::write(
            dir.path().join("app.tsx"),
            "import { Button } from './comp';\nexport const App = () => <Button />;\n",
        )
        .unwrap();
        let tsx_out = read(
            &mut ctx2,
            "comp.tsx",
            &json!({"file_path": "comp.tsx", "symbols": ["Button", "useAuth"]}),
        );
        assert!(tsx_out.ok, "{}", tsx_out.output);
        assert!(tsx_out.output.contains("export const Button"));
        assert!(tsx_out.output.contains("const useAuth"));

        // 5. `outline` reports both outgoing `Imports:` and incoming `Imported by:`.
        let out_res =
            crate::agent::tools::outline::outline(&mut ctx2, &json!({"path": "comp.tsx"}));
        assert!(out_res.ok, "{}", out_res.output);
        assert!(
            out_res.output.contains("Imports: './types'"),
            "{}",
            out_res.output
        );
        assert!(
            out_res.output.contains("Imported by (1): app.tsx:1"),
            "{}",
            out_res.output
        );
    }

    #[test]
    fn edit_handles_typographic_quotes_blank_boundaries_tabs_and_noop() {
        // 1. No-op edit (old == new) is rejected
        assert!(apply_one("hello\n", "hello\n", "hello\n", false).is_err());

        // 2. Typographic curly quotes / em-dash in source matched by ASCII old_string
        let curly = "let msg = “Don’t touch — active”;\n";
        let res = apply_one(
            curly,
            "let msg = \"Don't touch - active\";",
            "let msg = \"Updated\";",
            false,
        )
        .unwrap();
        assert_eq!(res, "let msg = \"Updated\";\n");

        // 3. Extra leading/trailing blank lines in old_string are stripped on fallback
        let src = "fn a() {}\nfn b() {\n    call();\n}\nfn c() {}\n";
        let res_blank = apply_one(
            src,
            "\nfn b() {\n    call();\n}\n\n",
            "\nfn b() {\n    call_new();\n}\n\n",
            false,
        )
        .unwrap();
        assert_eq!(
            res_blank,
            "fn a() {}\nfn b() {\n    call_new();\n}\nfn c() {}\n"
        );

        // 4. Tab-indented file matched when model sends 4-space indentation
        let tabbed = "class A {\n\tfn run() {\n\t\tstep();\n\t}\n}\n";
        let res_tabs = apply_one(
            tabbed,
            "fn run() {\n    step();\n}",
            "fn run() {\n    step_two();\n}",
            false,
        )
        .unwrap();
        assert_eq!(
            res_tabs,
            "class A {\n\tfn run() {\n\t\tstep_two();\n\t}\n}\n"
        );
    }

    #[test]
    fn grep_spills_beyond_200_and_supports_comma_separated_includes() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=260).map(|i| format!("hit_line_{i}\n")).collect();
        std::fs::write(dir.path().join("many_hits.rs"), body).unwrap();
        std::fs::write(dir.path().join("cfg.toml"), "hit_line_toml = 1\n").unwrap();
        let mut ctx = ToolCtx::new(dir.path());

        // Comma-separated include pattern matches both *.rs and *.toml
        let outcome = grep(
            &mut ctx,
            "hit_line_",
            Some("undefined"),
            Some("*.rs, *.toml"),
            0,
        );
        assert!(outcome.ok, "{}", outcome.output);
        assert!(
            outcome.output.contains("first 200 shown (261 total)"),
            "{}",
            outcome.output
        );

        // And the spill file actually contains match #255 (not just the first 200!)
        let spill_dir = dir.path().join(".sqwai").join("spill");
        let spill_file = std::fs::read_dir(&spill_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let spilled_text = std::fs::read_to_string(spill_file).unwrap();
        assert!(
            spilled_text.contains("hit_line_255"),
            "spill file must contain matches beyond 200"
        );
    }

    #[test]
    fn read_window_and_max_file_bytes_are_configurable() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (0..50).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.path().join("many.txt"), &body).unwrap();
        let mut ctx = ToolCtx::new(dir.path());
        ctx.tools.read.window = 5;
        let out = read(&mut ctx, "many.txt", &json!({}));
        assert!(out.ok, "{}", out.output);
        assert!(
            out.output.contains("showing lines 1–5 of 50"),
            "configured window must cap the read: {}",
            out.output
        );

        ctx.tools.read.max_file_bytes = 10;
        let out = read(&mut ctx, "many.txt", &json!({}));
        assert!(!out.ok);
        assert!(
            out.output.contains("[tools.read].max_file_bytes"),
            "{}",
            out.output
        );
    }
}
