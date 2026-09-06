use ignore::WalkBuilder;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const PROBE_TIMEOUT: Duration = Duration::from_millis(1_500);
const SESSION_PROBE_BUDGET: Duration = Duration::from_secs(2);
const TREE_LIMIT: usize = 40;

/// Facts that cannot change while the process runs: platform, shell, and
/// working directory. This is the first cacheable prompt layer.
pub fn process_block() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let shell = shell_name();
    format!(
        "<process_environment>\nPlatform: {} ({})\nShell: {shell}\nWorking directory: {}\n</process_environment>\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        cwd.display()
    )
}

/// Compatibility name for callers that need only the immutable process facts.
pub fn stable_block() -> String {
    process_block()
}

/// Session-scoped facts, captured on startup and after compaction. They are a
/// cacheable prompt layer: no wall-clock time or per-turn Git status belongs
/// here. Current changes come from the anchor, FACTS, or an explicit tool call.
pub fn session_block(root: &Path) -> String {
    let results = run_session_probes(root.to_path_buf());
    let date = chrono::Local::now().format("%Y-%m-%d");
    let mut out = format!("<session_environment>\nDate: {date}\n");
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
        ("git", ProbeTask::Git(root)),
        ("toolchains", ProbeTask::Toolchains),
    ] {
        jobs += 1;
        let tx = tx.clone();
        thread::spawn(move || {
            let value = match task {
                ProbeTask::Os => os_info(),
                ProbeTask::Git(root) => git_info(&root),
                ProbeTask::Toolchains => toolchains(),
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
    Toolchains,
}

fn shell_name() -> String {
    #[cfg(windows)]
    {
        if std::env::var_os("PSModulePath").is_some()
            || std::env::var_os("POWERSHELL_DISTRIBUTION_CHANNEL").is_some()
        {
            return "PowerShell environment".into();
        }
    }
    std::env::var("SHELL")
        .or_else(|_| std::env::var("COMSPEC"))
        .unwrap_or_else(|_| "unknown".into())
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
            match capture_in(root, "git", &["status", "--porcelain"]) {
                Ok(status) => out.push_str(&format!("Git status at session start: {} changed file(s)\n", status.lines().count())),
                Err(error) => out.push_str(&format!("Git status: unavailable ({error})\n")),
            }
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

/// Shallow, ignored-aware project tree. Directories come before files so a
/// dense root does not hide the project structure.
fn tree_block(root: &Path) -> String {
    let mut entries = Vec::new();
    for entry in WalkBuilder::new(root)
        .standard_filters(true)
        .require_git(false)
        .hidden(false)
        .max_depth(Some(2))
        .build()
        .flatten()
    {
        if entry.depth() == 0 || entry.path().starts_with(root.join(".sqwai")) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
        entries.push((relative.to_path_buf(), is_dir));
    }
    entries.sort_by(|(left_path, left_dir), (right_path, right_dir)| {
        right_dir.cmp(left_dir).then_with(|| left_path.cmp(right_path))
    });

    let omitted = entries.len().saturating_sub(TREE_LIMIT);
    let mut out = String::from("Project tree:\n");
    for (path, is_dir) in entries.into_iter().take(TREE_LIMIT) {
        let depth = path.components().count().saturating_sub(1);
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        out.push_str(&"  ".repeat(depth));
        out.push_str(&name);
        if is_dir {
            out.push('/');
        }
        out.push('\n');
    }
    if omitted > 0 {
        out.push_str(&format!("… +{omitted} more\n"));
    }
    out
}

fn toolchains() -> String {
    let tools = [
        ("rustc", "rustc", vec!["--version"]),
        ("cargo", "cargo", vec!["--version"]),
        ("node", "node", vec!["--version"]),
        ("npm", "npm", vec!["--version"]),
        ("python", "python", vec!["--version"]),
        ("go", "go", vec!["version"]),
    ];
    let (tx, rx) = mpsc::channel();
    for (name, program, args) in tools {
        let tx = tx.clone();
        thread::spawn(move || {
            let value = capture(program, &args)
                .map(|value| value.lines().next().unwrap_or_default().trim().to_string())
                .map_err(|error| error.to_string());
            let _ = tx.send((name, value));
        });
    }
    drop(tx);

    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut versions = BTreeMap::new();
    while versions.len() < 6 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok((name, Ok(value))) if !value.is_empty() => {
                versions.insert(name, value);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    if versions.is_empty() {
        "Toolchains: unavailable\n".into()
    } else {
        let list = versions
            .into_iter()
            .map(|(name, version)| format!("{name} {version}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("Toolchains: {list}\n")
    }
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
    capture_inner(None, program, args)
}

fn capture_in(root: &Path, program: &str, args: &[&str]) -> Result<String, ProbeError> {
    capture_inner(Some(root), program, args)
}

fn capture_inner(root: Option<&Path>, program: &str, args: &[&str]) -> Result<String, ProbeError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
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
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                if text.len() > 4_000 {
                    text.truncate(text.floor_char_boundary(4_000));
                }
                return Ok(text);
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

    #[test]
    fn runtime_context_is_empty_for_history_cache() {
        assert!(runtime_context().is_empty());
    }

    #[test]
    fn tree_marks_omitted_entries() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..(TREE_LIMIT + 3) {
            std::fs::write(root.path().join(format!("file-{index}")), "x").unwrap();
        }
        assert!(tree_block(root.path()).contains("… +3 more"));
    }

    #[test]
    fn tree_honors_gitignore() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir(root.path().join("ignored")).unwrap();
        std::fs::write(root.path().join("ignored").join("secret.txt"), "x").unwrap();
        assert!(!tree_block(root.path()).contains("ignored"));
    }
}
