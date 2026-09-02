//! Loading compatible `SKILL.md` instruction files.
//!
//! Skills are prompt extensions only in this phase: they never execute code.
//! Project skills override user skills with the same name.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::SkillsConfig;

const MAX_SKILL_BYTES: usize = 64 * 1024;
const MAX_TOTAL_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: Option<String>,
    pub triggers: Vec<String>,
    pub body: String,
    pub path: PathBuf,
}

/// Load skills from configured directories, plus the conventional project dir.
/// Later directories override an earlier skill with the same name.
pub fn load(config: &SkillsConfig, project_root: &Path) -> Vec<Skill> {
    if !config.auto_load {
        return Vec::new();
    }
    let mut dirs = config.dirs.clone();
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        dirs.push(PathBuf::from(home).join(".config/sqwai/skills"));
    }
    dirs.push(project_root.join(".sqwai/skills"));

    let mut by_name = BTreeMap::new();
    let mut total = 0usize;
    for dir in dirs {
        let mut paths = Vec::new();
        if dir.join("SKILL.md").is_file() {
            paths.push(dir.join("SKILL.md"));
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        paths.extend(
            entries
                .filter_map(Result::ok)
                .map(|e| e.path().join("SKILL.md"))
                .filter(|p| p.is_file()),
        );
        paths.sort();
        for path in paths {
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            let size = meta.len() as usize;
            if size == 0 || size > MAX_SKILL_BYTES || total.saturating_add(size) > MAX_TOTAL_BYTES {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let Some(skill) = parse(&raw, path) else {
                continue;
            };
            total += size;
            by_name.insert(skill.name.clone(), skill);
        }
    }
    by_name.into_values().collect()
}

/// Render loaded skills as a stable system-prompt section.
pub fn prompt(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut out = String::from("<skills>\n");
    for skill in skills {
        out.push_str("<skill name=\"");
        out.push_str(&escape_attr(&skill.name));
        out.push_str("\">\n");
        if let Some(desc) = &skill.description {
            out.push_str("Description: ");
            out.push_str(desc);
            out.push('\n');
        }
        if !skill.triggers.is_empty() {
            out.push_str("Triggers: ");
            out.push_str(&skill.triggers.join(", "));
            out.push('\n');
        }
        out.push_str(&skill.body);
        out.push_str("\n</skill>\n");
    }
    out.push_str("</skills>");
    Some(out)
}

fn parse(raw: &str, path: PathBuf) -> Option<Skill> {
    let (front, body) = raw.strip_prefix("---\n")?.split_once("\n---")?;
    let mut name = None;
    let mut description = None;
    let mut triggers = Vec::new();
    for line in front.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']);
        match key.trim() {
            "name" => name = (!value.is_empty()).then(|| value.to_string()),
            "description" => description = (!value.is_empty()).then(|| value.to_string()),
            "triggers" => {
                triggers = value
                    .trim_matches(['[', ']'])
                    .split(',')
                    .map(|item| item.trim().trim_matches(['"', '\'']))
                    .filter(|item| !item.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            _ => {}
        }
    }
    let name = name.or_else(|| path.parent()?.file_name()?.to_str().map(str::to_string))?;
    let body = body.trim_start_matches(['\r', '\n']).trim().to_string();
    (!body.is_empty()).then_some(Skill {
        name,
        description,
        triggers,
        body,
        path,
    })
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_renders_prompt() {
        let skill = parse(
            "---\nname: rust\ndescription: Rust rules\ntriggers: [rust, cargo]\n---\nUse cargo test.",
            PathBuf::from("rust/SKILL.md"),
        )
        .unwrap();
        assert_eq!(skill.name, "rust");
        assert_eq!(skill.triggers, ["rust", "cargo"]);
        let rendered = prompt(&[skill]).unwrap();
        assert!(rendered.contains("Description: Rust rules"));
        assert!(rendered.contains("Use cargo test."));
    }

    #[test]
    fn rejects_missing_frontmatter() {
        assert!(parse("# no frontmatter", PathBuf::from("x/SKILL.md")).is_none());
    }
}
