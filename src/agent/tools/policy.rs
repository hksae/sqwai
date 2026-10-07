//! Dispatch guards and policy predicates: subagent scope, acceptance policy,
//! registries, small classifiers.
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
/// any. Best-effort: redirect
/// destinations plus operands of the classic mutating shapes (tee, cp/mv/
/// install/rsync/ln, dd's `of=`, truncate/mkdir/rmdir/rm/touch, in-place
/// sed, patch application). Only what a shape *writes* counts: a mover's
/// sources are reads (its destination is its last operand), a sed script is
/// not a file, `/dev/null` is a sink. An unresolvable target fails closed (it
/// is an escape or it never reaches the tree — either way not an in-scope
/// write). What slips past — implicit writes like a test run rebuilding
/// `target/`, `cd` games, writers outside the list (python, cargo) — is
/// documented, not fixed: refusing all bash for scoped children would brick
/// their legitimate test runs, and the journal plus the shadow copy see those
/// writes either way.
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
                    if operands.iter().any(|w| w.starts_with("-i")) {
                        // POSIX: the first non-flag operand is the script —
                        // with or without -e in front of it — so
                        // `sed -i s/a/b/ f.rs` writes f.rs alone. A `-f`
                        // script file is the exception: then every remaining
                        // operand is a file.
                        let script_from_file = operands.iter().any(|w| w.starts_with("-f"));
                        let files: &[&str] = if script_from_file {
                            &non_flag
                        } else {
                            non_flag.get(1..).unwrap_or(&[])
                        };
                        targets.extend(files.iter().map(|s| s.to_string()));
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
    // A null sink carries nothing into the tree: `2>/dev/null` is not a
    // write, and treating it as one refused every scoped child that ran it.
    targets.into_iter().filter(|t| !is_null_sink(t)).collect()
}

/// The discard destinations of both shells: `/dev/null` and Windows `NUL`.
fn is_null_sink(target: &str) -> bool {
    let flat = target.replace('\\', "/");
    flat.ends_with("/dev/null") || flat.eq_ignore_ascii_case("nul")
}

/// `> "dest"`, `> 'dest'` — matched on the original text.
fn redirect_quoted_re() -> regex::Regex {
    regex::Regex::new("(?:^|[\\s;&|])(?:\\d+)?>>?\\s*(?:\"([^\"]+)\"|'([^']+)')").unwrap()
}

/// `> dest` — matched on a copy with quoted spans blanked (see
/// [`blank_quoted`]), so quoted operators never count. The capture excludes
/// `>`: otherwise `>> 'file'` backtracks into matching the second arrow as
/// the destination and reports a phantom `>` target.
fn redirect_bare_re() -> regex::Regex {
    regex::Regex::new("(?:^|[\\s;&|])(?:\\d+)?>>?\\s*([^\\s;&|>]+)").unwrap()
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
    // Unattended acceptance has no dialog to answer, so there is no ack to
    // honor: the headless branch decides and the Confirm branch both refuse.
    match crate::agent::trust::trust_gate(command, tainted, true, &[]) {
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

#[cfg(test)]
mod tests {
    use super::bash_write_targets;

    fn targets(command: &str) -> Vec<String> {
        bash_write_targets(command)
    }

    /// A mover writes only its destination. Sources are reads, and reading
    /// outside the scope is legal, so `mv ../old.rs ./new.rs` must yield only
    /// the destination — otherwise a scoped child could not pull a file in.
    #[test]
    fn movers_count_only_the_destination() {
        for bin in ["cp", "mv", "install", "rsync", "ln"] {
            assert_eq!(
                targets(&format!("{bin} -f ../outside.rs src/inside.rs")),
                vec!["src/inside.rs".to_string()],
                "{bin} must contribute its last operand only"
            );
        }
        // many sources, one directory destination
        assert_eq!(
            targets("cp a.rs b.rs ../c.rs src/"),
            vec!["src/".to_string()]
        );
    }

    /// tee appends to every operand it names: all of them are written.
    #[test]
    fn tee_counts_every_operand() {
        assert_eq!(
            targets("echo hi | tee out.log ../err.log"),
            vec!["out.log".to_string(), "../err.log".to_string()]
        );
    }

    /// Redirects are the other writer: bare and quoted destinations both
    /// count, and a `>` narrated inside quotes counts as nothing.
    #[test]
    fn redirect_destinations_are_targets() {
        assert_eq!(targets("echo hi > out.txt"), vec!["out.txt".to_string()]);
        assert_eq!(
            targets("echo hi > 'my file.txt'"),
            vec!["my file.txt".to_string()]
        );
        // the append arrow is one operator: no phantom `>` target, and the
        // destination is still read
        assert_eq!(targets("echo hi >> err.log"), vec!["err.log".to_string()]);
        assert!(targets("echo \"write a > b\"").is_empty());
        // a null sink writes nothing into the tree
        assert!(targets("make test 2>/dev/null").is_empty());
        assert!(targets("make test > NUL").is_empty());
    }

    /// The wrappers the safety AST walk strips must be stripped here too:
    /// a desynced list let `nohup mv` escape the scope (audit H2).
    #[test]
    fn wrappers_do_not_hide_the_mover() {
        assert_eq!(
            targets("nohup mv a.rs ../b.rs"),
            vec!["../b.rs".to_string()]
        );
        assert_eq!(
            targets("FOO=1 timeout 10 cp a.rs ../b.rs"),
            vec!["../b.rs".to_string()]
        );
    }

    /// Writers outside this list (python, cargo, an unquoted heredoc) are
    /// not extracted: documented best-effort, the journal and shadow copy
    /// are what sees those writes.
    #[test]
    fn unlisted_writers_yield_nothing() {
        assert!(targets("python write.py").is_empty());
        assert!(targets("cargo build").is_empty());
    }

    /// The shapes with their own spelling: dd's `of=`, in-place sed, and
    /// `git apply` (whose path operands name the files it scatters over).
    #[test]
    fn single_shape_writers() {
        assert_eq!(targets("dd if=a of=b.img bs=1k"), vec!["b.img".to_string()]);
        assert_eq!(
            targets("sed -i s/old/new/ src/main.rs"),
            vec!["src/main.rs".to_string()]
        );
        // with -e every non-flag operand is a file
        assert_eq!(
            targets("sed -i -e s/old/new/ src/a.rs src/b.rs"),
            vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
        );
        assert!(targets("sed s/old/new/ src/main.rs").is_empty());
        assert_eq!(
            targets("git apply --check src/patch.diff"),
            vec!["src/patch.diff".to_string()]
        );
    }
}
