use std::process::{Command, Stdio};

/// Facts that cannot change while the process runs: OS, shell, working
/// directory. Part of the **stable prefix** — byte-identical between requests,
/// so a provider-side prefix cache survives across turns.
pub fn stable_block() -> String {
    let mut s = String::from("<environment>\n");

    s.push_str(&format!(
        "\nOS: {} {} ({})\n",
        std::env::consts::OS,
        windows_version(),
        std::env::consts::ARCH
    ));
    s.push_str(&format!(
        "Shell: {}\n",
        std::env::var("SHELL")
            .or_else(|_| std::env::var("COMSPEC"))
            .unwrap_or_else(|_| "unknown".into())
    ));

    let cwd = std::env::current_dir().unwrap_or_default();
    s.push_str(&format!("Working directory: {}\n", cwd.display()));

    s.push_str("</environment>\n");
    s
}

/// Facts that change while the agent works: current date, git state, project
/// tree. Captured once per user turn and appended **after** the stable prefix,
/// so a new commit or a changed clock cannot invalidate the cached prefix.
///
/// Every probe fails silently.
pub fn volatile_block() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut s = String::from("<runtime_context>\n");
    let now = chrono::Local::now();
    s.push_str(&format!(
        "Date: {} (UTC {})\n",
        now.format("%Y-%m-%d %H:%M %A"),
        chrono::Utc::now().format("%Y-%m-%d %H:%M")
    ));
    s.push_str(&git_info());
    s.push_str(&tree_block(&cwd));
    s.push_str("</runtime_context>\n");
    s
}

fn windows_version() -> String {
    #[cfg(windows)]
    {
        let key = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion";
        let product = capture("reg", &["query", key, "/v", "ProductName"], 1500);
        let build = capture("reg", &["query", key, "/v", "CurrentBuildNumber"], 1500)
            .and_then(|v| value_from_reg(&v, "CurrentBuildNumber"))
            .and_then(|v| v.parse::<u32>().ok());
        if let Some(product) = product.and_then(|v| value_from_reg(&v, "ProductName")) {
            // Windows 11 keeps the legacy ProductName value "Windows 10".
            if build.is_some_and(|n| n >= 22_000) {
                return product.replacen("Windows 10", "Windows 11", 1);
            }
            return product;
        }
        "Windows".into()
    }
    #[cfg(not(windows))]
    {
        String::new()
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

fn git_info() -> String {
    let mut s = String::new();
    let branch = capture("git", &["rev-parse", "--abbrev-ref", "HEAD"], 2000);
    match branch {
        Some(b) => {
            s.push_str(&format!("Git: on branch {}\n", b.trim()));
            if let Some(st) = capture("git", &["status", "--porcelain"], 3000) {
                let n = st.lines().count();
                s.push_str(&format!("Git status: {n} changed file(s)\n"));
            }
            if let Some(log) = capture("git", &["log", "-3", "--format=- %s"], 2000) {
                s.push_str(&format!("Recent commits:\n{}\n", log.trim_end()));
            }
        }
        None => s.push_str("Git: not a repository\n"),
    }
    s
}

/// shallow project tree so the model knows where things live
fn tree_block(cwd: &std::path::Path) -> String {
    const SKIP: [&str; 6] = [
        ".git",
        "target",
        "node_modules",
        "dist",
        ".sqwai",
        ".vscode",
    ];
    let mut lines: Vec<String> = Vec::new();
    fn walk(dir: &std::path::Path, depth: usize, out: &mut Vec<String>, skip: &[&str; 6]) {
        if depth > 2 || out.len() > 40 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if skip.contains(&name.as_str()) {
                continue;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let indent = "  ".repeat(depth);
            if is_dir {
                out.push(format!("{indent}{name}/"));
                walk(&e.path(), depth + 1, out, skip);
            } else if out.len() < 38 {
                out.push(format!("{indent}{name}"));
            }
        }
    }
    walk(cwd, 0, &mut lines, &SKIP);
    let mut s = format!("Project tree:\n");
    for l in lines.into_iter().take(42) {
        s.push_str(&format!("{l}\n"));
    }
    s
}

#[allow(dead_code)]
fn toolchains() -> String {
    let mut parts: Vec<String> = Vec::new();
    for (name, prog, args) in [
        ("rustc", "rustc", vec!["--version"]),
        ("cargo", "cargo", vec!["--version"]),
        ("node", "node", vec!["--version"]),
        ("npm", "npm", vec!["--version"]),
        ("python", "python", vec!["--version"]),
        ("go", "go", vec!["version"]),
    ] {
        if let Some(v) = capture(prog, &args, 2500) {
            parts.push(format!(
                "{} {}",
                name,
                v.lines().next().unwrap_or("").trim()
            ));
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("Toolchains: {}\n", parts.join(", "))
    }
}

/// run a command capturing stdout, bounded in time; None on any failure
fn capture(prog: &str, args: &[&str], _timeout_ms: u64) -> Option<String> {
    let out = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() && out.stdout.is_empty() {
        return None;
    }
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    if s.len() > 4000 {
        s.truncate(s.floor_char_boundary(4000));
    }
    Some(s)
}
