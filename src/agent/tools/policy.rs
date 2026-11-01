//! Dispatch guards and policy predicates: subagent scope, acceptance policy,
//! registries, small classifiers.
//!
//! Moved byte-for-byte from `tools/mod.rs`; behavior unchanged.
//!
use super::ToolCtx;
use crate::plan;
use std::path::Path;

/// True when the step a subagent was spawned for still exists in the named
/// plan at exactly the inherited epoch. Anything else — reopened,
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

/// The first write target of this command that lands outside the project
/// root AND outside every approved directory — the main agent's workspace
/// boundary, asked as an approval rather than refused. Targets are the
/// same tokens the subagent scope gate sees, null sinks filtered,
/// resolution shared with the write jail.
///
/// Segments are walked in order so a `cd` updates the directory the following
/// relative targets belong to: `cd /elsewhere && rm x` deletes
/// `/elsewhere/x`, and resolving `x` against the project root called that
/// deletion safe while the snapshot, scoped to the root, never held the file.
pub(crate) fn first_outside_target(ctx: &ToolCtx, command: &str) -> Option<String> {
    resolved_write_targets(command)
        .into_iter()
        .find(|target| ctx.outside_uncovered(target))
}

/// The first write target of this command that lands in the host's own data
/// directory: the event log, plans, checkpoints, sessions and media — the raw
/// material `/undo` restores from, and an audit trail the model must not
/// rewrite. Refused in every mode, exactly as `.sqwai` was while it held that
/// state; a project that itself lives under the data dir owns its own tree and
/// is exempt.
pub(crate) fn first_host_state_target(ctx: &ToolCtx, command: &str) -> Option<String> {
    let base = crate::agent::state::data_base()?;
    resolved_write_targets(command)
        .into_iter()
        .find(|target| ctx.resolves_under(target, &base))
}

/// The host-only key store named anywhere in a command, read or write: a
/// shell pipeline has no per-open hook, so the only mechanical check is the
/// path itself. Matched in the spellings a shell actually produces — the
/// absolute path in both separator styles and the `$APPDATA`/`%APPDATA%`
/// forms — never by basename, which could be an ordinary project file.
pub(crate) fn first_host_secret_target(command: &str) -> Option<String> {
    let keys = crate::config::keys_path().ok()?;
    let abs = keys.to_string_lossy().replace('\\', "/");
    let win = abs.replace('/', "\\");
    let mut forms = vec![abs.clone(), win];
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    if !appdata.is_empty() {
        let app = appdata.replace('\\', "/");
        if let Some(rest) = abs.strip_prefix(&app) {
            for prefix in ["%APPDATA%", "$APPDATA", "${APPDATA}", "$env:APPDATA"] {
                forms.push(format!("{prefix}{rest}"));
                forms.push(format!("{prefix}{}", rest.replace('/', "\\")));
            }
        }
    }
    let lower = command.to_ascii_lowercase();
    forms
        .iter()
        .find(|form| !form.is_empty() && lower.contains(&form.to_ascii_lowercase()))
        .cloned()
}

/// Write targets of a shell command in the order they run, each resolved
/// against the directory the command has changed into by that point.
///
/// Segments are walked in order so a `cd` updates the directory the following
/// relative targets belong to: `cd /elsewhere && rm x` deletes
/// `/elsewhere/x`, and resolving `x` against the project root called that
/// deletion safe while the snapshot, scoped to the root, never held the file.
fn resolved_write_targets(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut base: Option<String> = None;
    for segment in command.split([';', '\n', '&', '|']) {
        let mut words = segment.split_whitespace();
        let head = words
            .next()
            .unwrap_or("")
            .trim_matches(|c| c == '"' || c == '\'');
        if head == "cd" || head == "pushd" {
            if let Some(dir) = words.next() {
                base = Some(chdir_base(base.as_deref(), dir));
            }
            continue;
        }
        for target in bash_write_targets(segment) {
            if is_null_sink(&target) {
                continue;
            }
            let bare = target.trim().trim_matches(|c| c == '"' || c == '\'');
            if bare.is_empty() {
                continue;
            }
            out.push(if is_absolute_like(bare) {
                bare.to_string()
            } else {
                match &base {
                    Some(base) => format!("{base}/{bare}"),
                    None => bare.to_string(),
                }
            });
        }
    }
    out
}

/// The directory a `cd`/`pushd` argument names, given where the command
/// already was: an absolute spelling replaces the base, a relative one joins
/// it. Quote characters come off first — the shell never sees them.
fn chdir_base(base: Option<&str>, dir: &str) -> String {
    let dir = dir.trim_matches(|c| c == '"' || c == '\'');
    match base {
        Some(base) if !is_absolute_like(dir) => format!("{base}/{dir}"),
        _ => dir.to_string(),
    }
}

/// Absolute in either spelling the shells accept: a Windows path with a drive
/// or UNC root, or a POSIX `/c/...` path Git Bash passes through.
fn is_absolute_like(p: &str) -> bool {
    Path::new(p).is_absolute() || p.starts_with('/') || p.starts_with('\\')
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
    // operands of mutating shapes, per command segment. One quote-aware pass:
    // a quoted operand (`rm "C:\path with spaces"`) is a real target, while an
    // operator inside quotes (`echo "a > b"`) stays text because the tokenizer
    // never splits there. `&` and `|` split too: `cd x && rm y` and
    // `cp a b | tee c` are chains of commands, and a chain read as one segment
    // hides every write past its first word.
    for words in shell_segments(command) {
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
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

/// Split a command into segments at unquoted `;`, `&`, `|` and newlines, and
/// each segment into words with quotes grouped and removed.
///
/// One pass, so a quoted operand keeps its spaces (`rm "C:\my file"` is one
/// target) and an operator inside quotes stays text (`echo "a > b"` writes
/// nothing). Parentheses end a word, matching the word split the shape scan
/// has always used.
fn shell_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let (mut in_single, mut in_double) = (false, false);
    let mut chars = command.chars().peekable();
    let end_word = |cur: &mut String, started: &mut bool, words: &mut Vec<String>| {
        if *started {
            words.push(std::mem::take(cur));
            *started = false;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            // Outside quotes a backslash escapes the next byte. Inside double
            // quotes it is literal here: Windows paths arrive quoted
            // (`rm "C:\my file"`), and eating the separators would turn a real
            // target into a path that does not exist.
            '\\' if !in_single && !in_double => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                    started = true;
                }
            }
            '\'' if !in_double => {
                in_single = !in_single;
                started = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                started = true;
            }
            c if !in_single && !in_double && matches!(c, ';' | '&' | '|' | '\n') => {
                end_word(&mut cur, &mut started, &mut words);
                if !words.is_empty() {
                    segments.push(std::mem::take(&mut words));
                }
            }
            c if !in_single && !in_double && (c.is_whitespace() || c == '(' || c == ')') => {
                end_word(&mut cur, &mut started, &mut words);
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    end_word(&mut cur, &mut started, &mut words);
    if !words.is_empty() {
        segments.push(words);
    }
    segments
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
