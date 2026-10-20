//! Loading compatible `SKILL.md` instruction files and progressive skill disclosure.
//!
//! Skills are structured instruction packages (`<dir>/SKILL.md` plus optional
//! bundled `references/`, `scripts/`, `assets/` files). Project skills override
//! user skills with the same name.
//!
//! Two-tier delivery keeps the prompt cache intact:
//! 1. [`catalog_prompt`] renders a compact `<available_skills>` index (`name`,
//!    `scope`, `description`, `triggers`, `globs`) in the cached system prefix,
//!    expanding full bodies only for skills marked `always: true`.
//! 2. [`active_prompt`] / [`render_skill_detail`] renders full `SKILL.md`
//!    instructions and bundled file manifests on demand when activated via the
//!    `skill` tool, `/skill <name>`, `/skills` TUI toggle, or `$skill-name`
//!    mentions.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::SkillsConfig;

pub const MAX_SKILL_BYTES: usize = 64 * 1024;
const MAX_TOTAL_BYTES: usize = 512 * 1024;
const MAX_BUNDLED_FILES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkillScope {
    User,
    Project,
}

impl SkillScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: Option<String>,
    pub triggers: Vec<String>,
    pub globs: Vec<String>,
    pub always: bool,
    pub scope: SkillScope,
    pub body: String,
    pub path: PathBuf,
    pub files: Vec<String>,
}

impl Skill {
    pub fn char_len(&self) -> usize {
        self.body.chars().count()
    }
}

/// Discover all skills from configured directories, user skill roots, and
/// project skill roots (`.agents/skills` and `.sqwai/skills`).
/// Later directories override an earlier skill with the same name.
pub fn discover(config: &SkillsConfig, project_root: &Path) -> Vec<Skill> {
    let mut roots: Vec<(PathBuf, SkillScope)> = config
        .dirs
        .iter()
        .cloned()
        .map(|d| (d, SkillScope::User))
        .collect();

    #[cfg(not(test))]
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        let home = PathBuf::from(home);
        roots.push((home.join(".agents/skills"), SkillScope::User));
        roots.push((home.join(".config/sqwai/skills"), SkillScope::User));
    }

    roots.push((project_root.join(".agents/skills"), SkillScope::Project));
    roots.push((project_root.join(".sqwai/skills"), SkillScope::Project));

    let mut by_name = BTreeMap::new();
    let mut total = 0usize;
    for (dir, scope) in roots {
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
            let Some(skill) = parse_with_scope(&raw, path, scope) else {
                continue;
            };
            total += size;
            by_name.insert(skill.name.to_ascii_lowercase(), skill);
        }
    }
    by_name.into_values().collect()
}

/// Match a query against an already-discovered catalog: exact name or trigger
/// first, then a prefix/substring fallback. Borrows the catalog so a hot path
/// (a keystroke) never re-scans the skill directories.
pub fn filter_by_query(all: &[Skill], query: &str) -> Vec<Skill> {
    let q_lower = query.to_ascii_lowercase();
    let exact: Vec<Skill> = all
        .iter()
        .filter(|skill| {
            skill.name.eq_ignore_ascii_case(query)
                || skill
                    .triggers
                    .iter()
                    .any(|trigger| trigger.eq_ignore_ascii_case(query))
        })
        .cloned()
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    all.iter()
        .filter(|skill| {
            skill.name.to_ascii_lowercase().contains(&q_lower)
                || skill
                    .triggers
                    .iter()
                    .any(|t| t.to_ascii_lowercase().contains(&q_lower))
        })
        .cloned()
        .collect()
}

/// Render the cached `<available_skills>` catalog for `stable_parts`
/// (`PrefixSection::Skills`).
///
/// Only lightweight metadata (`name`, `scope`, `description`, `triggers`,
/// `globs`, bundled file count) is emitted for normal skills; full bodies are
/// included here only for skills explicitly marked `always: true`.
pub fn catalog_prompt(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut out = String::from(
        "<available_skills>\n\
         Use the `skill` tool (`skill(name: \"...\")` or `skill(name: \"...\", file: \"...\")`) \
         to load a skill's full instructions or bundled files when the task matches its \
         description, triggers, or file globs.\n",
    );
    for skill in skills {
        out.push_str("<skill name=\"");
        out.push_str(&escape_attr(&skill.name));
        out.push_str("\" scope=\"");
        out.push_str(skill.scope.as_str());
        out.push('"');
        if !skill.triggers.is_empty() {
            out.push_str(" triggers=\"");
            out.push_str(&escape_attr(&skill.triggers.join(", ")));
            out.push('"');
        }
        if !skill.globs.is_empty() {
            out.push_str(" globs=\"");
            out.push_str(&escape_attr(&skill.globs.join(", ")));
            out.push('"');
        }
        if !skill.files.is_empty() {
            out.push_str(&format!(" files=\"{}\"", skill.files.len()));
        }
        if skill.always {
            out.push_str(" always=\"true\"");
        }
        out.push_str(">\n");
        if let Some(desc) = &skill.description {
            out.push_str("Description: ");
            out.push_str(desc);
            out.push('\n');
        }
        if skill.always {
            out.push_str(&skill.body);
            out.push('\n');
        }
        out.push_str("</skill>\n");
    }
    out.push_str("</available_skills>");
    Some(out)
}

/// Render full skill bodies for active/pinned skills (`<skills>...</skills>`).
pub fn prompt(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut out = String::from("<skills>\n");
    for skill in skills {
        out.push_str("<skill name=\"");
        out.push_str(&escape_attr(&skill.name));
        out.push_str("\" scope=\"");
        out.push_str(skill.scope.as_str());
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
        if !skill.globs.is_empty() {
            out.push_str("Globs: ");
            out.push_str(&skill.globs.join(", "));
            out.push('\n');
        }
        if !skill.files.is_empty() {
            out.push_str("Bundled files: ");
            out.push_str(&skill.files.join(", "));
            out.push('\n');
        }
        out.push_str(&skill.body);
        out.push_str("\n</skill>\n");
    }
    out.push_str("</skills>");
    Some(out)
}

/// Format a single skill's full payload for the `skill` tool response or TUI viewer.
pub fn render_skill_detail(skill: &Skill) -> String {
    let mut out = format!(
        "<skill name=\"{}\" scope=\"{}\">\n",
        escape_attr(&skill.name),
        skill.scope.as_str()
    );
    if let Some(desc) = &skill.description {
        out.push_str(&format!("Description: {desc}\n"));
    }
    if !skill.triggers.is_empty() {
        out.push_str(&format!("Triggers: {}\n", skill.triggers.join(", ")));
    }
    if !skill.globs.is_empty() {
        out.push_str(&format!("Globs: {}\n", skill.globs.join(", ")));
    }
    if !skill.files.is_empty() {
        out.push_str("Bundled files (read with `skill(name: \"");
        out.push_str(&skill.name);
        out.push_str("\", file: \"<path>\")`): ");
        out.push_str(&skill.files.join(", "));
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&skill.body);
    out.push_str("\n</skill>");
    out
}

/// Safely read a supporting file inside a skill's directory (`references/...`,
/// `scripts/...`, `assets/...`), enforcing a strict canonical path jail inside
/// the skill directory.
pub fn read_skill_file(skill: &Skill, rel_path: &str) -> Result<String, String> {
    let clean = rel_path.trim().replace('\\', "/");
    let clean = clean.trim_start_matches("./");
    if clean.is_empty() {
        return Err("file path inside skill cannot be empty".into());
    }
    if clean.starts_with('/')
        || clean.contains("..")
        || clean.split('/').any(|seg| seg.is_empty() || seg == "..")
    {
        return Err(format!(
            "invalid skill file path `{rel_path}`: must be a relative path inside the skill directory"
        ));
    }
    let Some(skill_dir) = skill.path.parent() else {
        return Err("skill has no parent directory".into());
    };
    let dir_canon = skill_dir
        .canonicalize()
        .map_err(|e| format!("cannot resolve skill directory: {e}"))?;
    let target = skill_dir.join(clean);
    let target_canon = target.canonicalize().map_err(|_| {
        if skill.files.is_empty() {
            format!(
                "file `{clean}` not found in skill `{}` (this skill has no bundled files)",
                skill.name
            )
        } else {
            format!(
                "file `{clean}` not found in skill `{}` (available files: {})",
                skill.name,
                skill.files.join(", ")
            )
        }
    })?;
    if !target_canon.starts_with(&dir_canon) {
        return Err(format!(
            "refused: `{clean}` escapes skill directory for `{}`",
            skill.name
        ));
    }
    let meta = fs::metadata(&target_canon).map_err(|e| format!("cannot stat `{clean}`: {e}"))?;
    if !meta.is_file() {
        return Err(format!("`{clean}` is not a regular file"));
    }
    if meta.len() as usize > MAX_SKILL_BYTES {
        return Err(format!(
            "`{clean}` exceeds maximum skill file size ({} bytes > {} bytes)",
            meta.len(),
            MAX_SKILL_BYTES
        ));
    }
    let raw = fs::read_to_string(&target_canon)
        .map_err(|e| format!("cannot read `{clean}` as UTF-8 text: {e}"))?;
    Ok(crate::agent::secrets::screen(&raw).text)
}

/// Detect `$skill-name` inline mentions in a user prompt and return the
/// matching skills from `available`.
pub fn extract_dollar_mentions(available: &[Skill], text: &str) -> Vec<Skill> {
    if available.is_empty() || !text.contains('$') {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();
    for raw_tok in text.split_whitespace() {
        let tok = raw_tok.trim_matches(|c: char| {
            !c.is_ascii_alphanumeric() && c != '$' && c != '-' && c != '_'
        });
        let Some(name) = tok.strip_prefix('$') else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        if let Some(skill) = available
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
            && seen.insert(skill.name.to_ascii_lowercase())
        {
            found.push(skill.clone());
        }
    }
    found
}

/// Build a non-intrusive 1-line hint when the user's message or touched files
/// match an inactive skill's `triggers` or `globs`.
pub fn match_hints(
    available: &[Skill],
    active_names: &[String],
    user_text: &str,
    touched_paths: &[String],
) -> Option<String> {
    if available.is_empty() {
        return None;
    }
    let lower_text = user_text.to_ascii_lowercase();
    let mut matched = Vec::new();
    for skill in available {
        if skill.always
            || active_names
                .iter()
                .any(|a| a.eq_ignore_ascii_case(&skill.name))
        {
            continue;
        }
        let trigger_hit = !lower_text.is_empty()
            && skill.triggers.iter().any(|trig| {
                let t = trig.trim().to_ascii_lowercase();
                t.len() >= 3 && contains_phrase(&lower_text, &t)
            });
        let glob_hit = !touched_paths.is_empty()
            && skill.globs.iter().any(|g| {
                touched_paths
                    .iter()
                    .any(|p| glob_matches(g, &p.replace('\\', "/")))
            });
        if trigger_hit || glob_hit {
            matched.push(skill.name.clone());
            if matched.len() >= 3 {
                break;
            }
        }
    }
    if matched.is_empty() {
        None
    } else {
        Some(format!(
            "[skill hint: matching skill(s) available: {} — call `skill(name: \"...\")` if relevant]",
            matched.join(", ")
        ))
    }
}

fn contains_phrase(haystack: &str, needle: &str) -> bool {
    for (idx, _) in haystack.match_indices(needle) {
        let before_ok = haystack[..idx]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        let after_idx = idx + needle.len();
        let after_ok = haystack[after_idx..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

/// Lightweight glob matcher supporting `*`, `**`, and `?` on `/` normalized paths.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    let pat = pattern.trim().replace('\\', "/");
    let pat = pat.trim_start_matches("./");
    let path = path.trim().replace('\\', "/");
    let path = path.trim_start_matches("./");
    if pat.is_empty() {
        return false;
    }
    // If pattern has no slash, match against either full path or basename
    if !pat.contains('/') {
        let base = path.rsplit('/').next().unwrap_or(path);
        return wildcard_match(pat.as_bytes(), base.as_bytes())
            || wildcard_match(pat.as_bytes(), path.as_bytes());
    }
    wildcard_match(pat.as_bytes(), path.as_bytes())
}

fn wildcard_match(pat: &[u8], text: &[u8]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None::<usize>, 0usize);
    let mut star_is_double = false;

    while ti < text.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi].eq_ignore_ascii_case(&text[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            let is_double = pi + 1 < pat.len() && pat[pi + 1] == b'*';
            star_pi = Some(pi);
            star_ti = ti;
            star_is_double = is_double;
            pi += if is_double { 2 } else { 1 };
            if is_double && pi < pat.len() && pat[pi] == b'/' {
                pi += 1;
            }
        } else if let Some(sp) = star_pi {
            if !star_is_double && text[star_ti] == b'/' {
                return false;
            }
            star_ti += 1;
            ti = star_ti;
            pi = sp + if star_is_double { 2 } else { 1 };
            if star_is_double && pi < pat.len() && pat[pi] == b'/' {
                pi += 1;
            }
        } else {
            return false;
        }
    }
    while pi < pat.len() && (pat[pi] == b'*' || pat[pi] == b'/') {
        pi += 1;
    }
    pi == pat.len()
}

#[cfg(test)]
pub fn parse(raw: &str, path: PathBuf) -> Option<Skill> {
    parse_with_scope(raw, path, SkillScope::User)
}

pub fn parse_with_scope(raw: &str, path: PathBuf, scope: SkillScope) -> Option<Skill> {
    let normalized = raw.replace("\r\n", "\n");
    let (front, body) = normalized.strip_prefix("---\n")?.split_once("\n---")?;
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut triggers: Vec<String> = Vec::new();
    let mut globs: Vec<String> = Vec::new();
    let mut always = false;

    let lines: Vec<&str> = front.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        let Some((key, raw_val)) = line.split_once(':') else {
            i += 1;
            continue;
        };
        let key = key.trim();
        let val = raw_val.trim();

        match key {
            "name" => {
                let v = unquote(val);
                if !v.is_empty() {
                    name = Some(v.to_string());
                }
                i += 1;
            }
            "description" => {
                if val.is_empty() || matches!(val, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
                    let fold = val.starts_with('>');
                    let mut block = Vec::new();
                    i += 1;
                    while i < lines.len() {
                        let next = lines[i];
                        if next.starts_with(' ') || next.starts_with('\t') {
                            block.push(next.trim());
                            i += 1;
                        } else if next.trim().is_empty() {
                            block.push("");
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    let joined = if fold {
                        block
                            .into_iter()
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join(" ")
                    } else {
                        block.join("\n").trim().to_string()
                    };
                    if !joined.is_empty() {
                        description = Some(joined);
                    }
                } else {
                    let mut parts = vec![unquote(val).to_string()];
                    i += 1;
                    while i < lines.len() {
                        let next = lines[i];
                        if (next.starts_with("  ") || next.starts_with('\t'))
                            && !next.trim_start().starts_with("- ")
                            && !next.contains(':')
                        {
                            parts.push(next.trim().to_string());
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    let joined = parts.join(" ").trim().to_string();
                    if !joined.is_empty() {
                        description = Some(joined);
                    }
                }
            }
            "triggers" | "when_to_use" => {
                let (items, next_i) = parse_yaml_list(val, &lines, i + 1);
                triggers.extend(items);
                i = next_i;
            }
            "globs" | "paths" => {
                let (items, next_i) = parse_yaml_list(val, &lines, i + 1);
                globs.extend(items);
                i = next_i;
            }
            "always" | "alwaysApply" | "always_apply" => {
                let v = unquote(val).to_ascii_lowercase();
                always = matches!(v.as_str(), "true" | "yes" | "1");
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    // Also extract inline `Triggers: a, b, c` from the end of description if
    // triggers list wasn't explicitly set in frontmatter (common in Claude/OpenCode skills).
    if triggers.is_empty()
        && let Some(desc) = &description
        && let Some(pos) = desc.find("Triggers:")
    {
        let tail = &desc[pos + "Triggers:".len()..];
        triggers = tail
            .split(',')
            .map(|s| s.trim().trim_end_matches('.').to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    let name = name.or_else(|| path.parent()?.file_name()?.to_str().map(str::to_string))?;
    let raw_body = body.trim_start_matches(['\r', '\n']).trim();
    if raw_body.is_empty() {
        return None;
    }
    let screened_body = crate::agent::secrets::screen(raw_body).text;
    let screened_desc = description.map(|d| crate::agent::secrets::screen(&d).text);
    let files = path
        .parent()
        .map(list_skill_files)
        .unwrap_or_default();

    Some(Skill {
        name,
        description: screened_desc,
        triggers,
        globs,
        always,
        scope,
        body: screened_body,
        path,
        files,
    })
}

fn unquote(s: &str) -> &str {
    s.trim().trim_matches(['"', '\''])
}

fn parse_yaml_list(inline_val: &str, lines: &[&str], mut next_i: usize) -> (Vec<String>, usize) {
    if !inline_val.is_empty() {
        let items = inline_val
            .trim_matches(['[', ']'])
            .split(',')
            .map(|item| unquote(item))
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect();
        return (items, next_i);
    }
    let mut items = Vec::new();
    while next_i < lines.len() {
        let line = lines[next_i];
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- ") {
            let v = unquote(rest);
            if !v.is_empty() {
                items.push(v.to_string());
            }
            next_i += 1;
        } else if trimmed.is_empty() {
            next_i += 1;
        } else {
            break;
        }
    }
    (items, next_i)
}

fn list_skill_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    collect_files_rec(root, root, 0, &mut out);
    out.sort();
    out.truncate(MAX_BUNDLED_FILES);
    out
}

fn collect_files_rec(base: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > 3 || out.len() >= MAX_BUNDLED_FILES {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if out.len() >= MAX_BUNDLED_FILES {
            break;
        }
        let Some(fname) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if fname.starts_with('.')
            || matches!(fname, "SKILL.md" | "node_modules" | "target" | "__pycache__")
        {
            continue;
        }
        if p.is_dir() {
            collect_files_rec(base, &p, depth + 1, out);
        } else if p.is_file()
            && let Ok(rel) = p.strip_prefix(base)
            && let Some(rel_str) = rel.to_str()
        {
            out.push(rel_str.replace('\\', "/"));
        }
    }
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
            "---\nname: rust\ndescription: Rust rules\ntriggers: [rust, cargo]\nglobs: [\"*.rs\", \"src/tui/**\"]\n---\nUse cargo test.",
            PathBuf::from("rust/SKILL.md"),
        )
        .unwrap();
        assert_eq!(skill.name, "rust");
        assert_eq!(skill.triggers, ["rust", "cargo"]);
        assert_eq!(skill.globs, ["*.rs", "src/tui/**"]);
        let rendered = prompt(&[skill.clone()]).unwrap();
        assert!(rendered.contains("Description: Rust rules"));
        assert!(rendered.contains("Use cargo test."));

        let cat = catalog_prompt(&[skill]).unwrap();
        assert!(cat.contains("<available_skills>"));
        assert!(cat.contains("Description: Rust rules"));
        assert!(!cat.contains("Use cargo test."));
    }

    #[test]
    fn parses_multiline_yaml_and_block_lists() {
        let raw = concat!(
            "---\n",
            "name: motion\n",
            "description: |\n",
            "  First line of description.\n",
            "  Second line of description.\n",
            "triggers:\n",
            "  - animate\n",
            "  - motion graphic\n",
            "globs:\n",
            "  - src/tui/*.rs\n",
            "always: true\n",
            "---\n",
            "Always animate composite properties.\n",
        );
        let skill = parse_with_scope(raw, PathBuf::from("motion/SKILL.md"), SkillScope::Project).unwrap();
        assert_eq!(skill.name, "motion");
        assert_eq!(skill.scope, SkillScope::Project);
        assert!(skill.always);
        assert_eq!(
            skill.description.as_deref(),
            Some("First line of description.\nSecond line of description.")
        );
        assert_eq!(skill.triggers, ["animate", "motion graphic"]);
        assert_eq!(skill.globs, ["src/tui/*.rs"]);

        // `always: true` skills include their body in `catalog_prompt`
        let cat = catalog_prompt(&[skill]).unwrap();
        assert!(cat.contains("Always animate composite properties."));
    }

    #[test]
    fn glob_and_trigger_hints_match_accurately() {
        let s1 = parse(
            "---\nname: tui-rules\ndescription: TUI layout\ntriggers: [ratatui, terminal ui]\nglobs: [src/tui/**]\n---\nKeep rows within width.",
            PathBuf::from("tui/SKILL.md"),
        )
        .unwrap();
        let available = vec![s1];

        let h1 = match_hints(&available, &[], "let's fix the ratatui layout", &[]);
        assert!(h1.unwrap().contains("tui-rules"));

        let h2 = match_hints(&available, &[], "edit file", &["src/tui/app/mod.rs".into()]);
        assert!(h2.unwrap().contains("tui-rules"));

        // Active skill is not hinted again
        let h3 = match_hints(
            &available,
            &["tui-rules".into()],
            "let's fix the ratatui layout",
            &["src/tui/app/mod.rs".into()],
        );
        assert!(h3.is_none());
    }

    #[test]
    fn extracts_dollar_skill_mentions() {
        let s1 = parse(
            "---\nname: apple-design\ndescription: Apple UI\n---\nUse springs.",
            PathBuf::from("apple-design/SKILL.md"),
        )
        .unwrap();
        let hits = extract_dollar_mentions(&[s1], "please apply $apple-design to this panel");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "apple-design");
    }

    #[test]
    fn rejects_missing_frontmatter() {
        assert!(parse("# no frontmatter", PathBuf::from("x/SKILL.md")).is_none());
    }

    #[test]
    fn parses_crlf_frontmatter() {
        let skill = parse(
            "---\r\nname: win\r\ndescription: Windows rules\r\ntriggers: [win, ps]\r\n---\r\nUse powershell.\r\n",
            PathBuf::from("win/SKILL.md"),
        )
        .unwrap();
        assert_eq!(skill.name, "win");
        assert_eq!(skill.description.as_deref(), Some("Windows rules"));
        assert_eq!(skill.triggers, ["win", "ps"]);
        assert!(skill.body.contains("Use powershell."));
    }
}
