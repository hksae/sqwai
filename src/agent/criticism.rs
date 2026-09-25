//! Grounding helpers for manual /verify: journal touches, artifact
//! matching, execution budgets.
//!
//! DROPPED: the H0 auto-detector (learned verdicts, weights,
//! Maybe-confirm) fired too imprecisely and nothing else used it — only
//! manual /verify drives the reflector pipeline now. The detector code,
//! its tests and its training assets are deleted; the design record of
//! the experiment lives in DESIGN §12.7.

use std::collections::HashMap;
use std::sync::OnceLock;


// --- Turn wiring: artifact signal for manual /verify -----------------

// (No verdict type remains: manual /verify asserts criticism by typing
// the command, so every Check it builds fires by construction.)

/// One named artifact the criticism points at, grounded in last-turn facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactFact {
    /// touched path as the journal labels it (e.g. `src/auth/login.rs`)
    pub path: String,
    pub added: Option<u64>,
    pub removed: Option<u64>,
    /// owning step at write time, if the writer attributed one
    pub step: Option<String>,
}

/// Everything manual /verify needs: grounding artifacts plus failures.
#[derive(Debug)]
pub struct Check {
    pub artifacts: Vec<ArtifactFact>,
    pub touched: Vec<ArtifactFact>,
    pub failures: Vec<String>,
    /// Manual /verify always fires by construction — the user asserted
    /// criticism by typing the command.
    pub fire: bool,
}

/// Budgets for the fact block: named artifacts, recent touches, failures.
const MAX_ARTIFACTS: usize = 5;
const MAX_FAILURES: usize = 3;
/// Symbol-resolution attempts per message (graph lookups are the only
/// non-trivial cost here).
const MAX_SYMBOLS: usize = 8;

/// Execution budgets per trigger level (H1 slice 3). `/verify` widens
/// the window on request; `--full` is the escalation and the
/// second-objection answer. The window rides here because it sizes
/// the check itself, not just the executor.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// journal records back for touches/failures
    pub window: usize,
    /// total executor tool calls
    pub calls: usize,
    /// executor wall clock, seconds
    pub wall_secs: u64,
}

impl Budget {
    pub fn verify() -> Self {
        Self {
            window: 200,
            calls: 48,
            wall_secs: 900,
        }
    }
    pub fn full() -> Self {
        Self {
            window: 400,
            calls: 96,
            wall_secs: 1200,
        }
    }
}


/// Recent touches (last write wins per path) plus recent tool failures.
/// Shared by the auto check and the manual `/verify` (which skips the
/// classifier: the user asserted criticism by typing the command).
pub fn gather(
    root: &std::path::Path,
    session: &str,
    window: usize,
) -> (Vec<ArtifactFact>, Vec<String>) {
    let mut touched: Vec<ArtifactFact> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let records = crate::agent::journal::Journal::records_for(root, session).unwrap_or_default();
    let window: Vec<_> = records.iter().rev().take(window).collect();
    // last write wins per path: the file's state at the criticism moment
    for record in window.iter().rev() {
        if record.kind != "file_diff" {
            continue;
        }
        let Some(path) = record.fields.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        if touched.iter().any(|t: &ArtifactFact| t.path == path) {
            continue;
        }
        touched.push(ArtifactFact {
            path: path.to_string(),
            added: record.fields.get("added").and_then(|v| v.as_u64()),
            removed: record.fields.get("removed").and_then(|v| v.as_u64()),
            step: record.step.clone(),
        });
    }
    if touched.is_empty() {
        return (touched, failures);
    }
    for record in window.iter().rev() {
        if record.kind != "tool_result"
            || record
                .fields
                .get("ok")
                .and_then(|v| v.as_bool())
                .unwrap_or(true)
            || record.fields.get("code").and_then(|v| v.as_str()) == Some("cancelled")
        {
            continue;
        }
        let tool = record
            .fields
            .get("tool")
            .and_then(|v| v.as_str())
            .unwrap_or("tool");
        let summary = record
            .fields
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let mut line = format!("{tool}: {}", truncate(summary, 100));
        if line.len() > 112 {
            line.truncate(112);
        }
        failures.push(line);
        if failures.len() >= MAX_FAILURES {
            break;
        }
    }
    (touched, failures)
}

/// Candidate names from the message matched against touched paths:
/// quoted spans and path-like tokens directly, identifier words via the
/// graph (symbol → file → touched). Paths first (free), symbols only
/// while nothing matched yet.
pub fn match_artifacts(
    root: &std::path::Path,
    text: &str,
    touched: &[ArtifactFact],
) -> Vec<ArtifactFact> {
    let mut out: Vec<ArtifactFact> = Vec::new();
    let mut candidates: Vec<String> = Vec::new();
    // quoted spans: "…", '…', `…`
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if matches!(c, '"' | '\'' | '`') {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != c {
                j += 1;
            }
            if j > i + 1 {
                candidates.push(chars[i + 1..j].iter().collect());
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    // path-like and identifier-like tokens
    for token in text
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '/' | '\\' | '.' | '_' | '-' | ':')))
    {
        let token = token.trim_matches(|c| matches!(c, '.' | ':' | '/' | '\\'));
        if token.len() >= 3 {
            candidates.push(token.to_string());
        }
    }
    // direct path hits first
    for candidate in &candidates {
        if out.len() >= MAX_ARTIFACTS {
            break;
        }
        let clean = candidate.replace('\\', "/");
        let hit = touched.iter().find(|t| {
            paths_match(&t.path, &clean)
                || clean
                    .split("::")
                    .next()
                    .is_some_and(|p| !p.is_empty() && paths_match(&t.path, p))
        });
        if let Some(hit) = hit
            && !out.iter().any(|a: &ArtifactFact| a.path == hit.path)
        {
            out.push(hit.clone());
        }
    }
    // then symbols, while the message still points at nothing
    if out.is_empty()
        && let Ok(mut store) = crate::agent::graph::SqliteGraphStore::open(root)
    {
        let mut tried = 0;
        for candidate in &candidates {
            if out.len() >= MAX_ARTIFACTS || tried >= MAX_SYMBOLS {
                break;
            }
            if candidate.contains('/') || candidate.contains('\\') || candidate.contains('.') {
                continue;
            }
            tried += 1;
            let resolved = store.resolve_ref(None, None, Some(candidate));
            let Ok(crate::agent::graph::ResolveRefResult::Found { key, .. }) = resolved else {
                continue;
            };
            let Some(path) = symbol_key_path(&key) else {
                continue;
            };
            if let Some(hit) = touched.iter().find(|t| paths_match(&t.path, &path))
                && !out.iter().any(|a: &ArtifactFact| a.path == hit.path)
            {
                out.push(hit.clone());
            }
        }
    }
    out
}

fn clean_path(raw: &str) -> String {
    raw.replace('\\', "/")
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn paths_match(a: &str, b: &str) -> bool {
    let a = clean_path(a);
    let b = clean_path(b);
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

/// Symbol keys look like `sym:src/lib.rs::fn::calculate` (files are
/// `file:src/lib.rs`): the path is the first `::` segment past the prefix.
fn symbol_key_path(key: &str) -> Option<String> {
    let rest = key
        .strip_prefix("file:")
        .or_else(|| key.strip_prefix("sym:"))
        .unwrap_or(key);
    let path = rest.split("::").next()?.trim();
    (!path.is_empty()).then(|| path.to_string())
}




