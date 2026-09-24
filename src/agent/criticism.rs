//! H0 criticism detector: pure-Rust inference for the learned student.
//!
//! PARKED: auto-detection fires too imprecisely, so nothing calls this
//! automatically anymore (see `auto_reflector_enabled` in loop_task — the
//! auto path runs only with SQWAI_AUTO_REFLECTOR=1). The detector and the
//! weights stay for manual /verify and future experiments.
//!
//! The model is a logistic regression on hashed char-trigrams, trained
//! offline by `bench/criticism/train.py` from LLM-labeled examples.
//! Weights ship as `criticism_weights.json` (sparse, versioned) and are
//! embedded at compile time — inference is microseconds, $0, offline.
//!
//! Normalization, trigram windows and the FNV-1a hash below MUST stay
//! byte-identical to train.py (each mirrored spot is marked). Change one
//! side, change both, retrain, re-embed.
//!
//! Output is a three-way verdict per user message: `Fire` (confident
//! criticism), `Maybe` (gray zone — the strict trigger and the artifact
//! signal decide at the call site), `Silent`. Thresholds ride with the
//! weights file so a retrain can move them without touching this code.

use std::collections::HashMap;
use std::sync::OnceLock;

const WEIGHTS_JSON: &str = include_str!("criticism_weights.json");

/// Per-message verdict. `Maybe` is not indecision to hide — it is the
/// documented handoff to the strict trigger (fire needs ≥2 signal groups
/// plus prior-turn mutations; the artifact signal breaks the tie).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Fire,
    Maybe,
    Silent,
}

struct Model {
    dim: u64,
    bias: f64,
    weights: HashMap<u64, f64>,
    threshold_fire: f64,
    threshold_maybe: f64,
}

fn model() -> &'static Model {
    static MODEL: OnceLock<Model> = OnceLock::new();
    MODEL.get_or_init(|| {
        let v: serde_json::Value =
            serde_json::from_str(WEIGHTS_JSON).expect("criticism_weights.json parses");
        let weights = v["weights"]
            .as_object()
            .expect("weights is a map")
            .iter()
            .map(|(k, val)| {
                (
                    k.parse::<u64>().expect("weight key is an index"),
                    val.as_f64().expect("weight is a number"),
                )
            })
            .collect();
        Model {
            dim: v["dim"].as_u64().expect("dim"),
            bias: v["bias"].as_f64().expect("bias"),
            weights,
            threshold_fire: v["threshold_fire"].as_f64().expect("threshold_fire"),
            threshold_maybe: v["threshold_maybe"].as_f64().expect("threshold_maybe"),
        }
    })
}

/// Lowercase (full Unicode mapping, like Python's str.lower), ё→е, and
/// the elongation cap: runs longer than 2 collapse to 2.
/// MIRRORED in train.py::normalize.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run_char: Option<char> = None;
    let mut run_len = 0usize;
    for ch in text.chars().flat_map(|c| c.to_lowercase()) {
        let ch = if ch == 'ё' { 'е' } else { ch };
        if Some(ch) == run_char {
            run_len += 1;
        } else {
            run_char = Some(ch);
            run_len = 1;
        }
        if run_len <= 2 {
            out.push(ch);
        }
    }
    out
}

fn fnv1a64(data: &[u8]) -> u64 {
    // MIRRORED in train.py::fnv1a64
    let mut h: u64 = 14695981039346656037;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// Raw criticism score in [0, 1]. Deterministic for a fixed weights file.
pub fn score(text: &str) -> f64 {
    let m = model();
    let norm = normalize(text);
    let padded = format!(" {norm} ");
    let chars: Vec<char> = padded.chars().collect();
    let mut sum = m.bias;
    for window in chars.windows(3) {
        let tri: String = window.iter().collect();
        let idx = fnv1a64(tri.as_bytes()) % m.dim;
        if let Some(w) = m.weights.get(&idx) {
            sum += w;
        }
    }
    1.0 / (1.0 + (-sum).exp())
}

/// Three-way verdict using the weights file's own thresholds.
pub fn classify(text: &str) -> Verdict {
    let m = model();
    let p = score(text);
    if p >= m.threshold_fire {
        Verdict::Fire
    } else if p >= m.threshold_maybe {
        Verdict::Maybe
    } else {
        Verdict::Silent
    }
}

// --- Turn wiring: artifact signal, strict trigger, fact block ---------

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

/// Everything the turn hook needs: verdict, grounding, strict decision.
#[derive(Debug)]
pub struct Check {
    pub verdict: Verdict,
    pub score: f64,
    pub artifacts: Vec<ArtifactFact>,
    pub touched: Vec<ArtifactFact>,
    pub failures: Vec<String>,
    /// Strict trigger: Fire plus prior-turn mutations, or Maybe carried
    /// by a resolved artifact plus mutations. Silent never fires, and
    /// nothing fires when the last turn touched nothing — there is
    /// nothing to check the criticism against.
    pub fire: bool,
}

/// How far back "the last turn" reaches in journal records. A turn is a
/// handful of tool calls; the auto window comfortably covers one and
/// rarely two. Wider windows belong to `/verify` (budgets below).
pub const AUTO_WINDOW: usize = 80;
/// Budgets for the fact block: named artifacts, recent touches, failures.
const MAX_ARTIFACTS: usize = 5;
const MAX_TOUCHED: usize = 5;
const MAX_FAILURES: usize = 3;
/// Symbol-resolution attempts per message (graph lookups are the only
/// non-trivial cost here, and only on Fire/Maybe).
const MAX_SYMBOLS: usize = 8;

/// Execution budgets per trigger level (H1 slice 3). Auto is the in-turn
/// pass; `/verify` widens it on request; `--full` is the escalation and
/// the second-objection answer. The window rides here because it sizes
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
    pub fn auto() -> Self {
        Self {
            window: AUTO_WINDOW,
            calls: 24,
            wall_secs: 600,
        }
    }
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

/// Same, with an explicit journal window (`/verify` widens it).
pub fn check_with_window(
    root: &std::path::Path,
    session: &str,
    text: &str,
    window: usize,
) -> Check {
    let verdict = classify(text);
    let score = score(text);
    let mut out = Check {
        verdict,
        score,
        artifacts: Vec::new(),
        touched: Vec::new(),
        failures: Vec::new(),
        fire: false,
    };
    if verdict == Verdict::Silent {
        return out;
    }
    let (touched, failures) = gather(root, session, window);
    out.touched = touched;
    out.failures = failures;
    if out.touched.is_empty() {
        return out;
    }
    out.artifacts = match_artifacts(root, text, &out.touched);
    out.fire = verdict == Verdict::Fire || (verdict == Verdict::Maybe && !out.artifacts.is_empty());
    out
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

/// The volatile block-D part. `None` when the strict trigger held back —
/// the caller pushes nothing and writes no marker.
pub fn block_text(check: &Check, quote: &str) -> Option<String> {
    if !check.fire {
        return None;
    }
    let mut out = String::from("<criticism-check>\nThe user criticizes prior work (\"");
    out.push_str(&truncate(quote.trim(), 120));
    out.push_str(
        "\"). Answer from these host facts; check with a tool before asserting anything missing:\n",
    );
    if check.artifacts.is_empty() {
        out.push_str("touched last turn:\n");
        for fact in check.touched.iter().take(MAX_TOUCHED) {
            out.push_str(&format!("- {}\n", describe(fact)));
        }
    } else {
        for fact in &check.artifacts {
            out.push_str(&format!("- {}\n", describe(fact)));
        }
    }
    for failure in &check.failures {
        out.push_str(&format!("recent failure: {failure}\n"));
    }
    out.push_str("</criticism-check>");
    Some(out)
}

fn describe(fact: &ArtifactFact) -> String {
    let mut s = fact.path.clone();
    match (fact.added, fact.removed) {
        (Some(a), Some(r)) => s.push_str(&format!(" (+{a}/-{r})")),
        (Some(a), None) => s.push_str(&format!(" (+{a})")),
        _ => {}
    }
    if let Some(step) = fact.step.as_deref() {
        s.push_str(&format!(" [step {step}]"));
    }
    s
}

/// Journal marker fields for a fired check. The `text` value is screened
/// for secrets by the journal append itself.
pub fn marker_fields(check: &Check, text: &str) -> serde_json::Value {
    serde_json::json!({
        "text": text,
        "verdict": match check.verdict {
            Verdict::Fire => "fire",
            Verdict::Maybe => "maybe",
            Verdict::Silent => "silent",
        },
        "score": (check.score * 10000.0).round() / 10000.0,
        "artifacts": check.artifacts.iter().map(|a| &a.path).collect::<Vec<_>>(),
        "fired": check.fire,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strong_criticism_fires_both_languages() {
        for text in [
            "ты сломал сборку",
            "ничего не работает после тебя",
            "ты всё испортил",
            "you broke the build",
            "nothing works after your change",
            "you broke auth again",
        ] {
            assert_eq!(classify(text), Verdict::Fire, "{text}");
        }
    }

    #[test]
    fn typo_tolerance_survives_single_typos() {
        for text in [
            "ты сламал сборку",
            "не рабоатет",
            "you broke teh build",
            "it doesnt work",
        ] {
            assert_ne!(classify(text), Verdict::Silent, "{text}");
        }
    }

    #[test]
    fn requests_praise_and_chatter_stay_silent() {
        for text in [
            "сделай вот так",
            "ты можешь проверить?",
            "покажи дифф",
            "спасибо, работает",
            "что дальше делаем",
            "can you refactor this?",
            "thanks, nice work",
            "show me the diff",
            " ты не поверишь, но всё завелось ",
        ] {
            assert_eq!(classify(text), Verdict::Silent, "{text}");
        }
    }

    #[test]
    fn normalization_mirrors_training() {
        // ё folds, elongation caps at 2, case folds
        assert_eq!(normalize("СЛОМАААЛ"), normalize("сломаал"));
        assert_eq!(normalize("её"), "ее");
        assert!(normalize("ТЫ СЛОМАЛ").contains("ты сломал"));
    }

    #[test]
    fn mirror_matches_python_bit_for_bit() {
        // pinned against bench/criticism/train.py output:
        // normalize('Ты СЛОМАААЛ ёж') == 'ты сломаал еж'
        assert_eq!(normalize("Ты СЛОМАААЛ ёж"), "ты сломаал еж");
        // fnv1a64('сло') == 0x6fa517ad5f825a50
        assert_eq!(fnv1a64("сло".as_bytes()), 0x6fa517ad5f825a50);
        // full-train scores: fire vs silent with margin
        assert!(score("ты сломал сборку") > 0.99);
        assert!(score("can you refactor this?") < 0.01);
    }

    #[test]
    fn scoring_is_deterministic() {
        let text = "ты сломал сборку опять";
        assert_eq!(score(text).to_bits(), score(text).to_bits());
    }

    fn journal_with_diff(dir: &std::path::Path, session: &str, path: &str) {
        let mut journal =
            crate::agent::journal::Journal::open(dir, session).expect("journal opens");
        journal.set_attribution(Some("2".into()), Some("plan".into()), "main");
        journal
            .append(
                "file_diff",
                serde_json::json!({"path": path, "added": 3, "removed": 1}),
            )
            .expect("file_diff appends");
    }

    #[test]
    fn check_fires_on_named_touched_file() {
        let dir = std::env::temp_dir().join(format!("sqwai-critic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        journal_with_diff(&dir, "sess", "src/a.rs");
        let check = check_with_window(&dir, "sess", "ты сломал src/a.rs", Budget::auto().window);
        assert!(check.fire, "{check:?}");
        assert_eq!(check.artifacts.len(), 1);
        assert_eq!(check.artifacts[0].path, "src/a.rs");
        assert_eq!(check.artifacts[0].step.as_deref(), Some("2"));
        let block = block_text(&check, "ты сломал src/a.rs").expect("block");
        assert!(block.contains("src/a.rs"), "{block}");
        assert!(block.contains("check with a tool"), "{block}");
        assert!(block.contains("[step 2]"), "{block}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn check_holds_back_without_mutations() {
        let dir = std::env::temp_dir().join(format!("sqwai-critic-nomut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _journal = crate::agent::journal::Journal::open(&dir, "sess").expect("journal opens");
        let check = check_with_window(&dir, "sess", "ты всё сломал", Budget::auto().window);
        assert_eq!(check.verdict, Verdict::Fire);
        assert!(!check.fire, "nothing to check against");
        assert!(block_text(&check, "ты всё сломал").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn check_maybe_carried_by_artifact_only() {
        let dir = std::env::temp_dir().join(format!("sqwai-critic-maybe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        journal_with_diff(&dir, "sess", "src/parser.py");
        let carried = check_with_window(
            &dir,
            "sess",
            "в parser.py теперь исключение",
            Budget::auto().window,
        );
        assert_eq!(carried.verdict, Verdict::Maybe);
        assert!(carried.fire, "{carried:?}");
        assert_eq!(carried.artifacts.len(), 1);

        let dir2 = std::env::temp_dir().join(format!("sqwai-critic-maybe2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir2);
        std::fs::create_dir_all(&dir2).unwrap();
        journal_with_diff(&dir2, "sess", "src/other.py");
        let dropped = check_with_window(
            &dir2,
            "sess",
            "в parser.py теперь исключение",
            Budget::auto().window,
        );
        assert!(!dropped.fire, "maybe without a name stays out");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&dir2).ok();
    }

    #[test]
    fn check_silent_short_circuits_despite_mutations() {
        let dir = std::env::temp_dir().join(format!("sqwai-critic-sil-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        journal_with_diff(&dir, "sess", "src/a.rs");
        let check = check_with_window(&dir, "sess", "сделай вот так", Budget::auto().window);
        assert_eq!(check.verdict, Verdict::Silent);
        assert!(!check.fire);
        assert!(check.artifacts.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn check_resolves_symbol_to_touched_file() {
        let dir = std::env::temp_dir().join(format!("sqwai-critic-sym-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "pub fn calculate(x: i32) -> i32 {\n    x + 1\n}\n",
        )
        .unwrap();
        let mut store = crate::agent::graph::SqliteGraphStore::open(&dir).expect("graph opens");
        crate::agent::graph_index::index_project(&mut store, &dir).expect("indexed");
        journal_with_diff(&dir, "sess", "src/lib.rs");
        let check = check_with_window(&dir, "sess", "ты сломал calculate", Budget::auto().window);
        assert!(check.fire, "{check:?}");
        assert!(
            check.artifacts.iter().any(|a| a.path == "src/lib.rs"),
            "{check:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn marker_fields_carry_verdict_and_names() {
        let dir = std::env::temp_dir().join(format!("sqwai-critic-mk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        journal_with_diff(&dir, "sess", "src/a.rs");
        let check = check_with_window(&dir, "sess", "ты сломал src/a.rs", Budget::auto().window);
        let marker = marker_fields(&check, "ты сломал src/a.rs");
        assert_eq!(marker["verdict"], serde_json::json!("fire"));
        assert_eq!(marker["fired"], serde_json::json!(true));
        assert_eq!(marker["text"], serde_json::json!("ты сломал src/a.rs"));
        assert!(marker["score"].as_f64().unwrap() > 0.9);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Dev probe, not an assertion test: scores arbitrary phrases so a
    /// human can spot-check the detector without wiring the turn loop.
    /// Usage (PowerShell):
    ///   $env:SQWAI_CRITIC_PROBE = "ты сломал всё|сделай вот так|you broke it";
    ///   cargo test critic_probe -- --nocapture
    /// Unset → no-op pass.
    #[test]
    fn critic_probe_prints_scores() {
        let Ok(raw) = std::env::var("SQWAI_CRITIC_PROBE") else {
            return;
        };
        for text in raw.split('|').map(str::trim).filter(|t| !t.is_empty()) {
            println!("p={:.4} {:?}  {text}", score(text), classify(text));
        }
    }
}
