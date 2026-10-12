//! Two-layer safety classification for shell commands (design §4.4).
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

/// Outward-data commands for the trust gate (R, §2.2): network upload and
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

    // --- forceful git operations -----------------------------------------
    for g in [
        "push --force",
        "push -f",
        "reset --hard",
        "clean -fdx",
        "clean -dfx",
        "checkout -- .",
        "restore .",
        "branch -d ",
        "branch -D ",
    ] {
        if lower.contains(g) {
            return Verdict::NeedsApproval("forceful git operation");
        }
    }
    // force push via refspec: `git push origin +master` moves the force flag
    // into the refspec, so the substring checks above never see it
    if lower.contains("git push") {
        let tokens: Vec<&str> = lower.split_whitespace().collect();
        if let Some(pos) = tokens.iter().position(|t| *t == "push")
            && tokens[pos + 1..].iter().any(|t| t.starts_with('+'))
        {
            return Verdict::NeedsApproval("forceful git operation");
        }
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
        if matches!(head, "sh" | "bash" | "dash" | "zsh" | "ksh" | "fish") && rest.contains(&"-c") {
            return true;
        }
        if head == "cmd" && rest.iter().any(|w| w.eq_ignore_ascii_case("/c")) {
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
    const SHELLS: &[&str] = &[
        "sh",
        "bash",
        "dash",
        "zsh",
        "fish",
        "ksh",
        "powershell",
        "pwsh",
        "cmd",
    ];
    // `lower` is already lowercased by the caller
    for segment in lower.split('|').skip(1) {
        let seg = segment.trim_start().trim_start_matches('&').trim_start();
        let mut token = seg.split_whitespace().next().unwrap_or("");
        token = token.trim_end_matches([';', '&']);
        // `powershell.exe`, `pwsh.exe`
        let token = token.strip_suffix(".exe").unwrap_or(token);
        if SHELLS.contains(&token) {
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

/// Index of the operative command word after transparent wrappers, `VAR=`
/// assignments and the wrapper's own flags/numeric args (`nice -n 10`,
/// `timeout 30`). Shared by the AST walk and the scope gate so both see the
/// same command. May point at the last word when nothing but wrappers is
/// present.
pub(crate) fn effective_command_index(words: &[&str]) -> usize {
    let mut i = 0;
    let mut saw_wrapper = false;
    while i < words.len() {
        let w = words[i];
        if ELEVATION_PREFIXES.contains(&w) {
            saw_wrapper = true;
            i += 1;
            continue;
        }
        if !w.starts_with('-') && w.contains('=') {
            i += 1;
            continue;
        }
        if saw_wrapper
            && (w.starts_with('-') || (!w.is_empty() && w.chars().all(|c| c.is_ascii_digit())))
        {
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

#[cfg(test)]
mod tests;
