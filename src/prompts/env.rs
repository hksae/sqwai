use ignore::WalkBuilder;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// A stalled `npm --version` costs 1.9s cold on a real machine: 1.5s killed
/// honest-looking probes and printed "unavailable" for tools that exist.
const PROBE_TIMEOUT: Duration = Duration::from_millis(2_500);
/// The block is built at session start and after compaction, off the turn.
const SESSION_PROBE_BUDGET: Duration = Duration::from_secs(3);
/// Directories only, at this cap: the model gets root files from one `ls`,
/// and a mixed snapshot of 40 entries is stale by the tenth move.
const TREE_LIMIT: usize = 28;
/// Installed-version probes cost processes; the primary ecosystems tell the
/// model what can run.
const PROBE_CAP: usize = 5;

/// Facts that cannot change while the process runs: platform, shell, and
/// working directory. This is the first cacheable prompt layer.
pub fn process_block() -> String {
    process_block_at(&std::env::current_dir().unwrap_or_default())
}

/// Same facts, rooted explicitly: bench runs must pass the fixture copy,
/// never the cargo-inherited cwd (which once told a model its workspace
/// was the sqwai repo and cost a 159-turn run in the wrong tree).
pub fn process_block_at(root: &std::path::Path) -> String {
    let shell = shell_name();
    format!(
        "<process_environment>\nPlatform: {} ({})\nShell: {shell}\nWorking directory: {}\n</process_environment>\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        root.display()
    )
}

/// Compatibility name for callers that need only the immutable process facts.
#[allow(dead_code)]
pub fn stable_block() -> String {
    process_block()
}

/// Session-scoped facts, captured on startup and after compaction. They are a
/// cacheable prompt layer: no per-turn Git status belongs here (the date and
/// clock are close enough to the session start, and the anchor refreshes them
/// after compaction). Current changes come from the anchor, FACTS, or an
/// explicit tool call.
pub fn session_block(root: &Path) -> String {
    let results = run_session_probes(root.to_path_buf());
    let now = chrono::Local::now();
    let date = now.format("%Y-%m-%d (%A)");
    let time = now.format("%H:%M (UTC%:z)");
    let mut out = format!("<session_environment>\nDate: {date}\nTime: {time}\n");
    for key in ["os", "git", "toolchains"] {
        if let Some(value) = results.get(key) {
            out.push_str(value);
        } else {
            out.push_str(&format!("{}: unavailable (timeout)\n", key_name(key)));
        }
    }
    out.push_str(&tree_block(root));
    out.push_str("</session_environment>\n");
    out
}

/// Per-turn environment context is intentionally empty. A changing block
/// before conversation history defeats provider prefix caches.
pub fn runtime_context() -> String {
    String::new()
}

fn key_name(key: &str) -> &str {
    match key {
        "os" => "OS",
        "git" => "Git",
        "toolchains" => "Toolchains",
        _ => key,
    }
}

fn run_session_probes(root: PathBuf) -> BTreeMap<String, String> {
    let (tx, rx) = mpsc::channel();
    let mut jobs = 0;
    for (key, task) in [
        ("os", ProbeTask::Os),
        ("git", ProbeTask::Git(root.clone())),
        ("toolchains", ProbeTask::Toolchains(root)),
    ] {
        jobs += 1;
        let tx = tx.clone();
        thread::spawn(move || {
            let value = match task {
                ProbeTask::Os => os_info(),
                ProbeTask::Git(root) => git_info(&root),
                ProbeTask::Toolchains(root) => toolchains(&root),
            };
            let _ = tx.send((key.to_string(), value));
        });
    }
    drop(tx);

    let deadline = Instant::now() + SESSION_PROBE_BUDGET;
    let mut results = BTreeMap::new();
    while results.len() < jobs {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok((key, value)) => {
                results.insert(key, value);
            }
            Err(_) => break,
        }
    }
    results
}

enum ProbeTask {
    Os,
    Git(PathBuf),
    Toolchains(PathBuf),
}

fn shell_name() -> String {
    match crate::agent::shell::ShellKind::detect() {
        crate::agent::shell::ShellKind::Bash => "bash".into(),
        crate::agent::shell::ShellKind::Sh => "sh".into(),
        crate::agent::shell::ShellKind::Cmd => "cmd.exe".into(),
        crate::agent::shell::ShellKind::PowerShell => "PowerShell".into(),
    }
}

fn os_info() -> String {
    #[cfg(windows)]
    {
        let key = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion";
        let product = capture("reg", &["query", key, "/v", "ProductName"])
            .ok()
            .and_then(|value| value_from_reg(&value, "ProductName"));
        let build = capture("reg", &["query", key, "/v", "CurrentBuildNumber"])
            .ok()
            .and_then(|value| value_from_reg(&value, "CurrentBuildNumber"))
            .and_then(|value| value.parse::<u32>().ok());
        let label = match product {
            Some(product) if build.is_some_and(|number| number >= 22_000) => {
                product.replacen("Windows 10", "Windows 11", 1)
            }
            Some(product) => product,
            None => "Windows".into(),
        };
        format!("OS: {label}\n")
    }
    #[cfg(target_os = "linux")]
    {
        let distro = std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|text| {
                text.lines().find_map(|line| {
                    line.strip_prefix("PRETTY_NAME=")
                        .map(|value| value.trim_matches('"').to_string())
                })
            })
            .unwrap_or_else(|| "Linux".into());
        let kernel = capture("uname", &["-r"])
            .map(|value| value.trim().to_string())
            .unwrap_or_else(|error| format!("unavailable ({error})"));
        format!("OS: {distro}; kernel {kernel}\n")
    }
    #[cfg(target_os = "macos")]
    {
        let version = capture("sw_vers", &["-productVersion"])
            .map(|value| value.trim().to_string())
            .unwrap_or_else(|error| format!("unavailable ({error})"));
        format!("OS: macOS {version}\n")
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        format!("OS: {}\n", std::env::consts::OS)
    }
}

#[allow(dead_code)]
fn value_from_reg(output: &str, value_name: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix(value_name)
            .map(str::trim)
            .and_then(|rest| rest.strip_prefix("REG_"))
            .and_then(|rest| rest.split_once(' '))
            .map(|(_, value)| value.trim().to_string())
    })
}

fn git_info(root: &Path) -> String {
    let probe = capture_in(root, "git", &["rev-parse", "--git-dir"]);
    match probe {
        Ok(_) => {
            let branch = match capture_in(root, "git", &["rev-parse", "--abbrev-ref", "HEAD"]) {
                Ok(branch) if branch.trim() == "HEAD" => "detached HEAD".to_string(),
                Ok(branch) => format!("on branch {}", branch.trim()),
                Err(ProbeError::Exit(_)) => "unborn HEAD".to_string(),
                Err(error) => return format!("Git: unavailable ({error})\n"),
            };
            let mut out = format!("Git: {branch}\n");
            // No "N changed file(s)" line: a bare count without names is zero
            // orientation — `git status` one tool call away says more, and it
            // says it current rather than at session start.
            match capture_in(root, "git", &["log", "-3", "--format=- %s"]) {
                Ok(log) if !log.trim().is_empty() => {
                    out.push_str("Recent commits at session start:\n");
                    out.push_str(log.trim_end());
                    out.push('\n');
                }
                Ok(_) | Err(ProbeError::Exit(_)) => {}
                Err(error) => out.push_str(&format!("Recent commits: unavailable ({error})\n")),
            }
            out
        }
        Err(ProbeError::Exit(_)) => "Git: not a repository\n".into(),
        Err(error) => format!("Git: unavailable ({error})\n"),
    }
}

/// Shallow, ignored-aware directory map. Files are deliberately absent: the
/// model reads root files with one `ls`, and mixed entries age out faster
/// than the block that carries them (§14).
fn tree_block(root: &Path) -> String {
    let mut entries = Vec::new();
    for entry in manifest_walk(root).flatten() {
        if entry.depth() == 0 || entry.path().starts_with(root.join(".sqwai")) {
            continue;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_dir()) {
            continue;
        }
        // .git internals are not project structure: seven of the twenty-eight
        // slots here were hooks/ objects/ refs/, and the model cannot work
        // with any of them
        if entry
            .path()
            .strip_prefix(root)
            .map(|rel| {
                rel.components()
                    .next()
                    .is_some_and(|c| c.as_os_str() == ".git")
            })
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        entries.push(relative.to_path_buf());
    }
    entries.sort();

    let omitted = entries.len().saturating_sub(TREE_LIMIT);
    let mut out = String::from("Project tree (directories) at session start:\n");
    for path in entries.into_iter().take(TREE_LIMIT) {
        let depth = path.components().count().saturating_sub(1);
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        out.push_str(&"  ".repeat(depth));
        out.push_str(&name);
        out.push('/');
        out.push('\n');
    }
    if omitted > 0 {
        out.push_str(&format!("… +{omitted} more\n"));
    }
    out
}

/// Project files to depth 2, honoring ignore files: manifests live one or two
/// levels down in monorepos, and a whole-tree walk would spend the budget on
/// `node_modules` we never read.
fn manifest_walk(root: &Path) -> ignore::Walk {
    WalkBuilder::new(root)
        .standard_filters(true)
        .require_git(false)
        .hidden(false)
        .max_depth(Some(2))
        .build()
}

/// Names (and relative paths) of the project's own files, depth ≤ 2.
fn manifest_paths(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in manifest_walk(root).flatten() {
        if entry.depth() == 0 || !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if let Ok(relative) = entry.path().strip_prefix(root) {
            out.push(relative.to_path_buf());
        }
    }
    out
}

/// One installed-version probe. `programs` are candidates tried in order:
/// `python3` does not exist on a machine where Python 3.14 is installed as
/// `python` (measured on this one), so a single-name probe reports a tool
/// that is there as absent.
struct Probe {
    label: &'static str,
    programs: Vec<&'static str>,
    args: &'static [&'static str],
    /// `java -version` prints to stderr; the first stdout-only probe of it
    /// returned an empty string and looked like a missing JDK.
    from_stderr: bool,
}

enum ProbeResult {
    Found(String),
    Missing,
    TimedOut,
}

/// Toolchain facts in the two layers that actually answer different questions
/// (§14): what the project declares it is written for, and what this machine
/// can run. Declared versions cost no processes and never lie about a tool
/// that exists; installed versions are probed, capped, and a timeout is
/// printed as a timeout rather than as "not installed".
fn toolchains(root: &Path) -> String {
    let paths = manifest_paths(root);
    let declared = declared_versions(root, &paths);
    let probes = probe_plan(&paths);
    let installed = run_probes(probes);
    let mut out =
        String::from("Toolchains: declared by manifests; installed probed at session start\n");
    if !declared.is_empty() {
        out.push_str(&format!("  declared: {}\n", declared.join(", ")));
    }
    if !installed.is_empty() {
        out.push_str(&format!("  installed: {}\n", installed.join(", ")));
    }
    out
}

/// The version a manifest pins, with the file it came from. Reading files,
/// zero spawns: an declared version is what to write code against, which is
/// not the same fact as what the machine happens to have installed.
fn declared_versions(root: &Path, paths: &[PathBuf]) -> Vec<String> {
    let find = |name: &str| {
        paths
            .iter()
            .find(|p| p.file_name().is_some_and(|f| f == name))
    };
    let read = |rel: &PathBuf| std::fs::read_to_string(root.join(rel)).ok();
    let mut out: Vec<String> = Vec::new();
    let mut push = |label: &str, value: Option<String>, source: &str| {
        if let Some(value) = value {
            out.push(format!("{label} {value} ({source})"));
        }
    };

    if let Some(rel) = find("rust-toolchain.toml").or_else(|| find("rust-toolchain")) {
        let source = rel
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = read(rel);
        let channel = text.as_deref().and_then(|t| {
            t.lines()
                .find_map(|line| line.split_once('='))
                .map(|(_, v)| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        });
        // a bare rust-toolchain file holds the channel alone, no `key =`
        let channel = channel.or_else(|| {
            text.as_deref()
                .map(|t| t.lines().next().unwrap_or_default().trim().to_string())
                .filter(|v| !v.is_empty())
        });
        push("rust", channel, &source);
    }
    if let Some(rel) = find("go.mod") {
        let go = read(rel).and_then(|t| {
            t.lines()
                .find_map(|line| line.trim().strip_prefix("go ").map(str::to_string))
        });
        push("go", go, "go.mod");
    }
    // .nvmrc is the pin on its own, and the one node has when package.json
    // carries no engines block
    let nvmrc = find(".nvmrc").and_then(&read).map(|t| {
        let first = t.lines().next().unwrap_or_default().trim().to_string();
        first.trim_start_matches('v').to_string()
    });
    if let Some(rel) = find("package.json") {
        let engines = read(rel).and_then(|t| {
            serde_json::from_str::<serde_json::Value>(&t)
                .ok()
                .and_then(|v| {
                    v.pointer("/engines/node")
                        .and_then(|n| n.as_str().map(str::to_string))
                })
        });
        push("node", engines.or(nvmrc), "package.json/.nvmrc");
    } else if nvmrc.is_some() {
        push("node", nvmrc, ".nvmrc");
    }
    if let Some(rel) = find("pyproject.toml") {
        let requires = read(rel).and_then(|t| {
            t.lines().find_map(|line| {
                line.trim()
                    .strip_prefix("requires-python")
                    .and_then(|rest| rest.split_once('='))
                    .map(|(_, v)| v.trim().trim_matches('"').to_string())
            })
        });
        let pinned = find(".python-version")
            .and_then(&read)
            .map(|t| t.lines().next().unwrap_or_default().trim().to_string());
        push(
            "python",
            requires.or(pinned),
            "pyproject.toml/.python-version",
        );
    }
    // the wrapper properties sit three levels down (gradle/wrapper/…), below
    // the manifest walk: stat the conventional path instead of deepening it
    let wrapper = find("gradle-wrapper.properties")
        .cloned()
        .unwrap_or_else(|| {
            PathBuf::from("gradle")
                .join("wrapper")
                .join("gradle-wrapper.properties")
        });
    if root.join(&wrapper).exists() {
        let version = read(&wrapper).and_then(|t| {
            t.lines().find_map(|line| {
                let url = line.split_once('=').map(|(_, v)| v)?;
                let after = url.split("gradle-").nth(1)?;
                Some(after.split('-').next()?.to_string())
            })
        });
        push("gradle", version, "gradle-wrapper.properties");
    }
    if let Some(rel) = find("global.json") {
        let sdk = read(rel).and_then(|t| {
            serde_json::from_str::<serde_json::Value>(&t)
                .ok()
                .and_then(|v| {
                    v.pointer("/sdk/version")
                        .and_then(|n| n.as_str().map(str::to_string))
                })
        });
        push("dotnet", sdk, "global.json");
    }
    out
}

/// Which tools to probe, chosen by the manifests the project actually has —
/// union across ecosystems, capped so a polyglot tree does not spend the
/// whole budget on `--version`.
fn probe_plan(paths: &[PathBuf]) -> Vec<Probe> {
    let has = |name: &str| {
        paths
            .iter()
            .any(|p| p.file_name().is_some_and(|f| f == name))
    };
    let ends = |suffix: &str| paths.iter().any(|p| p.to_string_lossy().ends_with(suffix));
    let python_chain = || match crate::agent::shell::ShellKind::detect() {
        // Git Bash and sh carry python3; cmd.exe and PowerShell usually carry
        // only `python` (and a Store alias that never answers)
        crate::agent::shell::ShellKind::Bash | crate::agent::shell::ShellKind::Sh => {
            vec!["python3", "python"]
        }
        _ => vec!["python", "python3"],
    };
    let mut probes: Vec<Probe> = Vec::new();
    if has("Cargo.toml") {
        probes.push(Probe {
            label: "rustc",
            programs: vec!["rustc"],
            args: &["--version"],
            from_stderr: false,
        });
        probes.push(Probe {
            label: "cargo",
            programs: vec!["cargo"],
            args: &["--version"],
            from_stderr: false,
        });
    }
    if has("package.json") {
        probes.push(Probe {
            label: "node",
            programs: vec!["node"],
            args: &["--version"],
            from_stderr: false,
        });
        let pm = if has("pnpm-lock.yaml") {
            "pnpm"
        } else if has("yarn.lock") {
            "yarn"
        } else if has("bun.lockb") || has("bun.lock") {
            "bun"
        } else {
            "npm"
        };
        probes.push(Probe {
            label: pm,
            programs: vec![pm],
            args: &["--version"],
            from_stderr: false,
        });
    }
    if has("pyproject.toml")
        || has("uv.lock")
        || has("poetry.lock")
        || has("requirements.txt")
        || has("setup.py")
    {
        probes.push(Probe {
            label: "python",
            programs: python_chain(),
            args: &["--version"],
            from_stderr: false,
        });
        if has("uv.lock") {
            probes.push(Probe {
                label: "uv",
                programs: vec!["uv"],
                args: &["--version"],
                from_stderr: false,
            });
        }
        if has("poetry.lock") {
            probes.push(Probe {
                label: "poetry",
                programs: vec!["poetry"],
                args: &["--version"],
                from_stderr: false,
            });
        }
    }
    if has("go.mod") {
        probes.push(Probe {
            label: "go",
            programs: vec!["go"],
            args: &["version"],
            from_stderr: false,
        });
    }
    // the ecosystems the old root-only list missed entirely (§14.3)
    if has("build.gradle")
        || has("build.gradle.kts")
        || has("settings.gradle")
        || has("pom.xml")
        || has("gradle-wrapper.properties")
        || ends(".java")
    {
        probes.push(Probe {
            label: "java",
            programs: vec!["java"],
            args: &["-version"],
            from_stderr: true,
        });
    }
    if has("Gemfile") || ends(".rb") {
        probes.push(Probe {
            label: "ruby",
            programs: vec!["ruby"],
            args: &["--version"],
            from_stderr: false,
        });
    }
    if has("composer.json") || ends(".php") {
        probes.push(Probe {
            label: "php",
            programs: vec!["php"],
            args: &["--version"],
            from_stderr: false,
        });
    }
    if has("build.zig") || ends(".zig") {
        probes.push(Probe {
            label: "zig",
            programs: vec!["zig"],
            args: &["--version"],
            from_stderr: false,
        });
    }
    if has("CMakeLists.txt") || has("meson.build") || ends(".c") || ends(".cpp") {
        probes.push(Probe {
            label: "cc",
            programs: vec!["clang", "gcc"],
            args: &["--version"],
            from_stderr: false,
        });
    }
    if has("global.json") || ends(".csproj") || ends(".sln") {
        probes.push(Probe {
            label: "dotnet",
            programs: vec!["dotnet"],
            args: &["--version"],
            from_stderr: false,
        });
    }
    probes.truncate(PROBE_CAP);
    probes
}

/// Run the probes concurrently under one deadline, so the block costs the
/// slowest probe rather than the sum. A probe that never answered by the
/// deadline is reported as a timeout: "not installed" and "we did not finish
/// asking" are different facts.
fn run_probes(probes: Vec<Probe>) -> Vec<String> {
    if probes.is_empty() {
        return Vec::new();
    }
    let labels: Vec<&'static str> = probes.iter().map(|p| p.label).collect();
    let (tx, rx) = mpsc::channel();
    for probe in probes {
        let tx = tx.clone();
        thread::spawn(move || {
            let outcome = probe_one(&probe);
            let _ = tx.send((probe.label, outcome));
        });
    }
    drop(tx);

    let deadline = Instant::now() + SESSION_PROBE_BUDGET;
    let mut results: BTreeMap<&'static str, ProbeResult> = BTreeMap::new();
    while results.len() < labels.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok((label, outcome)) => {
                results.insert(label, outcome);
            }
            Err(_) => break,
        }
    }
    labels
        .iter()
        .map(|label| {
            let outcome = match results.get(label) {
                Some(found) => found,
                // the whole block ran out of budget before this one answered
                None => &ProbeResult::TimedOut,
            };
            format!("{label} {}", render_outcome(outcome))
        })
        .collect()
}

fn render_outcome(outcome: &ProbeResult) -> String {
    match outcome {
        ProbeResult::Found(version) => version.clone(),
        ProbeResult::Missing => "not installed".into(),
        ProbeResult::TimedOut => "unknown (probe timeout)".into(),
    }
}

fn probe_one(probe: &Probe) -> ProbeResult {
    let mut timed_out = false;
    for program in &probe.programs {
        match capture_streams(program, probe.args) {
            Ok((stdout, stderr)) => {
                let source = if probe.from_stderr {
                    stderr
                } else {
                    stdout.clone()
                };
                let text = if source.trim().is_empty() {
                    stdout
                } else {
                    source
                };
                let line = text.lines().next().unwrap_or_default().trim().to_string();
                if !line.is_empty() {
                    return ProbeResult::Found(short_version(&line));
                }
            }
            Err(ProbeError::Timeout) => timed_out = true,
            Err(_) => {}
        }
    }
    if timed_out {
        ProbeResult::TimedOut
    } else {
        ProbeResult::Missing
    }
}

/// `rustc 1.98.0 (abc123 2026-09-01)` → `1.98.0`: the tool name is already
/// printed, and a commit hash is noise in a context block.
fn short_version(line: &str) -> String {
    let after_name = line.split_once(' ').map(|(_, rest)| rest).unwrap_or(line);
    let candidate = after_name.split(' ').next().unwrap_or("");
    // node prints `v20.11.1` and `go version go1.22.3` keeps its prefix after
    // the word, so a leading 'v' is decoration, not part of the number
    let trimmed = candidate.trim_start_matches('v');
    if trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return trimmed.to_string();
    }
    line.to_string()
}

#[derive(Debug)]
enum ProbeError {
    Spawn(String),
    Exit(i32),
    Timeout,
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => formatter.write_str(error),
            Self::Exit(code) => write!(formatter, "exit {code}"),
            Self::Timeout => formatter.write_str("timeout"),
        }
    }
}

/// Run a short probe without allowing a stalled external command to freeze the
/// TUI. Individual probes self-terminate; callers also impose a shared budget.
fn capture(program: &str, args: &[&str]) -> Result<String, ProbeError> {
    capture_inner(None, program, args).map(|(stdout, _)| stdout)
}

fn capture_in(root: &Path, program: &str, args: &[&str]) -> Result<String, ProbeError> {
    capture_inner(Some(root), program, args).map(|(stdout, _)| stdout)
}

/// Both streams, for tools whose answer is not on stdout.
fn capture_streams(program: &str, args: &[&str]) -> Result<(String, String), ProbeError> {
    capture_inner(None, program, args)
}

fn capture_inner(
    root: Option<&Path>,
    program: &str,
    args: &[&str],
) -> Result<(String, String), ProbeError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // stderr is piped, not nulled: `java -version` answers there
        .stderr(Stdio::piped());
    if let Some(root) = root {
        command.current_dir(root);
    }
    let mut child = command
        .spawn()
        .map_err(|error| ProbeError::Spawn(error.kind().to_string()))?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|error| ProbeError::Spawn(error.kind().to_string()))?;
                if !status.success() {
                    return Err(ProbeError::Exit(status.code().unwrap_or(-1)));
                }
                let clip = |bytes: Vec<u8>| {
                    let mut text = String::from_utf8_lossy(&bytes).into_owned();
                    if text.len() > 4_000 {
                        text.truncate(text.floor_char_boundary(4_000));
                    }
                    text
                };
                return Ok((clip(output.stdout), clip(output.stderr)));
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProbeError::Timeout);
            }
            Err(error) => return Err(ProbeError::Spawn(error.kind().to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_block_has_no_empty_os_fields() {
        let block = process_block();
        assert!(block.contains("Platform:"));
        assert!(!block.contains("  ("));
    }

    /// The block this repo actually gets, checked structurally: the bare
    /// changed-file count is gone, the tree is directories, and the
    /// toolchain header states which layer is which.
    #[test]
    fn session_block_of_this_repo_has_the_repaired_shape() {
        let block = session_block(Path::new(env!("CARGO_MANIFEST_DIR")));
        assert!(block.contains("Git: on branch"), "{block}");
        assert!(
            !block.contains("changed file"),
            "the bare count is retired: {block}"
        );
        assert!(
            block.contains("Project tree (directories) at session start"),
            "{block}"
        );
        assert!(
            block.contains("Toolchains: declared by manifests"),
            "{block}"
        );
        // this repo pins its toolchain, so the declared layer has something
        // to say — read from rust-toolchain.toml, no process spawned
        assert!(block.contains("declared: rust"), "{block}");
    }

    #[test]
    fn runtime_context_is_empty_for_history_cache() {
        assert!(runtime_context().is_empty());
    }

    #[test]
    fn tree_marks_omitted_entries() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..(TREE_LIMIT + 3) {
            std::fs::create_dir(root.path().join(format!("dir-{index}"))).unwrap();
        }
        assert!(tree_block(root.path()).contains("… +3 more"));
    }

    /// §14: directories only. Root files are one `ls` away, and a mixed list
    /// ages out before the block that carries it does.
    #[test]
    fn tree_lists_no_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("README.md"), "x").unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src").join("main.rs"), "x").unwrap();
        let out = tree_block(root.path());
        assert!(out.contains("src/"), "{out}");
        assert!(!out.contains("README.md"), "{out}");
        assert!(!out.contains("main.rs"), "{out}");
        assert!(out.contains("at session start"), "{out}");
    }

    /// `.git` internals are not project structure, and on this repo they ate
    /// a quarter of the cap.
    #[test]
    fn tree_skips_git_internals() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git").join("objects")).unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        let out = tree_block(root.path());
        assert!(out.contains("src/"), "{out}");
        assert!(!out.contains(".git"), "{out}");
    }

    #[test]
    fn tree_honors_gitignore() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir(root.path().join("ignored")).unwrap();
        std::fs::write(root.path().join("ignored").join("secret.txt"), "x").unwrap();
        assert!(!tree_block(root.path()).contains("ignored"));
    }

    #[test]
    fn toolchains_follow_manifests_not_a_fixed_list() {
        let root = tempfile::tempdir().unwrap();
        // no manifests: nothing declared, nothing probed
        let empty = toolchains(root.path());
        assert!(empty.contains("declared by manifests"), "{empty}");
        assert!(!empty.contains("installed:"), "{empty}");
        std::fs::write(root.path().join("Cargo.toml"), "[package]\n").unwrap();
        let out = toolchains(root.path());
        // cargo/rustc exist wherever tests run (they built this binary)
        assert!(out.contains("installed:") && out.contains("cargo"), "{out}");
        assert!(!out.contains("node"), "{out}");
        assert!(!out.contains("python"), "{out}");
    }

    /// The declared layer reads files and spawns nothing: it is the version
    /// to write code against even on a machine that has no toolchain.
    #[test]
    fn declared_versions_come_from_manifests() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.98.0\"\n",
        )
        .unwrap();
        std::fs::write(root.path().join("go.mod"), "module m\n\ngo 1.22.3\n").unwrap();
        std::fs::write(
            root.path().join("package.json"),
            r#"{"engines":{"node":">=20"}}"#,
        )
        .unwrap();
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[project]\nrequires-python = \">=3.11\"\n",
        )
        .unwrap();
        let out = declared_versions(root.path(), &manifest_paths(root.path()));
        let joined = out.join(", ");
        assert!(joined.contains("rust 1.98.0"), "{joined}");
        assert!(joined.contains("go 1.22.3"), "{joined}");
        assert!(joined.contains("node >=20"), "{joined}");
        assert!(joined.contains("python >=3.11"), "{joined}");
    }

    #[test]
    fn declared_reads_gradle_and_nvmrc_and_dotnet() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".nvmrc"), "v20.11.1\n").unwrap();
        std::fs::create_dir_all(root.path().join("gradle/wrapper")).unwrap();
        std::fs::write(
            root.path().join("gradle/wrapper/gradle-wrapper.properties"),
            "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.11-bin.zip\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("global.json"),
            r#"{"sdk":{"version":"8.0.100"}}"#,
        )
        .unwrap();
        let joined = declared_versions(root.path(), &manifest_paths(root.path())).join(", ");
        assert!(joined.contains("node 20.11.1"), "{joined}");
        assert!(joined.contains("gradle 8.11"), "{joined}");
        assert!(joined.contains("dotnet 8.0.100"), "{joined}");
    }

    /// Coverage (§14.3): the ecosystems the root-only list missed, and the
    /// cap that keeps a polyglot tree from spending the whole budget. The cap
    /// cuts in manifest order, so the sixth ecosystem is honestly absent.
    #[test]
    fn probe_plan_covers_the_manifest_union_and_caps() {
        let mut paths = Vec::new();
        for name in [
            "build.gradle",
            "Gemfile",
            "composer.json",
            "build.zig",
            "CMakeLists.txt",
            "go.mod",
        ] {
            paths.push(PathBuf::from(name));
        }
        let labels: Vec<&str> = probe_plan(&paths).iter().map(|p| p.label).collect();
        assert_eq!(labels.len(), PROBE_CAP, "{labels:?}");
        assert!(labels.contains(&"java"), "{labels:?}");
        assert!(labels.contains(&"ruby"), "{labels:?}");
        assert!(labels.contains(&"php"), "{labels:?}");
        assert!(labels.contains(&"zig"), "{labels:?}");
        // a project with room left probes the C toolchain too
        let cmake = vec![PathBuf::from("CMakeLists.txt"), PathBuf::from("go.mod")];
        let labels: Vec<&str> = probe_plan(&cmake).iter().map(|p| p.label).collect();
        assert_eq!(labels, vec!["go", "cc"], "{labels:?}");
    }

    /// A missing tool and an unfinished probe are different facts; printing
    /// the first as the second teaches the model to distrust the block.
    #[test]
    fn timeout_and_absence_render_differently() {
        assert_eq!(
            render_outcome(&ProbeResult::TimedOut),
            "unknown (probe timeout)"
        );
        assert_eq!(render_outcome(&ProbeResult::Missing), "not installed");
        assert_eq!(
            render_outcome(&ProbeResult::Found("1.98.0".into())),
            "1.98.0"
        );
    }

    #[test]
    fn probe_candidates_prefer_the_shell_native_python() {
        let paths = vec![PathBuf::from("pyproject.toml")];
        let python = probe_plan(&paths)
            .into_iter()
            .find(|p| p.label == "python")
            .expect("python is probed for a pyproject");
        // whichever shell this process runs under, the chain has both spellings
        assert_eq!(python.programs.len(), 2, "{:?}", python.programs);
        assert!(python.programs.contains(&"python"));
        assert!(python.programs.contains(&"python3"));
    }

    #[test]
    fn version_lines_clip_to_the_number() {
        assert_eq!(short_version("rustc 1.98.0 (abc123 2026-09-01)"), "1.98.0");
        assert_eq!(short_version("v20.11.1"), "20.11.1");
        // `go version go1.22.3 windows/amd64` — the number is not first
        assert_eq!(short_version("node v20.11.1"), "20.11.1");
        assert_eq!(short_version("nope"), "nope");
    }
}
