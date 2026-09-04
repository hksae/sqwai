//! Host-owned MEMORY.md and USER.md proposal storage (DESIGN §2.3.5).

use crate::agent::diary::screen;
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

const SECTIONS: [&str; 4] = ["Project", "Conventions", "User", "Agreements"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project,
    User,
}

impl Scope {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "project" | "" => Ok(Self::Project),
            "user" => Ok(Self::User),
            _ => Err("scope must be project or user".into()),
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

pub fn project_path(root: &Path) -> PathBuf {
    root.join(".sqwai").join("memory").join("MEMORY.md")
}

pub fn user_path() -> Result<PathBuf> {
    Ok(crate::config::config_dir()?.join("USER.md"))
}

pub fn path(root: &Path, scope: Scope) -> Result<PathBuf> {
    match scope {
        Scope::Project => Ok(project_path(root)),
        Scope::User => user_path(),
    }
}

/// Apply an approved proposal to the selected host-owned memory file.
///
/// `replaces`, when present, must match an existing exact text fragment. An
/// absent match is rejected instead of silently appending a duplicate fact.
pub fn apply_proposal(
    root: &Path,
    scope: Scope,
    section: &str,
    text: &str,
    replaces: Option<&str>,
    session_id: &str,
    max_tokens: u32,
) -> Result<PathBuf> {
    let section = canonical_section(section)?;
    let text = text.trim();
    if text.is_empty() {
        bail!("memory proposal text must not be empty");
    }
    if text.chars().count() > max_tokens.saturating_mul(4) as usize {
        bail!("memory proposal exceeds the configured memory limit");
    }
    let screened = screen(text).text;
    let path = path(root, scope)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {} memory directory", scope.label()))?;
    }
    let old = fs::read_to_string(&path).unwrap_or_else(|_| template(scope));
    let mut content = ensure_sections(&old);
    let provenance = format!("<!-- session {session_id} -->");
    let replacement = format!("{screened} {provenance}");
    if let Some(target) = replaces.map(str::trim).filter(|target| !target.is_empty()) {
        if !content.contains(target) {
            bail!("memory replacement text was not found");
        }
        content = content.replacen(target, &replacement, 1);
    } else {
        let heading = format!("## {section}");
        let start = content
            .find(&heading)
            .ok_or_else(|| anyhow::anyhow!("memory section is missing: {section}"))?;
        let body_start = start + heading.len();
        let end = content[body_start..]
            .find("\n## ")
            .map(|offset| body_start + offset)
            .unwrap_or(content.len());
        let entry = format!("\n- {replacement}\n");
        content.insert_str(end, &entry);
    }
    atomic_write(&path, &content)?;
    Ok(path)
}

fn canonical_section(section: &str) -> Result<&'static str> {
    let value = section.trim();
    SECTIONS
        .iter()
        .copied()
        .find(|candidate| candidate.eq_ignore_ascii_case(value))
        .ok_or_else(|| anyhow::anyhow!("section must be Project, Conventions, User, or Agreements"))
}

fn template(scope: Scope) -> String {
    match scope {
        Scope::Project => {
            "# Project memory\n\n## Project\n\n## Conventions\n\n## User\n\n## Agreements\n".into()
        }
        Scope::User => {
            "# User memory\n\n## Project\n\n## Conventions\n\n## User\n\n## Agreements\n".into()
        }
    }
}

fn ensure_sections(input: &str) -> String {
    let mut output = input.trim_end().to_string();
    for section in SECTIONS {
        if !output
            .lines()
            .any(|line| line.trim() == format!("## {section}"))
        {
            output.push_str(&format!("\n\n## {section}\n"));
        }
    }
    output.push('\n');
    output
}

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("MEMORY.md");
    let temp = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    fs::write(&temp, content)
        .with_context(|| format!("writing temporary memory {}", temp.display()))?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error).with_context(|| format!("replacing memory {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "sqwai-memory-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn approved_project_proposal_creates_section_and_provenance() {
        let dir = root();
        let path = apply_proposal(
            &dir,
            Scope::Project,
            "Conventions",
            "Use cargo fmt",
            None,
            "session-1",
            3000,
        )
        .unwrap();
        let text = fs::read_to_string(path).unwrap();
        assert!(text.contains("## Conventions"));
        assert!(text.contains("Use cargo fmt <!-- session session-1 -->"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn replacement_must_match_existing_text() {
        let dir = root();
        let error = apply_proposal(
            &dir,
            Scope::Project,
            "Project",
            "new",
            Some("missing"),
            "session-1",
            3000,
        )
        .unwrap_err();
        assert!(error.to_string().contains("not found"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn invalid_scope_and_section_are_rejected() {
        assert!(Scope::parse("team").is_err());
        assert!(canonical_section("Other").is_err());
    }
}
