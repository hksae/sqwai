//! Two-layer safety classification for shell commands.
//!
//! Layer 1: fast heuristic (substring/regex-based).
//! Layer 2: AST structural analysis via tree-sitter-bash (pipeline into
//! interpreter, command_substitution with dangerous heads, destructive
//! redirections, find -delete/-exec, sudo/prefix escalation).
//!
//! The detector never "allows by default" anything it matches — matches go to
//! the user for approval. Everything else runs immediately per the design.

use crate::agent::shell::ShellKind;
use tree_sitter::{Node, Parser, Tree};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// run without asking
    Safe,
    /// show the command to the user first
    NeedsApproval(&'static str),
    /// refuse without asking — host-owned state or forbidden operation
    Blocked(&'static str),
}

/// classify a command using the syntax of the shell that will execute it.
#[cfg(test)] // the detect-the-shell shorthand; every product call passes its known ShellKind
pub fn classify(cmd: &str) -> Verdict {
    classify_for(ShellKind::detect(), cmd)
}

/// Outward-data commands for the trust gate: network upload and
/// transports out of the machine. Returns a short label for the approval
/// reason. Deliberately narrow — unknown exfiltration shapes stay the
/// safety classifier's job; this answers only "does this command send
/// data outward". Token-scanned (like `check_protected_path`) so
/// `somecurl` or an echoed string never matches a binary name.
pub fn egress_kind(cmd: &str) -> Option<&'static str> {
    // Shells glue `"cu"rl` back into `curl` before executing, so strip
    // quotes before tokenizing — otherwise fragmented binaries dodge every
    // shape below (same dodge the .sqwai gate used to allow).
    let dequoted: String = cmd
        .chars()
        .filter(|c| *c != '"' && *c != '\'' && *c != '`')
        .collect();
    // Binary names match case-insensitively; flags stay case-sensitive
    // (`-T` uploads, `-t` does not — lowercasing first would conflate them).
    fn split(s: &str) -> Vec<&str> {
        s.split(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    ';' | '|' | '&' | '>' | '<' | '`' | '(' | ')' | '"' | '\'' | '='
                )
        })
        .map(|t| t.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '(' | ')' | '[' | ']')))
        .filter(|t| !t.is_empty())
        .collect()
    }
    let raw: Vec<&str> = split(&dequoted);
    let tokens: Vec<String> = raw.iter().map(|t| t.to_lowercase()).collect();
    let has = |name: &str| {
        tokens
            .iter()
            .any(|t| t == name || t.ends_with(&format!("/{name}")))
    };
    // git push: the subcommand follows the binary, but flags (with values)
    // may sit between them (`git -C repo push`). Skip flags and their
    // values; the first bare token decides. (Narrated "echo git push" is
    // not a push: the head must be the git binary itself. `sudo git push`
    // stays the classifier's problem, not this gate's.)
    if tokens
        .first()
        .is_some_and(|t| t == "git" || t.ends_with("/git"))
    {
        // flags taking a separate value (matched lowercase: tokens are
        // already folded; single-token `--opt=value` forms skip as flags
        // without consuming the next token). Residual: `-c a.b=c push`
        // splits the value across tokens, so the tail reads as the
        // subcommand position and misses — exotic enough to document
        // rather than chase with a grammar.
        const VALUE_FLAGS: &[&str] = &[
            "-c",
            "--git-dir",
            "--work-tree",
            "--namespace",
            "--super-prefix",
        ];
        let mut rest = tokens.iter().skip(1).peekable();
        while let Some(token) = rest.next() {
            if token.starts_with('-') {
                if VALUE_FLAGS.contains(&token.as_str()) {
                    rest.next();
                }
                continue;
            }
            if token == "push" {
                return Some("git push");
            }
            break;
        }
    }
    // upload transports, any use
    for (binary, label) in [
        ("ssh", "remote transport"),
        ("scp", "remote transport"),
        ("sftp", "remote transport"),
        ("ftp", "remote transport"),
        ("rsync", "remote transport"),
    ] {
        if has(binary) {
            return Some(label);
        }
    }
    // downloaders only count when they send: upload/POST flags.
    // `-d` is curl's data flag but wget's debug flag — scoped per binary.
    let curl = has("curl");
    let wget = has("wget");
    if curl || wget {
        const UPLOAD_FLAGS: &[&str] = &[
            "--data",
            "--data-binary",
            "--data-raw",
            "--data-ascii",
            "--upload",
            "--post-data",
            "--post-file",
            "-T",
            "-F",
        ];
        if raw.iter().any(|t| UPLOAD_FLAGS.contains(t)) {
            return Some("network upload");
        }
        if curl && raw.contains(&"-d") {
            return Some("network upload");
        }
        for (i, token) in tokens.iter().enumerate() {
            if (*token == "-x" || *token == "--request" || *token == "--method")
                && tokens
                    .get(i + 1)
                    .is_some_and(|v| matches!(v.as_str(), "post" | "put" | "patch" | "delete"))
            {
                return Some("network upload");
            }
        }
    }
    // PowerShell web cmdlets (full names and aliases): bare GETs are
    // downloads, but -Method POST/PUT/PATCH/DELETE, -Body or -InFile send
    // data outward — the same upload shape as curl above.
    if ["invoke-webrequest", "invoke-restmethod", "iwr", "irm"]
        .iter()
        .any(|name| has(name))
    {
        if tokens.iter().any(|t| t == "-body" || t == "-infile") {
            return Some("network upload");
        }
        for (i, token) in tokens.iter().enumerate() {
            if token == "-method"
                && tokens
                    .get(i + 1)
                    .is_some_and(|v| matches!(v.as_str(), "post" | "put" | "patch" | "delete"))
            {
                return Some("network upload");
            }
        }
    }
    None
}

pub fn classify_for(shell: ShellKind, cmd: &str) -> Verdict {
    // 0. Protected host-owned state (.sqwai/) hard block, or approval when
    // a variable expansion makes the real path unknowable (audit C1)
    match check_protected_path(cmd) {
        Verdict::Blocked(reason) => return Verdict::Blocked(reason),
        Verdict::NeedsApproval(reason) => return Verdict::NeedsApproval(reason),
        Verdict::Safe => {}
    }
    // Layer 1: shell-specific heuristic (substring/regex)
    if let Verdict::NeedsApproval(reason) = heuristic_classify(shell, cmd) {
        return Verdict::NeedsApproval(reason);
    }
    // Bash AST is only valid for Bash-compatible syntax.
    match shell {
        ShellKind::Bash | ShellKind::Sh => ast_classify(cmd),
        ShellKind::Cmd | ShellKind::PowerShell => Verdict::Safe,
    }
}

fn check_protected_path(cmd: &str) -> Verdict {
    // Shells concatenate adjacent quoted/unquoted fragments before opening
    // the file (`".sq"wai/x` === `.sqwai/x`), so strip quotes first: matching
    // the raw text lets fragmented paths dodge the substring gate below.
    // Over-blocking quoted prose that merely mentions .sqwai matches the
    // existing behavior for unquoted text (fail closed on host state).
    let dequoted: String = cmd
        .chars()
        .filter(|c| *c != '"' && *c != '\'' && *c != '`')
        .collect();
    // Shells also expand `x=sqwai; cat .$x/...` into the protected path at
    // runtime (audit C1). Resolve inline assignments before matching so the
    // gate sees what the shell will open.
    let lower = expand_inline_assignments(&dequoted).to_lowercase();
    if lower.contains(".sqwai") {
        return scan_protected_tokens(&lower);
    }
    // What is left can still be a protected path assembled at runtime: a
    // `.$x` / `.${d}` / `.%VAR%` expansion in hidden-path position opens any
    // dot-directory, and no text match can prove which. Not provably safe →
    // approval, never Safe. Ordinary `$VAR` args (token-initial) stay quiet:
    // approval fatigue is its own safety problem.
    if hidden_path_expansion(&lower) {
        return Verdict::NeedsApproval("path assembled from a variable expansion");
    }
    Verdict::Safe
}

/// Substitute `name=value` assignments made earlier on the same command
/// line into later `$name` / `${name}` expansions, to a fixed point (max
/// 4 rounds) so chains like `a=sq; b=wai; cat .$a$b/x` resolve too.
/// Over-expansion (a variable used before its assignment) only ever adds
/// matches — fail closed.
fn expand_inline_assignments(text: &str) -> String {
    let mut vars: Vec<(String, String)> = Vec::new();
    for token in text.split(|c: char| c.is_whitespace() || c == ';' || c == '|' || c == '&') {
        if let Some((name, value)) = token.split_once('=') {
            let valid_name = !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if valid_name && !value.is_empty() {
                vars.push((name.to_string(), value.to_string()));
            }
        }
    }
    if vars.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();
    for _ in 0..4 {
        let mut changed = false;
        for (name, value) in &vars {
            for pattern in [format!("${{{name}}}"), format!("${name}")] {
                if out.contains(&pattern) {
                    out = out.replace(&pattern, value);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    out
}

/// True when an expansion sits in hidden-path position: `$` or `%` right
/// after `.` or a path separator. `cat $FILE` is ordinary use; `cat .$FILE`
/// assembles a dot-directory name the classifier cannot see through.
fn hidden_path_expansion(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    (1..bytes.len()).any(|i| {
        (bytes[i] == b'$' || bytes[i] == b'%') && matches!(bytes[i - 1], b'.' | b'/' | b'\\')
    })
}

fn scan_protected_tokens(lower: &str) -> Verdict {
    // Split into tokens by whitespace and shell operators/delimiters
    for raw_token in lower.split(|c: char| {
        c.is_whitespace()
            || c == ';'
            || c == '|'
            || c == '&'
            || c == '>'
            || c == '<'
            || c == '`'
            || c == '('
            || c == ')'
            || c == '"'
            || c == '\''
            || c == '='
    }) {
        let token = raw_token.trim_matches(|c: char| {
            c == '"' || c == '\'' || c == '`' || c == '(' || c == ')' || c == '[' || c == ']'
        });
        // Every occurrence is examined: stopping at the first would let a
        // lookalike name hide the real segment behind it.
        for (idx, _) in token.match_indices(".sqwai") {
            // `.sqwai` is host state as a path SEGMENT. The old rule looked
            // only at what preceded the match, so any token that merely
            // STARTED with the spelling was blocked as if it were the
            // protected directory: `.sqwaitmp/plans/1.json`, `.sqwai-backup`
            // (#180).
            let prev_is_boundary = match idx.checked_sub(1).map(|i| token.as_bytes()[i]) {
                None => true,
                Some(b'/') | Some(b'\\') | Some(b':') => true,
                // the doubled-dot spelling the old list admitted stays blocked
                // only in segment-leading position
                Some(b'.') => idx == 1 || matches!(token.as_bytes()[idx - 2], b'/' | b'\\'),
                Some(_) => false,
            };
            let rest = &token[idx + ".sqwai".len()..];
            let ends_segment = rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\');
            if !prev_is_boundary || !ends_segment {
                continue;
            }

            let rest_norm = rest.replace('\\', "/");

            // Allowed exceptions:
            // 1. .sqwai/skills or .sqwai/skills/ or .sqwai/skills/...
            // 2. .sqwai/config.toml
            // (Both must not contain ".." path traversal)
            let is_skills = rest_norm == "/skills"
                || rest_norm == "/skills/"
                || rest_norm.starts_with("/skills/");
            let is_config = rest_norm == "/config.toml";

            let has_traversal = rest_norm.contains("..");

            if (is_skills || is_config) && !has_traversal {
                continue;
            }

            return Verdict::Blocked("protected_path");
        }
    }

    Verdict::Safe
}

// ---------------------------------------------------------------------------
// Layer 1 — heuristic
// ---------------------------------------------------------------------------

fn heuristic_classify(shell: ShellKind, cmd: &str) -> Verdict {
    let lower = cmd.to_lowercase();

    // Decoding an opaque payload and piping/redirecting it onward hides the
    // real bytes from every text-based layer (audit M10). Decoding to stdout
    // alone stays quiet — it is just `cat` with extra steps.
    if decodes_and_forwards(&lower) {
        return Verdict::NeedsApproval("decoding and forwarding an opaque payload");
    }

    if matches!(shell, ShellKind::Cmd) {
        if contains_any(
            &lower,
            &[
                "rd /s", "rmdir /s", "del /f", "del /s", "format ", "diskpart",
            ],
        ) {
            return Verdict::NeedsApproval("destructive disk/filesystem operation");
        }
        if lower.contains("erase ") || lower.contains("rd /q") || lower.contains("rmdir /q") {
            return Verdict::NeedsApproval("destructive filesystem operation");
        }
    }

    if matches!(shell, ShellKind::PowerShell) {
        if powershell_recursive_delete(&lower) {
            return Verdict::NeedsApproval("recursive delete");
        }
        if contains_any(&lower, &["format-volume", "clear-disk", "remove-partition"]) {
            return Verdict::NeedsApproval("destructive disk/filesystem operation");
        }
        if powershell_critical_write(&lower) {
            return Verdict::NeedsApproval("write to a critical system path");
        }
    }

    if matches!(shell, ShellKind::Cmd) && cmd_critical_redirect(&lower) {
        return Verdict::NeedsApproval("redirect overwrites a critical path");
    }

    // --- destructive filesystem ops -------------------------------------
    if contains_any(
        &lower,
        &[
            "mkfs",
            "format ",
            "format/",
            "diskpart",
            "cipher /w",
            "cipher -w",
            "rd /s /q c:",
            "rmdir /s /q c:",
            "del /f /s /q c:",
            "rm -rf /",
            "rm -fr /",
            "> /dev/sda",
            "dd if=",
            "shred ",
            ":(){:|:&};:",
            "fork(){{|:&}};:" as &str,
        ],
    ) {
        return Verdict::NeedsApproval("destructive disk/filesystem operation");
    }
    // rm with recursive flag anywhere
    if looks_like_rm_rf(&lower) {
        return Verdict::NeedsApproval("recursive delete");
    }
    if lower.contains("remove-item") && lower.contains("-recurse") {
        return Verdict::NeedsApproval("recursive delete");
    }

    // --- privilege escalation / system control ---------------------------
    for w in [
        "sudo ",
        "doas ",
        "su root",
        "shutdown",
        "reboot",
        "poweroff",
        "halt",
        "bcdedit",
        "schtasks /delete",
        "taskkill /f /im winlogon",
        "net user ",
        "net localgroup",
    ] {
        if lower.contains(w) {
            return Verdict::NeedsApproval("privilege/system control");
        }
    }

    // --- dynamic code execution -----------------------------------------
    // `eval`, `source` and `sh -c` turn remote bytes into code only with a
    // dynamic argument — the static forms (`sh -c "echo hi"`) stay Safe.
    // Process substitution `<(...)` always executes by construction.
    if has_dynamic_code(&lower) {
        return Verdict::NeedsApproval("dynamic code execution");
    }

    // --- remote code execution patterns ----------------------------------
    if pipe_into_shell(&lower)
        || lower.contains("invoke-expression")
        || lower.contains("iex ")
        || lower.contains("iex(")
        || lower.contains("| iex")
        || lower.contains("| invoke-expression")
        || (lower.contains("curl") || lower.contains("wget") || has_pwsh_downloader(&lower))
            && pipe_into_shell(&lower)
    {
        return Verdict::NeedsApproval("remote code execution pattern");
    }

    // --- powershell code-execution primitives ---------------------------
    // No bash-AST backstop exists on Windows, so heuristics are the only
    // layer that sees these: data-into-code and stealth-execution shapes
    // ask first. (Plain `Get-ExecutionPolicy` stays Safe: only the setter
    // and the `-ExecutionPolicy` parameter form match.)
    if contains_any(
        &lower,
        &[
            "set-executionpolicy",
            " -executionpolicy",
            " -ex ",
            "-encodedcommand",
            " -enc ",
            "downloadstring",
            "net.webclient",
            // `[Reflection.Assembly]` and `[System.Reflection.Assembly]`
            "reflection.assembly",
            "add-type",
        ],
    ) || (lower.contains("start-process") && lower.contains("hidden"))
    {
        return Verdict::NeedsApproval("powershell code execution pattern");
    }

    // --- forceful git operations (token-level, position-independent) -------
    if let Some(reason) = check_git_destructive(&lower) {
        return Verdict::NeedsApproval(reason);
    }

    // --- cross-platform find -delete / -exec & tee to critical path --------
    if check_find_destructive(&lower) {
        return Verdict::NeedsApproval("find -delete/-exec (bulk file destruction)");
    }
    if check_tee_critical(&lower) {
        return Verdict::NeedsApproval("redirect overwrites a critical path");
    }
    if check_unc_network_path(cmd) {
        return Verdict::NeedsApproval("network UNC path access");
    }

    // --- package publishing ------------------------------------------------
    for p in ["npm publish", "cargo publish", "pip upload", "twine upload"] {
        if lower.contains(p) {
            return Verdict::NeedsApproval("publishes a package");
        }
    }

    // --- mass permissions ----------------------------------------------------
    if (lower.contains("chmod -r 777") || lower.contains("chmod -r 000"))
        && !in_project_scope(&lower)
    {
        return Verdict::NeedsApproval("massive permission change");
    }

    Verdict::Safe
}

/// Token-level detector for destructive/forceful git subcommands across all
/// shells. Handles global git options (`git -C dir push origin main --force`)
/// and flags in any position (`--force`, `-f`, `--force-with-lease`, `+ref`,
/// `clean -fd`, `reset --hard`, `branch -D`, `stash clear/drop`).
fn check_git_destructive(lower: &str) -> Option<&'static str> {
    const GIT_VALUE_FLAGS: &[&str] = &[
        "-c",
        "--git-dir",
        "--work-tree",
        "--namespace",
        "--super-prefix",
    ];
    for seg in lower.split([';', '|', '&', '\n']) {
        let words: Vec<&str> = seg.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        let idx = effective_command_index(&words);
        let head = words[idx].trim_end_matches(".exe");
        if head != "git" && !head.ends_with("/git") && !head.ends_with("\\git") {
            continue;
        }
        let mut i = idx + 1;
        let mut subcmd: Option<&str> = None;
        while i < words.len() {
            let w = words[i];
            if w.starts_with('-') {
                if GIT_VALUE_FLAGS.contains(&w) {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            subcmd = Some(w);
            i += 1;
            break;
        }
        let Some(sub) = subcmd else {
            continue;
        };
        let args = &words[i..];
        match sub {
            "push" => {
                let forceful = args.iter().any(|a| {
                    *a == "-f"
                        || *a == "--force"
                        || a.starts_with("--force-with-lease")
                        || a.starts_with("--force-if-includes")
                        || *a == "--delete"
                        || *a == "-d"
                        || (a.starts_with('+') && a.len() > 1)
                        || (a.starts_with(':') && a.len() > 1)
                });
                if forceful {
                    return Some("forceful git operation");
                }
            }
            "reset" => {
                if args.iter().any(|a| *a == "--hard" || *a == "--merge") {
                    return Some("forceful git operation");
                }
            }
            "clean" => {
                if args.iter().any(|a| {
                    *a == "--force"
                        || (a.starts_with('-') && !a.starts_with("--") && a.contains('f'))
                }) {
                    return Some("forceful git operation");
                }
            }
            "checkout" | "switch" => {
                if args
                    .iter()
                    .any(|a| *a == "-f" || *a == "--force" || *a == ".")
                {
                    return Some("forceful git operation");
                }
            }
            "restore" => {
                if args.contains(&".") && !args.contains(&"--staged") && !args.contains(&"-s") {
                    return Some("forceful git operation");
                }
            }
            "branch" => {
                if args
                    .iter()
                    .any(|a| *a == "-d" || *a == "-D" || *a == "--delete")
                {
                    return Some("forceful git operation");
                }
            }
            "stash" => {
                if args.first().is_some_and(|a| *a == "clear" || *a == "drop") {
                    return Some("forceful git operation");
                }
            }
            "filter-branch" | "filter-repo" => {
                return Some("forceful git operation");
            }
            _ => {}
        }
    }
    None
}

fn check_find_destructive(lower: &str) -> bool {
    for seg in lower.split([';', '|', '&', '\n']) {
        let words: Vec<&str> = seg.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        let idx = effective_command_index(&words);
        let head = words[idx].trim_end_matches(".exe");
        if (head == "find" || head.ends_with("/find"))
            && words[idx + 1..].iter().any(|w| {
                matches!(
                    *w,
                    "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-fprint" | "-fls"
                )
            })
        {
            return true;
        }
    }
    false
}

fn check_tee_critical(lower: &str) -> bool {
    for seg in lower.split([';', '|', '&', '\n']) {
        let words: Vec<&str> = seg.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        let idx = effective_command_index(&words);
        let head = words[idx].trim_end_matches(".exe");
        if (head == "tee" || head.ends_with("/tee"))
            && words[idx + 1..]
                .iter()
                .filter(|w| !w.starts_with('-'))
                .any(|w| {
                    let clean = w.trim_matches(['"', '\'']);
                    is_critical_path(clean)
                        || clean.contains("c:\\windows")
                        || clean.contains("c:/windows")
                        || clean.contains("\\system32")
                })
        {
            return true;
        }
    }
    false
}

fn check_unc_network_path(cmd: &str) -> bool {
    for raw in cmd.split_whitespace() {
        let token = raw.trim_matches(['"', '\'']);
        if let Some(rest) = token.strip_prefix("\\\\")
            && let Some((host, share)) = rest.split_once('\\')
            && !host.is_empty()
            && !share.is_empty()
            && host != "."
            && host != "?"
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            return true;
        }
    }
    false
}

fn contains_any(hay: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| hay.contains(n))
}

/// `rm` combined with a recursive/force flag
fn looks_like_rm_rf(lower: &str) -> bool {
    // token-level check to avoid matching "confirm" etc.
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    for (i, t) in tokens.iter().enumerate() {
        if *t == "rm" {
            let rest = &tokens[i + 1..];
            // any flag containing 'r' (recursive): -r, -rf, -fr, --recursive
            if rest
                .iter()
                .any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('r'))
            {
                return true;
            }
            if rest.contains(&"--recursive") {
                return true;
            }
        }
    }
    false
}

fn powershell_recursive_delete(lower: &str) -> bool {
    // #173: `rmdir` is a standard Remove-Item alias — without it
    // `rmdir -Recurse` slipped through as Safe
    let aliases = ["remove-item", "ri", "rm", "rmdir", "del", "erase", "rd"];
    let tokens: Vec<&str> = lower
        .split(|c: char| c.is_whitespace() || c == ';' || c == '|')
        .map(|token| token.trim())
        .filter(|token| !token.is_empty())
        .collect();
    let has_delete = tokens
        .iter()
        .any(|token| aliases.contains(&token.trim_start_matches('-')));
    let has_recurse = tokens.iter().any(|token| {
        *token == "-r"
            || *token == "-recurse"
            || token.starts_with("-recurse:")
            || token.starts_with("-r:")
    }) || lower.contains("-recurse");
    has_delete && has_recurse
}

fn powershell_critical_write(lower: &str) -> bool {
    // Redirection needs no space before its target (`echo evil>C:\Windows\x`),
    // so the space-sensitive forms `"> "` / `">> "` let exactly the writes this
    // gate exists for slip through (#174). Same shape as
    // `cmd_critical_redirect`: any redirect operator plus a critical path.
    // `c:\\boot` is listed as the cmd gate lists it: the PowerShell spelling
    // was missing it, so redirecting to the boot configuration asked nothing.
    (["set-content", "add-content", "out-file"]
        .iter()
        .any(|write| lower.contains(write))
        || lower.contains('>'))
        && [
            "c:\\windows",
            "c:/windows",
            "\\system32",
            "c:\\boot",
            "/etc/",
            "/boot/",
        ]
        .iter()
        .any(|path| lower.contains(path))
}

fn cmd_critical_redirect(lower: &str) -> bool {
    (lower.contains('>') || lower.contains("copy "))
        && ["c:\\windows", "c:/windows", "\\system32", "c:\\boot"]
            .iter()
            .any(|path| lower.contains(path))
}

/// true if any `|`-segment runs a shell/interpreter as its head command.
/// Evaluates downloaded/generated bytes as code: `eval`/`source` and
/// `sh -c`-style interpreter invocations only count with a dynamic argument
/// (`$(...)`, backticks, `<(...)`); their static forms are transparent.
/// A bare process substitution executes unconditionally. Shell-agnostic
/// substring scan like the rest of this layer: a quoted literal is a rare,
/// approval-cheap false positive.
fn has_dynamic_code(lower: &str) -> bool {
    if lower.contains("<(") {
        return true;
    }
    for segment in lower.split([';', '&', '|', '\n']) {
        let mut words = segment.split_whitespace();
        let head = words.next().unwrap_or("");
        let rest: Vec<&str> = words.collect();
        // Any dollar form counts, not just `$(...)`: `eval "$PAYLOAD"` runs
        // bytes the classifier never sees (audit C2). The shell expands a
        // bare `$VAR` just the same.
        let dynamic = rest
            .iter()
            .any(|w| w.contains('$') || w.contains('`') || w.contains("<("));
        if !dynamic {
            continue;
        }
        if head == "eval" || head == "source" || head == "." {
            return true;
        }
        let head_clean = head.strip_suffix(".exe").unwrap_or(head);
        if matches!(
            head_clean,
            "sh" | "bash"
                | "dash"
                | "zsh"
                | "ksh"
                | "fish"
                | "pwsh"
                | "powershell"
                | "python"
                | "python3"
                | "python2"
        ) && (rest.contains(&"-c") || rest.contains(&"-command"))
        {
            return true;
        }
        if matches!(head_clean, "node" | "bun" | "deno" | "perl" | "ruby" | "lua")
            && (rest.contains(&"-e") || rest.contains(&"--eval"))
        {
            return true;
        }
        if head_clean == "cmd" && rest.iter().any(|w| w.eq_ignore_ascii_case("/c")) {
            return true;
        }
    }
    false
}

/// `base64 -d`/`xxd -r`/`certutil -decode` whose output is piped or
/// redirected: the bytes every other layer inspects are not the bytes that
/// run or land on disk (audit M10). Decode-to-stdout stays quiet.
fn decodes_and_forwards(lower: &str) -> bool {
    if !lower.contains('|') && !lower.contains('>') {
        return false;
    }
    let words: Vec<&str> = lower.split_whitespace().collect();
    // combined short flags (`-di`, `-rp`) count too
    let short_flag =
        |flag: &str, c: char| flag.starts_with('-') && !flag.starts_with("--") && flag.contains(c);
    words.windows(2).any(|w| match w {
        ["base64", flag] => matches!(*flag, "-d" | "-D" | "--decode") || short_flag(flag, 'd'),
        ["xxd", flag] => matches!(*flag, "-r" | "--revert") || short_flag(flag, 'r'),
        ["certutil", flag] => *flag == "-decode",
        _ => false,
    })
}

/// Token-aware, not substring: `| head` stays Safe while `| sh`,
/// `| pwsh -c`, `| powershell.exe` flag. Mirrors INTERPRETERS below.
fn pipe_into_shell(lower: &str) -> bool {
    // `lower` is already lowercased by the caller
    for segment in lower.split('|').skip(1) {
        let seg = segment.trim_start().trim_start_matches('&').trim_start();
        let words: Vec<&str> = seg
            .split_whitespace()
            .map(|w| w.trim_end_matches([';', '&']))
            .filter(|w| !w.is_empty())
            .collect();
        if words.is_empty() {
            continue;
        }
        let idx = effective_command_index(&words);
        let token = words[idx].strip_suffix(".exe").unwrap_or(words[idx]);
        let token = token.rsplit(['/', '\\']).next().unwrap_or(token);
        if INTERPRETERS.contains(&token) {
            return true;
        }
    }
    false
}

/// PowerShell downloaders: curl/wget are matched by substring above;
/// these aliases only count as whole tokens (`confirm` must stay Safe).
fn has_pwsh_downloader(lower: &str) -> bool {
    lower
        .split(|c: char| {
            c.is_whitespace() || c == ';' || c == '|' || c == '&' || c == '(' || c == ')'
        })
        .any(|t| matches!(t, "irm" | "iwr" | "invoke-restmethod" | "invoke-webrequest"))
}

/// very rough check that the path being chmod-ed is inside the project
fn in_project_scope(_lower: &str) -> bool {
    // v1: any recursive chmod asks; refine when tool calls carry cwd context
    false
}

// ---------------------------------------------------------------------------
// Layer 2 — tree-sitter-bash AST
// ---------------------------------------------------------------------------

/// command heads that are shells/interpreters: piping into these runs code
const INTERPRETERS: &[&str] = &[
    "sh",
    "bash",
    "dash",
    "zsh",
    "fish",
    "ksh",
    "pwsh",
    "powershell",
    "cmd",
    "python",
    "python3",
    "python2",
    "node",
    "bun",
    "deno",
    "perl",
    "ruby",
    "php",
    "lua",
];

/// command heads that are dangerous by name regardless of context
const DANGEROUS_HEADS: &[&str] = &[
    "dd", "mkfs", "shutdown", "reboot", "poweroff", "halt", "diskpart",
];

/// command heads that elevate privileges (skip them to find the real target)
/// Transparent execution wrappers: the real command follows the prefix (and
/// possibly its flags/numeric args). One list for both layers that strip
/// them — the AST walk here and the subagent scope gate in
/// `tools/policy.rs`. They diverged once already, and `nohup mv` escaped
/// the scope (audit H2).
pub(crate) const ELEVATION_PREFIXES: &[&str] = &[
    "sudo", "doas", "env", "nice", "nohup", "xargs", "time", "timeout", "strace",
];

/// Flags that consume the word after them, per wrapper.
///
/// `effective_command_index` skipped every flag and every numeric token
/// generically, so the *value* of a flag became the command: `sudo -u root rm
/// /etc/shadow` reported its binary as `root`, `strace -o log cp a b` as `log`
/// (audit H2 residual). The scope gate in `tools/policy.rs` reads the same
/// index, so both were looking at the wrong word.
///
/// Deliberately a list of the flags that take a value, not a full grammar: a
/// flag that is not listed behaves as before (skip the flag, stop at its
/// value), which is the shape that over-estimates risk rather than
/// under-estimating it.
const WRAPPER_VALUE_FLAGS: &[(&str, &[&str])] = &[
    (
        "sudo",
        &[
            "-u",
            "-g",
            "-h",
            "-p",
            "-r",
            "-t",
            "-C",
            "-D",
            "-R",
            "-b",
            "--user",
            "--group",
            "--prompt",
            "--chroot",
            "--other-user",
        ],
    ),
    ("doas", &["-u", "-l"]),
    ("env", &["-u", "-C", "-P", "-S", "-v", "--unset", "--chdir"]),
    ("nice", &["-n", "-a"]),
    ("strace", &["-o", "-s", "-p", "-P", "-e", "-a", "-S", "-U"]),
    (
        "xargs",
        &[
            "-n",
            "-I",
            "-d",
            "-E",
            "-P",
            "-s",
            "-L",
            "-N",
            "--max-args",
            "--replace",
        ],
    ),
    ("timeout", &["-k", "-s", "--kill-after", "--signal"]),
    ("time", &["-f", "-o"]),
    ("nohup", &[]),
];

/// True when `flag` is a known value-consuming option of `wrapper`.
fn flag_takes_value(wrapper: Option<&str>, flag: &str) -> bool {
    let Some(known) = wrapper else {
        return false;
    };
    // `-x5` and `--flag=value` carry their value inside the token
    if flag.len() > 2 && !flag.starts_with("--") {
        return false;
    }
    if flag.contains('=') {
        return false;
    }
    WRAPPER_VALUE_FLAGS
        .iter()
        .find(|(name, _)| *name == known)
        .is_some_and(|(_, flags)| flags.contains(&flag))
}

/// Index of the operative command word after transparent wrappers, `VAR=`
/// assignments and the wrapper's own flags/numeric args (`nice -n 10`,
/// `timeout 30`). Shared by the AST walk and the scope gate so both see the
/// same command. May point at the last word when nothing but wrappers is
/// present.
pub(crate) fn effective_command_index(words: &[&str]) -> usize {
    let mut i = 0;
    let mut wrapper: Option<&str> = None;
    while i < words.len() {
        let w = words[i];
        if ELEVATION_PREFIXES.contains(&w) {
            wrapper = Some(w);
            i += 1;
            continue;
        }
        if !w.starts_with('-') && w.contains('=') {
            i += 1;
            continue;
        }
        if wrapper.is_some() && w.starts_with('-') {
            i += 1;
            if flag_takes_value(wrapper, w) && i < words.len() {
                i += 1;
            }
            continue;
        }
        if wrapper.is_some() && !w.is_empty() && w.chars().all(|c| c.is_ascii_digit()) {
            i += 1;
            continue;
        }
        break;
    }
    i.min(words.len().saturating_sub(1))
}

/// paths that must never be overwritten by a redirect
fn is_critical_path(path: &str) -> bool {
    let p = path.trim().to_lowercase();
    // device / disk images
    if p.starts_with("/dev/sd")
        || p.starts_with("/dev/nvme")
        || p.starts_with("/dev/hd")
        || p.starts_with("/dev/mmcblk")
        || p.starts_with("/dev/vd")
    {
        return true;
    }
    // shell/identity config
    let critical = [
        ".bashrc",
        ".bash_profile",
        ".bash_login",
        ".zshrc",
        ".zprofile",
        ".profile",
        ".login",
        ".cshrc",
        ".inputrc",
        ".gitconfig",
        ".git-credentials",
        ".ssh/",
        ".ssh",
        ".gnupg/",
        ".aws/",
        "authorized_keys",
    ];
    if critical.iter().any(|c| p.contains(c)) {
        return true;
    }
    // system dirs
    if p.starts_with("/etc/")
        || p.starts_with("/boot")
        || p.starts_with("/usr/")
        || p.starts_with("/bin")
        || p.starts_with("/sbin")
        || p.starts_with("/var/")
    {
        return true;
    }
    false
}

fn ast_classify(cmd: &str) -> Verdict {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        // parse failure: rely on the heuristic layer alone
        return Verdict::Safe;
    }
    let Some(tree) = parser.parse(cmd, None) else {
        return Verdict::Safe;
    };
    walk(&tree, cmd)
}

fn walk(tree: &Tree, src: &str) -> Verdict {
    // search the whole tree, depth-first; bail on the first hit
    fn rec(node: Node, src: &str) -> Verdict {
        let src_bytes = src.as_bytes();

        // structural checks at this node
        match node.kind() {
            "pipeline" => {
                if pipeline_into_interpreter(&node, src) {
                    return Verdict::NeedsApproval("pipe into interpreter");
                }
            }
            "command" => {
                if let Some((head, words, elev)) = command_parts(&node, src) {
                    // find -delete / find -exec (and kin): bulk execution
                    if head == "find"
                        && words.iter().any(|w| {
                            w == "-delete"
                                || w == "-exec"
                                || w == "-execdir"
                                || w == "-ok"
                                || w == "-okdir"
                        })
                    {
                        return Verdict::NeedsApproval(
                            "find -delete/-exec (bulk file destruction)",
                        );
                    }
                    // dangerous head by name
                    if DANGEROUS_HEADS.contains(&head.as_str()) {
                        return Verdict::NeedsApproval("dangerous command name (disk/system)");
                    }
                    // elevation prefix wrapping a destructive/recursive action
                    if elev && words.iter().any(|w| w == "rm" || w.starts_with("rm-r")) {
                        return Verdict::NeedsApproval("elevated destructive command");
                    }
                    // recursive delete through elevation or xargs
                    if (elev || head == "xargs")
                        && words.iter().any(|w| w.starts_with("rm"))
                        && words.iter().any(|w| w.starts_with('-') && w.contains('r'))
                    {
                        return Verdict::NeedsApproval("elevated recursive delete");
                    }
                }
            }
            "command_name" => {
                // a command_substitution used as the command head = dynamic code
                if node
                    .child(0)
                    .is_some_and(|c| c.kind() == "command_substitution")
                {
                    return Verdict::NeedsApproval("command substitution as command head");
                }
            }
            "file_redirect" => {
                // inspect the redirect target path node
                for child in node.named_children(&mut node.walk()) {
                    let raw = child.utf8_text(src_bytes).unwrap_or("");
                    let text = raw.trim().trim_matches(['"', '\'']);
                    if matches!(
                        child.kind(),
                        "word" | "string" | "raw_string" | "concatenation"
                    ) && !text.is_empty()
                    {
                        let operator = node.utf8_text(src_bytes).unwrap_or("");
                        // `>` / `>>` truncate-overwrite; `<` is input (fine)
                        if operator.contains('>') && is_critical_path(text) {
                            return Verdict::NeedsApproval("redirect overwrites a critical path");
                        }
                    }
                }
            }
            _ => {}
        }

        // recurse
        let mut c = node.walk();
        for child in node.children(&mut c) {
            if let v @ Verdict::NeedsApproval(_) = rec(child, src) {
                return v;
            }
        }
        Verdict::Safe
    }

    rec(tree.root_node(), src)
}

/// true if a pipeline has an interpreter as the head of a command after `|`
fn pipeline_into_interpreter(node: &Node, src: &str) -> bool {
    let mut c = node.walk();
    for child in node.children(&mut c) {
        if child.kind() == "command"
            && let Some((head, _, _)) = command_parts(&child, src)
            && INTERPRETERS.contains(&head.as_str())
        {
            return true;
        }
    }
    false
}

/// Extract (head_name, all_word_args, elevated) from a `command` node.
/// Variable assignments and elevation-prefix words are skipped so the real
/// command gets classified, not `env`/`sudo`.
fn command_parts(node: &Node, src: &str) -> Option<(String, Vec<String>, bool)> {
    let mut words: Vec<String> = Vec::new();
    let mut elevated = false;
    let mut got_head = false;
    let mut c = node.walk();
    for child in node.children(&mut c) {
        let text = child
            .utf8_text(src.as_bytes())
            .ok()
            .unwrap_or("")
            .to_string();
        match child.kind() {
            "command_name" => {
                // command_name children are `word` tokens; use concatenated text
                if !got_head {
                    words.insert(0, text);
                    got_head = true;
                }
            }
            "variable_assignment" => continue,
            "word" | "argument" | "string" | "raw_string" | "literal" => {
                if !got_head {
                    // prefix words like FOO=bar were handled; plain word before
                    // an explicit command_name shouldn't happen, but if it does
                    // treat a leading word as the (possibly elevated) head.
                    words.insert(0, text);
                    got_head = true;
                } else {
                    words.push(text);
                }
            }
            _ => {}
        }
    }
    if words.is_empty() {
        return None;
    }
    // strip elevation prefixes to find the real head
    let mut head = words[0].clone();
    if ELEVATION_PREFIXES.contains(&head.as_str()) {
        elevated = true;
        let refs: Vec<&str> = words.iter().map(|w| w.as_str()).collect();
        head = words[effective_command_index(&refs)].clone();
    }
    Some((head, words, elevated))
}

/// Split a shell command into independent subcommands across `&&`, `||`, `;`,
/// `|`, `|&`, `&`, and `\n` (respecting quotes and escapes), while also
/// extracting inner commands from `$(...)` and `` `...` `` substitutions.
/// Returns `Err` if quotes/substitutions are unterminated or if a list
/// operator (`&&`, `||`, `|`) is left dangling at the end (Claude Code /
/// OpenCode `opaque` rule).
pub fn split_compound_commands(cmd: &str) -> Result<Vec<String>, &'static str> {
    let mut out: Vec<String> = Vec::new();
    let mut nested: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = cmd.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut dangling_op = false;

    while let Some(c) = chars.next() {
        if in_single {
            cur.push(c);
            if c == '\'' {
                in_single = false;
            }
            continue;
        }
        if c == '\\' {
            cur.push(c);
            if let Some(next) = chars.next() {
                cur.push(next);
            } else {
                return Err("unterminated escape");
            }
            dangling_op = false;
            continue;
        }
        if in_double {
            if c == '"' {
                in_double = false;
                cur.push(c);
                continue;
            }
            if c == '$' && chars.peek() == Some(&'(') {
                chars.next();
                let inner = consume_balanced_parens(&mut chars)?;
                if let Ok(subs) = split_compound_commands(&inner) {
                    nested.extend(subs);
                }
                cur.push_str("$(");
                cur.push_str(&inner);
                cur.push(')');
                continue;
            }
            cur.push(c);
            continue;
        }

        match c {
            '\'' => {
                in_single = true;
                cur.push(c);
                dangling_op = false;
            }
            '"' => {
                in_double = true;
                cur.push(c);
                dangling_op = false;
            }
            '`' => {
                let mut inner = String::new();
                let mut closed = false;
                while let Some(bc) = chars.next() {
                    if bc == '\\' {
                        if let Some(esc) = chars.next() {
                            inner.push(esc);
                        }
                        continue;
                    }
                    if bc == '`' {
                        closed = true;
                        break;
                    }
                    inner.push(bc);
                }
                if !closed {
                    return Err("unterminated backtick substitution");
                }
                if let Ok(subs) = split_compound_commands(&inner) {
                    nested.extend(subs);
                }
                cur.push('`');
                cur.push_str(&inner);
                cur.push('`');
                dangling_op = false;
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                let inner = consume_balanced_parens(&mut chars)?;
                if let Ok(subs) = split_compound_commands(&inner) {
                    nested.extend(subs);
                }
                cur.push_str("$(");
                cur.push_str(&inner);
                cur.push(')');
                dangling_op = false;
            }
            ';' | '\n' => {
                let seg = cur.trim();
                if !seg.is_empty() {
                    out.push(seg.to_string());
                } else if dangling_op && c == ';' {
                    return Err("invalid empty command after operator");
                }
                cur.clear();
                dangling_op = false;
            }
            '|' => {
                if chars.peek() == Some(&'|') || chars.peek() == Some(&'&') {
                    chars.next();
                }
                let seg = cur.trim();
                if seg.is_empty() {
                    return Err("empty left-hand side before pipe/or operator");
                }
                out.push(seg.to_string());
                cur.clear();
                dangling_op = true;
            }
            '&' => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                    let seg = cur.trim();
                    if seg.is_empty() {
                        return Err("empty left-hand side before &&");
                    }
                    out.push(seg.to_string());
                    cur.clear();
                    dangling_op = true;
                } else if chars.peek() == Some(&'>')
                    || cur.ends_with('>')
                    || cur.ends_with('<')
                {
                    // Redirection descriptor (`&>`, `2>&1`, `<&0`), not a command separator
                    cur.push(c);
                } else {
                    let seg = cur.trim();
                    if !seg.is_empty() {
                        out.push(seg.to_string());
                    }
                    cur.clear();
                    dangling_op = false;
                }
            }
            _ => {
                if !c.is_whitespace() {
                    dangling_op = false;
                }
                cur.push(c);
            }
        }
    }

    if in_single || in_double {
        return Err("unterminated quote");
    }
    if dangling_op && cur.trim().is_empty() {
        return Err("dangling operator at end of command");
    }
    let tail = cur.trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out.extend(nested);
    Ok(out)
}

fn consume_balanced_parens(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Result<String, &'static str> {
    let mut depth = 1usize;
    let mut out = String::new();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            out.push(c);
            continue;
        }
        if c == '\\' {
            out.push(c);
            if let Some(next) = chars.next() {
                out.push(next);
            }
            continue;
        }
        if in_double {
            if c == '"' {
                in_double = false;
            }
            out.push(c);
            continue;
        }
        match c {
            '\'' => in_single = true,
            '"' => in_double = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(out);
                }
            }
            _ => {}
        }
        out.push(c);
    }
    Err("unterminated command substitution")
}

/// Transparent wrappers that can be stripped before computing a permission rule
/// (Claude Code process-wrappers convention: `timeout`, `time`, `nice`, `nohup`,
/// `stdbuf`, `env`). Privilege/exec wrappers (`sudo`, `doas`, `xargs`, `watch`,
/// `find`) are NOT stripped here — they require exact-command matching.
const SAFE_STRIP_WRAPPERS: &[&str] = &["timeout", "time", "nice", "nohup", "stdbuf", "env"];

/// Exec wrappers and dynamic runners whose inner command or action is too broad
/// for a wildcard prefix grant (Claude Code "Exec wrappers and find actions").
const EXACT_ONLY_BINARIES: &[&str] = &[
    "sudo",
    "doas",
    "su",
    "watch",
    "xargs",
    "find",
    "eval",
    "source",
    "sh",
    "bash",
    "dash",
    "zsh",
    "ksh",
    "fish",
    "pwsh",
    "powershell",
    "cmd",
    "iex",
    "invoke-expression",
    "python",
    "python3",
    "py",
    "node",
    "perl",
    "ruby",
    "php",
    "lua",
    "npx",
    "bunx",
    "pnpx",
    "uvx",
];

/// Destructive filesystem binaries where a session grant must also pin the
/// target path arguments (`rm -rf target` must never grant `rm -rf src`).
const TARGET_SCOPED_BINARIES: &[&str] = &[
    "rm",
    "rmdir",
    "del",
    "erase",
    "rd",
    "remove-item",
    "ri",
    "mv",
    "cp",
    "chmod",
    "chown",
    "dd",
    "shred",
    "format",
    "diskpart",
    "mkfs",
];

/// Parsed structure of a single subcommand for session permission grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubcommandGrant {
    pub head: String,
    pub flags: std::collections::BTreeSet<String>,
    pub targets: Option<std::collections::BTreeSet<String>>,
    pub exact_command: Option<String>,
}

impl SubcommandGrant {
    pub fn from_subcommand(sub: &str) -> Self {
        let norm_sub = sub.split_whitespace().collect::<Vec<_>>().join(" ");
        let raw_words: Vec<&str> = sub.split_whitespace().collect();
        if raw_words.is_empty() {
            return Self {
                head: String::new(),
                flags: std::collections::BTreeSet::new(),
                targets: None,
                exact_command: Some(norm_sub),
            };
        }

        // Strip leading safe env assignments (`NODE_ENV=test`) and transparent
        // wrappers (`timeout 30`, `nice -n 10`).
        let mut i = 0usize;
        let mut wrapper: Option<&str> = None;
        while i < raw_words.len() {
            let w = raw_words[i];
            let w_low = w.to_ascii_lowercase();
            if SAFE_STRIP_WRAPPERS.contains(&w_low.as_str()) {
                wrapper = Some(raw_words[i]);
                i += 1;
                continue;
            }
            if !w.starts_with('-') && w.contains('=') {
                i += 1;
                continue;
            }
            if let Some(wrap) = wrapper {
                let wrap_low = wrap.to_ascii_lowercase();
                if w.starts_with('-') {
                    i += 1;
                    if flag_takes_value(Some(wrap_low.as_str()), w) && i < raw_words.len() {
                        i += 1;
                    }
                    continue;
                }
                if !w.is_empty() && w.chars().all(|c| c.is_ascii_digit()) {
                    i += 1;
                    continue;
                }
            }
            break;
        }
        let words = &raw_words[i.min(raw_words.len().saturating_sub(1))..];
        let bin_raw = words[0].trim_matches(['"', '\'']);
        let bin_base = bin_raw
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(bin_raw)
            .trim_end_matches(".exe")
            .to_ascii_lowercase();

        // Exec wrappers, interpreters, or commands with redirects to critical paths require exact match
        if EXACT_ONLY_BINARIES.contains(&bin_base.as_str()) || sub.contains('>') {
            return Self {
                head: bin_base,
                flags: std::collections::BTreeSet::new(),
                targets: None,
                exact_command: Some(norm_sub),
            };
        }

        let head = resolve_arity_head(&bin_base, &words[1..]);
        if [
            "npm exec",
            "pnpm exec",
            "yarn exec",
            "bun x",
            "deno run",
            "deno eval",
            "uv run",
            "poetry run",
            "pipenv run",
        ]
        .iter()
        .any(|p| head == *p || head.starts_with(&format!("{p} ")))
        {
            return Self {
                head: bin_base,
                flags: std::collections::BTreeSet::new(),
                targets: None,
                exact_command: Some(norm_sub),
            };
        }
        let arity_words = head.split_whitespace().count();
        let flags: std::collections::BTreeSet<String> = words[1..]
            .iter()
            .filter(|t| t.len() > 1 && (t.starts_with('-') || (bin_base == "cmd" && t.starts_with('/'))))
            .map(|t| (*t).to_string())
            .collect();

        let targets = if TARGET_SCOPED_BINARIES.contains(&bin_base.as_str()) {
            let positional: std::collections::BTreeSet<String> = words
                .iter()
                .skip(arity_words)
                .filter(|t| !t.starts_with('-') && !(bin_base == "del" && t.starts_with('/')) && !(bin_base == "rd" && t.starts_with('/')) && !(bin_base == "rmdir" && t.starts_with('/')))
                .map(|t| t.trim_matches(['"', '\'']).to_string())
                .filter(|t| !t.is_empty())
                .collect();
            Some(positional)
        } else {
            None
        };

        Self {
            head,
            flags,
            targets,
            exact_command: None,
        }
    }

    pub fn covers(&self, candidate_sub: &str) -> bool {
        let cand = Self::from_subcommand(candidate_sub);
        if let Some(exact) = &self.exact_command {
            let cand_norm = candidate_sub.split_whitespace().collect::<Vec<_>>().join(" ");
            return *exact == cand_norm;
        }
        if cand.exact_command.is_some() {
            return false;
        }
        if self.head != cand.head || !cand.flags.is_subset(&self.flags) {
            return false;
        }
        if let Some(allowed_targets) = &self.targets {
            let Some(cand_targets) = &cand.targets else {
                return false;
            };
            if cand_targets.is_empty() || !cand_targets.is_subset(allowed_targets) {
                return false;
            }
        }
        true
    }

    pub fn pattern_label(&self) -> String {
        if let Some(exact) = &self.exact_command {
            return exact.clone();
        }
        let mut parts = vec![self.head.clone()];
        for f in &self.flags {
            parts.push(f.clone());
        }
        if let Some(targets) = &self.targets
            && !targets.is_empty()
        {
            for t in targets {
                parts.push(t.clone());
            }
            return parts.join(" ");
        }
        parts.push("*".to_string());
        parts.join(" ")
    }
}

/// Arity-aware command prefix (OpenCode `parse.ts` ARITY convention):
/// - `git <subcmd>` (skipping global flags like `-C <dir>`) -> `"git <subcmd>"`
/// - `docker compose <subcmd>` -> `"docker compose <subcmd>"`
/// - `cargo/npm/pnpm/yarn/bun/gh/kubectl <subcmd>` -> 2–3 words
/// - single-verb binaries (`rm`, `chmod`, `curl`, `ssh`) -> 1 word
fn resolve_arity_head(bin_base: &str, rest: &[&str]) -> String {
    if bin_base == "git" {
        const GIT_VAL_FLAGS: &[&str] = &[
            "-c",
            "-C",
            "--git-dir",
            "--work-tree",
            "--namespace",
            "--super-prefix",
        ];
        let mut i = 0usize;
        while i < rest.len() {
            let w = rest[i];
            if w.starts_with('-') {
                if GIT_VAL_FLAGS.contains(&w) {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            return format!("git {}", w.to_ascii_lowercase());
        }
        return "git".to_string();
    }

    let non_flag: Vec<String> = rest
        .iter()
        .take_while(|w| !w.starts_with('-'))
        .take(2)
        .map(|w| w.to_ascii_lowercase())
        .collect();

    if let Some(first) = non_flag.first() {
        let two = format!("{bin_base} {first}");
        if matches!(
            two.as_str(),
            "docker compose"
                | "docker container"
                | "docker image"
                | "docker volume"
                | "docker network"
                | "docker builder"
                | "cargo run"
                | "cargo add"
                | "npm run"
                | "npm exec"
                | "pnpm run"
                | "pnpm exec"
                | "yarn run"
                | "bun run"
                | "deno task"
                | "gh pr"
                | "gh issue"
                | "gh repo"
                | "kubectl rollout"
        ) && let Some(second) = non_flag.get(1)
        {
            return format!("{two} {second}");
        }
        if matches!(
            bin_base,
            "cargo"
                | "npm"
                | "pnpm"
                | "yarn"
                | "bun"
                | "deno"
                | "docker"
                | "podman"
                | "kubectl"
                | "helm"
                | "terraform"
                | "gh"
                | "brew"
                | "pip"
                | "uv"
                | "pipenv"
                | "poetry"
                | "twine"
                | "go"
                | "rustup"
                | "systemctl"
                | "ufw"
                | "net"
                | "schtasks"
        ) {
            return two;
        }
    }
    bin_base.to_string()
}

/// Human-readable preview of what "always allow this session" will grant for `command`.
pub fn session_grant_preview(command: &str) -> String {
    let Ok(subs) = split_compound_commands(command) else {
        return command.trim().to_string();
    };
    let shell = ShellKind::detect();
    let mut labels: Vec<String> = Vec::new();
    for sub in &subs {
        let is_risky = !matches!(classify_for(shell, sub), Verdict::Safe)
            || egress_kind(sub).is_some();
        if is_risky {
            let lbl = SubcommandGrant::from_subcommand(sub).pattern_label();
            if !labels.contains(&lbl) {
                labels.push(lbl);
            }
        }
    }
    if labels.is_empty()
        && let Some(first) = subs.first()
    {
        labels.push(SubcommandGrant::from_subcommand(first).pattern_label());
    }
    labels.join(" ; ")
}

#[cfg(test)]
mod tests;
