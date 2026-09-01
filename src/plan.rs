use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const PLAN_DIR: &str = ".sqwai";
const PLAN_FILE: &str = "plan.md";

/// Hidden, project-local planning document used as a durable context anchor.
pub fn path(root: &Path) -> PathBuf {
    root.join(PLAN_DIR).join(PLAN_FILE)
}

pub fn load(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path(root)).ok()?;
    (!text.trim().is_empty()).then_some(text)
}

pub fn update(root: &Path, content: &str, context_limit: u64) -> Result<String> {
    let content = normalize(content)?;
    let approx_tokens = content.len().div_ceil(4) as u64;
    let limit = (context_limit / 10).max(256);
    anyhow::ensure!(
        approx_tokens <= limit,
        "plan is too large (about {approx_tokens} tokens; limit {limit}); rewrite it more compactly"
    );
    let dir = root.join(PLAN_DIR);
    std::fs::create_dir_all(&dir).context("creating .sqwai")?;
    let target = dir.join(PLAN_FILE);
    let tmp = dir.join("plan.md.tmp");
    std::fs::write(&tmp, &content).context("writing plan")?;
    std::fs::rename(&tmp, &target).context("installing plan")?;
    Ok(content)
}

fn normalize(content: &str) -> Result<String> {
    let text = content.trim();
    anyhow::ensure!(!text.is_empty(), "plan cannot be empty");
    anyhow::ensure!(
        text.starts_with("# Task:"),
        "plan must start with '# Task:'"
    );
    Ok(format!("{text}\n"))
}

pub fn todo_items(content: &str) -> Vec<String> {
    content
        .lines()
        .filter(|line| line.trim_start().starts_with("- ["))
        .map(str::trim)
        .filter(|line| {
            line.starts_with("- [ ]") || line.starts_with("- [x]") || line.starts_with("- [X]")
        })
        .map(ToOwned::to_owned)
        .collect()
}

pub fn from_fields(args: &serde_json::Value) -> String {
    let task = args["task"].as_str().unwrap_or("untitled task");
    let status = args["status"].as_str().unwrap_or("in-progress");
    let list = |key: &str| -> String {
        args[key]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    };
    let steps = list("steps");
    let decisions = list("decisions");
    let files = list("files");
    let next = args["next_action"].as_str().unwrap_or("none specified");
    format!(
        "# Task: {task}\n## Status: {status}\n## Steps\n{steps}\n## Decisions & Gotchas\n{decisions}\n## Files touched\n{files}\n## Next immediate action\n{next}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_make_structured_plan() {
        let p = from_fields(&serde_json::json!({
            "task": "ship feature", "status": "in-progress",
            "steps": ["- [x] inspect", "- [ ] implement"],
            "decisions": ["keep it compact"], "files": ["src/plan.rs — store"],
            "next_action": "add tests"
        }));
        assert!(p.contains("# Task: ship feature"));
        assert!(p.contains("## Next immediate action\nadd tests"));
    }

    #[test]
    fn rejects_invalid_plan() {
        assert!(normalize("not markdown").is_err());
    }
}
