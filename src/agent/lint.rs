//! Write-path lint gates: scope guard (Z) and hardcode linter (AF).
//!
//! Both are warn-layer only: they append `warning:` lines to the tool
//! outcome the model already sees. Nothing here refuses a mutation — refs
//! are routinely incomplete (exploration writes outside them legitimately),
//! and a block gate would train the model to declare vacuous refs. What the
//! warnings buy is attribution pressure at the moment of the write, where
//! the finish-time misattribution check arrives too late to steer.
//!
//! Pure functions over caller-supplied data, so the heuristics are
//! unit-testable without a project tree.

use crate::plan::StepRef;

/// Long enough that nobody writes it by accident in a comparison.
const LONG_LITERAL: usize = 24;

/// Z: paths touched by a mutation that match none of the current step's
/// refs. Empty refs mean "no scope declared" — nothing to enforce, no
/// warning. Matching is the same suffix rule the finish-time
/// misattribution check uses: either side may be the longer path.
pub fn scope_warnings(refs: &[StepRef], touched: &[&str]) -> Vec<String> {
    if refs.is_empty() || touched.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for path in touched {
        let clean = clean_path(path);
        let covered = refs.iter().any(|r| {
            let want = clean_path(&r.path);
            clean == want
                || clean.ends_with(&format!("/{want}"))
                || want.ends_with(&format!("/{clean}"))
        });
        if !covered {
            out.push(format!(
                "warning: '{path}' is outside this step's refs ({} declared) — \
                 the work may belong to another step; declare it or move it",
                refs.len(),
            ));
        }
        if out.len() >= 3 {
            break;
        }
    }
    out
}

/// AF: added lines shaped like literals smuggled in to satisfy one specific
/// test input rather than the acceptance. Two signals: an outright
/// confession in the text, and a long string literal compared against or
/// returned (the T4 trap shape: branch on fixture bytes, return canned
/// output). Numeric magic constants are deliberately NOT flagged — ports,
/// timeouts and HTTP codes would drown the signal.
pub fn hardcode_warnings(diff: &str, path: &str) -> Vec<String> {
    const CONFESSIONS: &[&str] = &[
        "hardcode",
        "hard-code",
        "hard code",
        "cheat",
        "to pass the test",
        "satisfy the test",
        "test expects",
        "fool the test",
        "fake result",
        "stub for test",
        "stubbed for test",
    ];
    let mut out = Vec::new();
    for line in diff.lines() {
        let Some(added) = line.strip_prefix('+') else {
            continue;
        };
        if added.starts_with("++") {
            continue; // +++ header, not content
        }
        let lowered = added.to_lowercase();
        if CONFESSIONS.iter().any(|c| lowered.contains(c)) {
            out.push(format!(
                "warning: possible test-shaped literal in {path}: `{}` — \
                 acceptance must hold for unseen inputs, not this one",
                truncate(added.trim(), 120),
            ));
        } else if !is_comment(added) && has_long_compared_literal(added) {
            out.push(format!(
                "warning: possible test-shaped literal in {path}: `{}` — \
                 a long literal compared against or returned; make sure the \
                 acceptance holds for unseen inputs, not this one",
                truncate(added.trim(), 120),
            ));
        }
        if out.len() >= 3 {
            break;
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

fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//") || t.starts_with('#') || t.starts_with('*') || t.starts_with("<!--")
}

/// A `"..."` span of [`LONG_LITERAL`]+ chars on a line that compares
/// (`==`/`!=`) or returns. URL literals are exempt — long, but rarely a
/// test bypass.
fn has_long_compared_literal(line: &str) -> bool {
    if line.contains("http://") || line.contains("https://") {
        return false;
    }
    if !(line.contains("==") || line.contains("!=") || line.contains("return")) {
        return false;
    }
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let mut j = i + 1;
            let mut len = 0;
            while j < bytes.len() && bytes[j] != b'"' {
                if bytes[j] == b'\\' {
                    j += 1;
                }
                len += 1;
                j += 1;
            }
            if len >= LONG_LITERAL {
                return true;
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    false
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // byte slicing can split a char boundary (Cyrillic in pasted
    // fixtures); back off to one instead of panicking in a warn path.
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(paths: &[&str]) -> Vec<StepRef> {
        paths.iter().map(|p| StepRef::from(*p)).collect()
    }

    #[test]
    fn scope_gate_covers_declared_paths_both_directions() {
        let r = refs(&["src/agent/loop_task.rs"]);
        assert!(scope_warnings(&r, &["src/agent/loop_task.rs"]).is_empty());
        // worktree-absolute touched path against a repo-relative ref
        assert!(scope_warnings(&r, &["C:/Users/Asus/sqwai/src/agent/loop_task.rs"]).is_empty());
        let w = scope_warnings(&r, &["src/agent/other.rs"]);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("src/agent/other.rs") && w[0].contains("outside"));
    }

    #[test]
    fn scope_gate_silent_without_declared_scope() {
        assert!(scope_warnings(&[], &["anything.rs"]).is_empty());
        assert!(scope_warnings(&refs(&["a.rs"]), &[]).is_empty());
    }

    #[test]
    fn scope_gate_caps_at_three() {
        let w = scope_warnings(&refs(&["a.rs"]), &["b.rs", "c.rs", "d.rs", "e.rs", "f.rs"]);
        assert_eq!(w.len(), 3);
    }

    #[test]
    fn hardcode_gate_flags_compared_long_literal() {
        let diff = "--- a/x.rs\n+++ b/x.rs\n@@\n+    if input == \"the quick brown fixture bytes 0123456789\" {\n+        return 1;\n";
        let w = hardcode_warnings(diff, "x.rs");
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("x.rs") && w[0].contains("unseen inputs"));
    }

    #[test]
    fn hardcode_gate_flags_confession_even_in_comments() {
        let diff =
            "--- a/x.rs\n+++ b/x.rs\n@@\n+// hardcode for the acceptance test\n+    Ok(())\n";
        let w = hardcode_warnings(diff, "x.rs");
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn hardcode_gate_ignores_ordinary_code() {
        let diff = "--- a/x.rs\n+++ b/x.rs\n@@\n-    if x == 1 {\n+    if status == 200 {\n+        timeout_ms = 600000;\n+        let url = \"https://example.com/some/long/api/endpoint/path\";\n";
        assert!(hardcode_warnings(diff, "x.rs").is_empty());
    }

    #[test]
    fn hardcode_gate_skips_headers_and_removed_lines() {
        let diff = "--- a/old.rs\n+++ b/old.rs\n@@\n-    if input == \"the quick brown fixture bytes 0123456789\" {\n";
        assert!(hardcode_warnings(diff, "old.rs").is_empty());
    }

    #[test]
    fn hardcode_gate_caps_at_three() {
        let mut diff = String::from("--- a/x\n+++ b/x\n@@\n");
        for i in 0..6 {
            diff.push_str(&format!(
                "+    if a == \"long fixture literal number {i:0>24}\" {{\n"
            ));
        }
        assert_eq!(hardcode_warnings(&diff, "x").len(), 3);
    }
}
