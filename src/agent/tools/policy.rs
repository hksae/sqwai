//! Dispatch guards and policy predicates: subagent scope, frozen check
//! inputs, acceptance policy, registries, small classifiers.
//!
//! Moved byte-for-byte from `tools/mod.rs`; behavior unchanged.
//!
use super::ToolCtx;
use super::git;
use crate::plan;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// True when the step a subagent was spawned for still exists in the named
/// plan at exactly the inherited epoch (§2.2.4). Anything else — reopened,
/// retired plan, deleted step — means the inherited context is stale.
pub(crate) fn step_epoch_current(root: &Path, inherited: &plan::StepContext) -> bool {
    let Some(plan) = plan::read_plan_file(root, &inherited.plan_id) else {
        return false;
    };
    plan.steps
        .iter()
        .find(|step| step.id == inherited.step_id)
        .is_some_and(|step| step.step_epoch == inherited.step_epoch)
}

/// Project-relative, lexically cleaned mutation targets of a file-writing
/// call: `file_path` for write/edit/multi_edit, `+++` files for patch.
/// Unresolvable or empty spellings yield nothing — the tool's own
/// validation reports those, not the scope gates.
pub(crate) fn mutation_target_paths(ctx: &ToolCtx, name: &str, args: &Value) -> Vec<String> {
    let raws: Vec<String> = if name == "patch" {
        let patch = args["patch"].as_str().unwrap_or_default();
        if patch.trim().is_empty() {
            return Vec::new();
        }
        git::extract_patch_files(ctx, patch)
    } else if name == "git_stage" {
        // `paths` (array or string) plus the singular `path` alias, the
        // same set git::stage stages. `all: true` has no bounded target
        // list — the gate refuses it separately instead of guessing.
        let mut paths = Vec::new();
        if let Some(arr) = args.get("paths").and_then(Value::as_array) {
            for v in arr {
                if let Some(s) = v.as_str().filter(|s| !s.trim().is_empty()) {
                    paths.push(s.to_string());
                }
            }
        } else if let Some(s) = args.get("paths").and_then(Value::as_str)
            && !s.trim().is_empty()
        {
            paths.push(s.to_string());
        }
        if let Some(s) = args.get("path").and_then(Value::as_str)
            && !s.trim().is_empty()
            && !paths.iter().any(|p| p == s.trim())
        {
            paths.push(s.trim().to_string());
        }
        paths
    } else {
        let raw = args["file_path"].as_str().unwrap_or_default();
        if raw.trim().is_empty() {
            return Vec::new();
        }
        vec![raw.to_string()]
    };
    raws.into_iter()
        .filter_map(|raw| {
            // scope decisions run in canonical space: the OS resolves
            // symlinks, the lexical spelling does not (audit H3)
            let rel = ctx.resolve_scope_rel(&raw).ok()?;
            Some(lexical_clean(&rel))
        })
        .collect()
}

/// First write target of a shell command outside the subagent scope, if
/// any. Best-effort like [`frozen_input_command_hit`]: redirect
/// destinations plus operands of the classic mutating shapes (tee, cp/mv/
/// install/rsync/ln, dd's `of=`, truncate/mkdir/rmdir/rm/touch, in-place
/// sed, patch application). An unresolvable target fails closed (it is an
/// escape or it never reaches the tree — either way not an in-scope
/// write). What slips past — implicit writes like a test run rebuilding
/// `target/`, `cd` games — is documented, not fixed: refusing all bash
/// for scoped children would brick their legitimate test runs.
pub(crate) fn bash_scope_hit(ctx: &ToolCtx, scope: &[String], command: &str) -> Option<String> {
    for target in bash_write_targets(command) {
        // canonical, not lexical: a symlink inside scope can point at a
        // sibling's directory, and the OS follows it (audit H3)
        match ctx.resolve_scope_rel(&target) {
            Ok(rel) => {
                let rel = lexical_clean(&rel);
                if !in_write_scope(&rel, scope) {
                    return Some(rel);
                }
            }
            Err(_) => return Some(target),
        }
    }
    None
}

/// `$name` / `${name}` references in acceptance texts: what the host
/// expanded from project config, for the provenance note. Pure scan —
/// expansion itself (and unknown-name rejection) lives in
/// `plan::substitute_verify_commands`; the name grammar mirrors it.
pub(crate) fn commanded_verify_refs(texts: &[String]) -> Vec<String> {
    let mut names = Vec::new();
    for text in texts {
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '$' {
                continue;
            }
            let braced = chars.peek() == Some(&'{');
            if braced {
                chars.next();
            }
            let mut name = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_alphanumeric() || d == '_' || d == '-' {
                    name.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            if braced {
                if chars.peek() == Some(&'}') {
                    chars.next();
                } else {
                    continue;
                }
            }
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// Raw write-target tokens of a shell command: redirect destinations and
/// mutating-shape operands. Quoted spans never contribute operators (an
/// `echo "a > b"` is not a redirect), but stay available as quoted
/// destinations (`> "my file"`).
fn bash_write_targets(command: &str) -> Vec<String> {
    let mut targets = Vec::new();
    // Quoted destinations come from the original text (`> "my file"`).
    for caps in redirect_quoted_re().captures_iter(command) {
        let dest = caps.get(1).or_else(|| caps.get(2)).map(|m| m.as_str());
        if let Some(dest) = dest.filter(|d| !d.starts_with('&') && !d.is_empty()) {
            targets.push(dest.to_string());
        }
    }
    // Bare destinations come from a copy with quoted spans blanked, so
    // `echo "a > b"` contributes nothing: the `>` sits inside quotes and
    // the separator class cannot match there.
    let blanked = blank_quoted(command);
    for caps in redirect_bare_re().captures_iter(&blanked) {
        if let Some(dest) = caps.get(1).map(|m| m.as_str())
            && !dest.starts_with('&')
            && !dest.is_empty()
        {
            targets.push(dest.to_string());
        }
    }
    // operands of mutating shapes, per command segment (quoting blanked so
    // a narrated "we need tee" is not a tee invocation)
    for segment in blanked.split([';', '\n']) {
        // pipelines run left to right; each stage is its own command
        for stage in segment.split('|') {
            let words: Vec<&str> = stage
                .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
                .filter(|w| !w.is_empty() && *w != "&")
                .collect();
            // binary is the first word past the transparent wrappers —
            // the SAME list the safety AST walk strips (audit H2: the
            // desynced copy here let `nohup mv` escape the scope) — and
            // past VAR= assignments and the wrapper's flags/numeric args
            let idx = crate::agent::safety::effective_command_index(&words);
            let binary = words.get(idx).map(|w| w.rsplit('/').next().unwrap_or(w));
            let operands: Vec<&str> = words[idx + 1..].to_vec();
            let non_flag: Vec<&str> = operands
                .iter()
                .filter(|w| !w.starts_with('-') && !w.contains('='))
                .copied()
                .collect();
            match binary {
                Some("tee") => targets.extend(non_flag.iter().map(|s| s.to_string())),
                Some("cp" | "mv" | "install" | "rsync" | "ln") => {
                    if let Some(last) = non_flag.last() {
                        targets.push(last.to_string());
                    }
                }
                Some("dd") => {
                    for w in operands {
                        if let Some(path) = w.strip_prefix("of=") {
                            targets.push(path.to_string());
                        }
                    }
                }
                Some("truncate" | "mkdir" | "rmdir" | "rm" | "touch") => {
                    targets.extend(non_flag.iter().map(|s| s.to_string()));
                }
                Some("sed") => {
                    if operands.iter().any(|w| *w == "-i" || w.starts_with("-i")) {
                        targets.extend(non_flag.iter().map(|s| s.to_string()));
                    }
                }
                Some("patch") => {
                    targets.extend(
                        non_flag
                            .iter()
                            .filter(|w| w.contains('/'))
                            .map(|s| s.to_string()),
                    );
                }
                Some("git") => {
                    // `git apply` scatters writes the tokens cannot
                    // enumerate; any path operand outside fails below
                    let mut ops = non_flag.iter().peekable();
                    if ops.peek() == Some(&&"apply") {
                        targets.extend(ops.filter(|w| w.contains('/')).map(|s| s.to_string()));
                    }
                }
                _ => {}
            }
        }
    }
    targets
}

/// `> "dest"`, `> 'dest'` — matched on the original text.
fn redirect_quoted_re() -> regex::Regex {
    regex::Regex::new("(?:^|[\\s;&|])(?:\\d+)?>>?\\s*(?:\"([^\"]+)\"|'([^']+)')").unwrap()
}

/// `> dest` — matched on a copy with quoted spans blanked (see
/// [`blank_quoted`]), so quoted operators never count.
fn redirect_bare_re() -> regex::Regex {
    regex::Regex::new("(?:^|[\\s;&|])(?:\\d+)?>>?\\s*([^\\s;&|]+)").unwrap()
}

/// The redirect regex above runs on the original for quoted destinations;
/// this blanks quoted spans for the operand scan.
fn blank_quoted(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut quote: Option<char> = None;
    for c in command.chars() {
        if let Some(q) = quote {
            out.push(' ');
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => {
                quote = Some(c);
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}
/// for the jail verdict, then compares lexically cleaned relative paths so
/// `..` spellings cannot dodge the freeze. Fail-open on an unreadable
/// store: the journal heals the plan, and an infra hiccup must not brick
/// writes.
pub(crate) fn frozen_input_hit(ctx: &ToolCtx, name: &str, args: &Value) -> Option<String> {
    let plan = plan::open_active_for_session(&ctx.root, Some(&ctx.session_id)).ok()??;
    let frozen = plan::frozen_input_paths(&plan);
    if frozen.is_empty() {
        return None;
    }
    mutation_target_paths(ctx, name, args)
        .into_iter()
        .find(|clean| frozen.iter().any(|f| f == clean))
}

/// Verdict on a write to a frozen check input: the gate predicts from the
/// call arguments, before anything touches disk.
pub(crate) enum FrozenVerdict {
    /// no frozen input targeted — proceed normally
    Clear,
    /// pure addition to the frozen baseline (new test, new fixture rows):
    /// proceed, the host re-freezes afterwards
    Additive { path: String },
    /// existing frozen lines would change: needs user approval. `preview`
    /// is the frozen-vs-proposed diff for the dialog.
    Modify { path: String, preview: String },
}

/// Predict whether a file-writing call extends frozen check inputs or
/// rewrites them. `write` carries its content; `edit`/`multi_edit` are
/// simulated against the current file bytes, so the verdict sees exactly
/// what would land. `patch` cannot be simulated — it always needs approval.
pub(crate) fn frozen_write_verdict(ctx: &ToolCtx, name: &str, args: &Value) -> FrozenVerdict {
    let Some(hit) = frozen_input_hit(ctx, name, args) else {
        return FrozenVerdict::Clear;
    };
    let plan = match plan::open_active_for_session(&ctx.root, Some(&ctx.session_id)) {
        Ok(Some(plan)) => plan,
        _ => {
            return FrozenVerdict::Modify {
                path: hit,
                preview: "plan unreadable — cannot prove the write is additive".into(),
            };
        }
    };
    let input = plan
        .acceptance
        .iter()
        .filter(|item| item.status != plan::AcceptanceStatus::Waived)
        .flat_map(|item| item.inputs.iter())
        .find(|input| input.path == hit);
    let Some(input) = input else {
        // raced with a waiver between hit and lookup: nothing frozen now
        return FrozenVerdict::Clear;
    };
    let proposed: Option<String> = match name {
        "write" => args["content"].as_str().map(str::to_string),
        "edit" => {
            let current = std::fs::read_to_string(ctx.root.join(&hit)).ok();
            current.and_then(|content| {
                super::fs::apply_one(
                    &content,
                    args["old_string"].as_str().unwrap_or_default(),
                    args["new_string"].as_str().unwrap_or_default(),
                    args["replace_all"].as_bool().unwrap_or(false),
                )
                .ok()
            })
        }
        "multi_edit" => {
            let Ok(current) = std::fs::read_to_string(ctx.root.join(&hit)) else {
                return FrozenVerdict::Modify {
                    path: hit,
                    preview: "file unreadable — cannot prove the write is additive".into(),
                };
            };
            let Some(edits) = args["edits"].as_array() else {
                return FrozenVerdict::Modify {
                    path: hit,
                    preview: "edits unreadable — cannot prove the write is additive".into(),
                };
            };
            let mut staged = current;
            let mut failed = false;
            for edit in edits {
                match super::fs::apply_one(
                    &staged,
                    edit["old_string"].as_str().unwrap_or_default(),
                    edit["new_string"].as_str().unwrap_or_default(),
                    edit["replace_all"].as_bool().unwrap_or(false),
                ) {
                    Ok(next) => staged = next,
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
            if failed {
                return FrozenVerdict::Modify {
                    path: hit,
                    preview: "edits do not apply — cannot prove the write is additive".into(),
                };
            }
            Some(staged)
        }
        _ => None,
    };
    let (Some(proposed), Some(frozen)) = (
        proposed,
        input.blob.as_deref().and_then(|id| {
            crate::agent::blobs::get(&ctx.root, id)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
        }),
    ) else {
        return FrozenVerdict::Modify {
            path: hit,
            preview: "no frozen copy to judge against — only the user can allow this".into(),
        };
    };
    match additive_only(&frozen, &proposed) {
        true => FrozenVerdict::Additive { path: hit },
        false => FrozenVerdict::Modify {
            path: hit,
            preview: compact_diff(&frozen, &proposed),
        },
    }
}

/// Pure-addition check over two texts (frozen baseline first): every
/// baseline line must survive in order. Extracted so tools can judge a
/// simulated post-edit text without touching the plan store.
pub(crate) fn additive_only(frozen: &str, new_content: &str) -> bool {
    let mut rest = frozen.lines().peekable();
    for line in new_content.lines() {
        if rest.peek() == Some(&line) {
            rest.next();
        }
    }
    rest.next().is_none()
}

/// First lines of a unified diff for approval dialogs: the user approves
/// an intent, not a wall of text.
fn compact_diff(before: &str, after: &str) -> String {
    const MAX_DIFF_LINES: usize = 40;
    let diff = super::fs::make_diff(before, after);
    let lines: Vec<&str> = diff.lines().collect();
    if lines.len() <= MAX_DIFF_LINES {
        return diff;
    }
    let mut out = lines[..MAX_DIFF_LINES].join("\n");
    out.push_str(&format!(
        "\n…({} more diff lines)",
        lines.len() - MAX_DIFF_LINES
    ));
    out
}
/// Lexically clean a relative path: drop `.`, resolve `..` against the
/// stack. No filesystem access — the jail verdict already came from
/// `resolve`.
pub(crate) fn lexical_clean(path: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if stack.last().is_some_and(|top| *top != "..") {
                    stack.pop();
                } else {
                    stack.push("..");
                }
            }
            _ => stack.push(comp),
        }
    }
    stack.join("/")
}

/// Frozen input a shell command would write, if any. Best-effort heuristic,
/// documented as such: a frozen path token plus a write shape (redirect,
/// `tee`, in-place `sed`, copy/move onto it, patch application). Reading a
/// frozen file never matches. What slips past still meets the receipt-time
/// hash comparison — this gate steers early, that one judges.
pub(crate) fn frozen_input_command_hit(
    root: &Path,
    session_id: &str,
    command: &str,
) -> Option<String> {
    let plan = plan::open_active_for_session(root, Some(session_id)).ok()??;
    let frozen = plan::frozen_input_paths(&plan);
    if frozen.is_empty() {
        return None;
    }
    // path-ish tokens, quotes stripped, lexically cleaned
    let tokens: Vec<String> = command
        .split(|c: char| {
            c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '$' | '`' | '\'' | '"')
        })
        .filter(|t| t.contains('/'))
        .map(|t| lexical_clean(t.trim_matches(|c| c == '\'' || c == '"')))
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let frozen_token = || {
        tokens
            .iter()
            .find(|t| frozen.iter().any(|f| f == *t))
            .cloned()
    };
    // `> file`, `>> file`, `2>file`: the token after a redirect operator
    // (`2>&1` merges streams — no file — so `>` before `&` never counts)
    let mut words = command.split_whitespace().peekable();
    while let Some(word) = words.next() {
        let op = word.trim_matches(|c| c == '\'' || c == '"');
        let redirect = op == ">"
            || op == ">>"
            || (op.ends_with('>') && op[..op.len() - 1].chars().all(|c| c.is_ascii_digit()));
        if !redirect {
            continue;
        }
        if let Some(dest) = words.peek() {
            let clean = lexical_clean(dest.trim_matches(|c| c == '\'' || c == '"'));
            if clean.starts_with('&') {
                continue;
            }
            if frozen.iter().any(|f| f == &clean) {
                return Some(frozen_input_reason(&clean));
            }
        }
    }
    // `tee` writes every file arg; `patch`/`git apply` scatter writes the
    // tokens cannot resolve — any frozen token in such a command asks first
    let lower = command.to_lowercase();
    if (lower.contains("tee") || lower.contains("git apply") || lower.contains("patch "))
        && let Some(hit) = frozen_token()
    {
        return Some(frozen_input_reason(&hit));
    }
    // in-place editors and copy/move: a frozen token beside the shape asks
    if ((lower.contains("sed") && lower.contains("-i"))
        || ["cp", "mv", "install", "rsync", "dd", "truncate"]
            .iter()
            .any(|w| lower.split_whitespace().any(|t| t == *w)))
        && let Some(hit) = frozen_token()
    {
        return Some(frozen_input_reason(&hit));
    }
    None
}

fn frozen_input_reason(path: &str) -> String {
    format!(
        "edits frozen check input '{path}': the test/fixture was hashed at plan time, approve to change the check itself"
    )
}

/// Writer scopes of live subagents, keyed by child session. Written at
/// spawn (after scope validation), taken once at child-context
/// construction — single take, so a crashed spawn cannot poison anything
/// later (session ids are unique per spawn).
fn subagent_scopes() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static SCOPES: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    SCOPES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a writer child's path scope. Called at spawn, after the
/// write flag and the sibling-overlap checks passed.
pub(crate) fn register_subagent_scope(session: &str, paths: Vec<String>) {
    subagent_scopes()
        .lock()
        .unwrap()
        .insert(session.to_string(), paths);
}

/// Take a registered scope for child-context construction.
pub(crate) fn take_subagent_scope(session: &str) -> Option<Vec<String>> {
    subagent_scopes().lock().unwrap().remove(session)
}

/// Canonical paths whose bytes the turn's opening message already carries
/// via @-mention injection, keyed by session. Written at submit (after
/// resolution), taken once at agent-context construction — single take,
/// so an abandoned submit (slash command, empty send) cannot poison a
/// later turn with stale paths.
fn mention_prereads() -> &'static Mutex<HashMap<String, Vec<PathBuf>>> {
    static PREREADS: OnceLock<Mutex<HashMap<String, Vec<PathBuf>>>> = OnceLock::new();
    PREREADS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register @-resolved paths for the coming turn.
pub(crate) fn register_mention_prereads(session: &str, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    mention_prereads()
        .lock()
        .unwrap()
        .insert(session.to_string(), paths);
}

/// Take registered @-paths for context construction (single take).
pub(crate) fn take_mention_prereads(session: &str) -> Vec<PathBuf> {
    mention_prereads()
        .lock()
        .unwrap()
        .remove(session)
        .unwrap_or_default()
}

/// True when `path` (project-relative, forward slashes) sits inside one
/// of the scope roots: equal, nested under a root, or the whole tree (`.`).
pub(crate) fn in_write_scope(path: &str, scope: &[String]) -> bool {
    scope
        .iter()
        .any(|root| path == root || path.starts_with(&format!("{root}/")) || root == ".")
}

/// Why an acceptance command must not run, beyond what the classifier
/// says at each call site. The classifier stays where it is (every runner
/// phrases its refusal differently); this carries the policy layers the
/// runners used to skip: the user's hard blocks, untaint-conditioned
/// exfil refusal, and the exfil trust gate. Acceptance runs unattended,
/// so anything but Safe refuses.
pub(crate) struct PolicyRefusal {
    pub code: &'static str,
    pub reason: String,
    pub hint: &'static str,
}

/// The full bash policy for an unattended acceptance command: the user's
/// `[safety].blocked_patterns` first (fail-closed on a bad regex, like the
/// bash tool), then exfil shapes (uploads, pushes — refused with or
/// without session taint, because no taint state makes an unattended
/// upload consenting), then the exfil trust gate (Deny and would-Confirm
/// both refuse — there is nobody to ask). Model-typed `cmd:` and
/// project-injected `cmd: $name` (`.sqwai/config.toml`, MEMORY.md) face
/// the same list either way: a `[verify]` plant that classifies Safe is
/// exactly what the egress rule stops.
pub(crate) fn acceptance_policy_hit(ctx: &ToolCtx, command: &str) -> Option<PolicyRefusal> {
    for pat in &ctx.blocked_patterns {
        match regex::Regex::new(pat) {
            Ok(re) => {
                if re.is_match(command) {
                    return Some(PolicyRefusal {
                        code: "blocked_command",
                        reason: format!("matches [safety].blocked_patterns '{pat}'"),
                        hint: "remove the pattern or rewrite the check",
                    });
                }
            }
            Err(e) => {
                return Some(PolicyRefusal {
                    code: "blocked_command",
                    reason: format!("invalid [safety].blocked_patterns regex '{pat}': {e}"),
                    hint: "fix the pattern in [safety].blocked_patterns",
                });
            }
        }
    }
    if let Some(kind) = crate::agent::safety::egress_kind(command) {
        return Some(PolicyRefusal {
            code: "unsafe_acceptance",
            reason: format!(
                "sends data outward ({kind}): unattended acceptance never runs exfiltration-shaped checks, tainted session or not"
            ),
            hint: "acceptance commands run without asking, so they must be safe; \
                   rewrite it or have the user waive the item",
        });
    }
    let tainted = ctx.external_taint();
    match crate::agent::trust::trust_gate(command, tainted, true) {
        crate::agent::trust::Gate::Allow => None,
        crate::agent::trust::Gate::Deny(reason) | crate::agent::trust::Gate::Confirm(reason) => {
            Some(PolicyRefusal {
                code: "unsafe_acceptance",
                reason,
                hint: "acceptance commands run without asking, so they must be safe; \
                       rewrite it or have the user waive the item",
            })
        }
    }
}

/// Pure half of the `forbid-cmd:` live gate: first pattern matching the
/// command (case-insensitive substring), if any. Heuristic like every
/// text match; the waiver covers overmatches.
pub(crate) fn forbidden_command(patterns: &[String], command: &str) -> Option<String> {
    let lower = command.to_lowercase();
    patterns
        .iter()
        .filter(|p| !p.trim().is_empty())
        .find(|p| lower.contains(&p.to_lowercase()))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn additive_only_accepts_appended_lines() {
        let frozen = "fn a() {}\nfn b() {}\n";
        assert!(additive_only(frozen, "fn a() {}\nfn b() {}\nfn c() {}\n"));
        // identical content is trivially additive
        assert!(additive_only(frozen, frozen));
        // prepended lines keep every frozen line in order
        assert!(additive_only(frozen, "// header\nfn a() {}\nfn b() {}\n"));
    }

    #[test]
    fn additive_only_rejects_rewrites_and_deletions() {
        let frozen = "fn a() {}\nfn b() {}\n";
        // modified oracle line
        assert!(!additive_only(
            frozen,
            "fn a() {}\nfn b() { assert!(true) }\n"
        ));
        // deleted line
        assert!(!additive_only(frozen, "fn a() {}\n"));
        // empty replacement
        assert!(!additive_only(frozen, ""));
    }

    #[test]
    fn additive_only_rejects_moved_blocks() {
        // same lines, different order: reads as delete+insert, correctly
        // non-additive — order can carry meaning (fixture rows, tests)
        let frozen = "one\ntwo\nthree\n";
        assert!(!additive_only(frozen, "three\ntwo\none\n"));
    }
}
