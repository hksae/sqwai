//! Host-owned MEMORY.md and USER.md proposal storage.

use crate::agent::secrets::screen;
use anyhow::{Context, Result, bail};
use chrono::Utc;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryWriteResult {
    pub path: PathBuf,
    pub section: &'static str,
    pub op: &'static str,
    pub used_chars: usize,
    pub limit_chars: usize,
}

impl MemoryWriteResult {
    pub fn summary(&self) -> String {
        let pct = if self.limit_chars > 0 {
            (self.used_chars * 100) / self.limit_chars
        } else {
            0
        };
        let mut msg = format!(
            "memory {} in {} [{}] ({}/{} chars, {pct}%)",
            self.op,
            self.path.display(),
            self.section,
            self.used_chars,
            self.limit_chars
        );
        if pct >= 80 {
            msg.push_str(
                " — memory is >80% full; consolidate or delete outdated entries using `replaces`",
            );
        }
        msg
    }
}

/// Strip inline `<!-- ... -->` HTML comments from a single line.
pub fn strip_line_comments(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => {
                rest = &rest[start + 4 + end + 3..];
            }
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim_end().to_string()
}

/// Clean a raw `MEMORY.md` / `USER.md` file for prompt injection:
/// strips `<!-- session ... -->` provenance comments (preserving them on disk
/// for audit) and omits empty `## Section` headings that carry no entries.
/// Returns `None` if no actual facts/entries remain.
pub fn clean_for_prompt(raw: &str) -> Option<String> {
    let mut title: Option<String> = None;
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    let mut current_heading: Option<String> = None;
    let mut current_lines: Vec<String> = Vec::new();
    let mut free_lines: Vec<String> = Vec::new();

    for raw_line in raw.lines() {
        let cleaned = strip_line_comments(raw_line);
        let trimmed = cleaned.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("# ") && title.is_none() && current_heading.is_none() {
            title = Some(trimmed.to_string());
            continue;
        }
        if trimmed.starts_with("## ") {
            if let Some(h) = current_heading.take()
                && !current_lines.is_empty()
            {
                sections.push((h, std::mem::take(&mut current_lines)));
            }
            current_lines.clear();
            current_heading = Some(trimmed.to_string());
            continue;
        }
        if current_heading.is_some() {
            current_lines.push(cleaned);
        } else {
            free_lines.push(cleaned);
        }
    }
    if let Some(h) = current_heading.take()
        && !current_lines.is_empty()
    {
        sections.push((h, current_lines));
    }

    if free_lines.is_empty() && sections.is_empty() {
        return None;
    }

    let mut out = String::new();
    if let Some(t) = title {
        out.push_str(&t);
        out.push('\n');
    }
    if !free_lines.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&free_lines.join("\n"));
        out.push('\n');
    }
    for (heading, lines) in sections {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&heading);
        out.push('\n');
        out.push_str(&lines.join("\n"));
        out.push('\n');
    }
    Some(out.trim().to_string())
}

/// Normalize a bullet or target fragment for storage/comparison: strips inline
/// `<!-- ... -->` provenance comments, collapses internal newlines/whitespace
/// into single spaces (so multi-line text can never inject raw headings or
/// unprefixed orphan lines), and strips leading `- ` / `* ` / `• ` markers.
fn normalize_fact(s: &str) -> String {
    let joined = s
        .lines()
        .map(strip_line_comments)
        .collect::<Vec<_>>()
        .join(" ");
    let collapsed = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed
        .trim()
        .trim_start_matches("- ")
        .trim_start_matches("* ")
        .trim_start_matches("• ")
        .trim();
    trimmed.to_string()
}

fn normalize_for_compare(s: &str) -> String {
    normalize_fact(s)
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{00A0}'
            | '\u{2002}'..='\u{200A}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect::<String>()
        .to_lowercase()
}

/// Apply an approved proposal to the selected host-owned memory file.
///
/// `replaces`, when present, must match an existing text fragment (or bullet).
/// Passing `replaces` with an empty `text` deletes the matched bullet entry.
#[cfg(test)]
pub fn apply_proposal(
    root: &Path,
    scope: Scope,
    section: &str,
    text: &str,
    replaces: Option<&str>,
    session_id: &str,
    max_tokens: u32,
) -> Result<PathBuf> {
    Ok(apply_proposal_with_stats(
        root, scope, section, text, replaces, session_id, max_tokens,
    )?
    .path)
}

pub fn apply_proposal_with_stats(
    root: &Path,
    scope: Scope,
    section: &str,
    text: &str,
    replaces: Option<&str>,
    session_id: &str,
    max_tokens: u32,
) -> Result<MemoryWriteResult> {
    let section = canonical_section(section)?;
    let clean_input = normalize_fact(text);
    let target = replaces
        .map(normalize_fact)
        .filter(|target| !target.is_empty());
    if clean_input.is_empty() && target.is_none() {
        bail!("memory proposal text must not be empty");
    }
    let limit_chars = max_tokens.saturating_mul(4) as usize;
    if clean_input.chars().count() > limit_chars {
        bail!("memory proposal exceeds the configured memory limit");
    }
    let screened = screen(&clean_input).text;
    let path = path(root, scope)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {} memory directory", scope.label()))?;
    }
    let old = fs::read_to_string(&path).unwrap_or_else(|_| template(scope));
    let mut content = ensure_sections(&old);
    let provenance = format!("<!-- session {session_id} {} -->", Utc::now().date_naive());
    let replacement = format!("{screened} {provenance}");

    let op = if let Some(target_str) = target.as_deref() {
        if clean_input.is_empty() {
            // Delete / forget obsolete entry
            delete_matching_fact(&mut content, section, target_str)?;
            "deleted"
        } else {
            // Update / replace existing entry cleanly without leaving old provenance comments
            replace_matching_fact(&mut content, section, target_str, &screened, &provenance)?;
            "updated"
        }
    } else if replace_duplicate_in_section(&mut content, section, &screened, &provenance) {
        "updated"
    } else {
        append_bullet_to_section(&mut content, section, &replacement)?;
        "added"
    };

    content = ensure_sections(&content);
    let used_chars = clean_for_prompt(&content)
        .map(|s| s.chars().count())
        .unwrap_or(0);
    if used_chars > limit_chars {
        bail!("memory file exceeds the configured memory limit");
    }
    atomic_write(&path, &content)?;
    Ok(MemoryWriteResult {
        path,
        section,
        op,
        used_chars,
        limit_chars,
    })
}

fn append_bullet_to_section(content: &mut String, section: &str, replacement: &str) -> Result<()> {
    let Some((body_start, end)) = section_byte_range(content, section) else {
        bail!("memory section is missing: {section}");
    };
    let slice = content[body_start..end].trim_end();
    let updated_slice = if slice.is_empty() {
        format!("\n- {replacement}\n")
    } else {
        format!("{slice}\n- {replacement}\n")
    };
    content.replace_range(body_start..end, &updated_slice);
    Ok(())
}

fn section_byte_range(content: &str, section: &str) -> Option<(usize, usize)> {
    let heading = format!("## {section}");
    let start = content.find(&heading)?;
    let body_start = start + heading.len();
    let end = content[body_start..]
        .find("\n## ")
        .map(|offset| body_start + offset)
        .unwrap_or(content.len());
    Some((body_start, end))
}

fn section_entries_hint(content: &str, section: &str) -> String {
    let mut entries = Vec::new();
    if let Some((body_start, end)) = section_byte_range(content, section) {
        for line in content[body_start..end].lines() {
            let clean = normalize_fact(line);
            if !clean.is_empty() {
                entries.push(format!("  - {clean}"));
            }
        }
    }
    if entries.is_empty() {
        format!("(section [{section}] is currently empty)")
    } else {
        format!("Current entries in [{section}]:\n{}", entries.join("\n"))
    }
}

/// If `screened` already exists (case/typographic-insensitive) as a bullet in
/// `section`, refresh its line and provenance in-place. If it exists in
/// another section, remove the old copy and move it into `section`.
fn replace_duplicate_in_section(
    content: &mut String,
    section: &str,
    screened: &str,
    provenance: &str,
) -> bool {
    let want = normalize_for_compare(screened);
    if want.is_empty() {
        return false;
    }
    // 1. Check target `section` first
    if let Some((body_start, end)) = section_byte_range(content, section) {
        let slice = &content[body_start..end];
        let mut new_lines: Vec<String> = Vec::new();
        let mut matched = false;
        for line in slice.split('\n') {
            if !matched && line.trim_start().starts_with("- ") {
                if normalize_for_compare(line) == want {
                    new_lines.push(format!("- {screened} {provenance}"));
                    matched = true;
                    continue;
                }
            }
            new_lines.push(line.to_string());
        }
        if matched {
            content.replace_range(body_start..end, &new_lines.join("\n"));
            return true;
        }
    }
    // 2. Check other sections: if found, remove from old section and append to `section`
    let lines: Vec<&str> = content.lines().collect();
    if let Some(idx) = lines
        .iter()
        .position(|l| l.trim_start().starts_with("- ") && normalize_for_compare(l) == want)
    {
        let mut rebuilt: Vec<String> = lines.iter().map(|&s| s.to_string()).collect();
        rebuilt.remove(idx);
        *content = ensure_sections(&rebuilt.join("\n"));
        let _ = append_bullet_to_section(content, section, &format!("{screened} {provenance}"));
        return true;
    }
    false
}

fn find_matching_bullet_indices(
    lines: &[&str],
    preferred_section: &str,
    target_str: &str,
) -> Vec<usize> {
    let want = normalize_for_compare(target_str);
    if want.is_empty() {
        return Vec::new();
    }
    // Pass 1: exact normalized bullet match (preferred section first, then all sections)
    for check_all_sections in [false, true] {
        let mut current_sec = "";
        let mut hits = Vec::new();
        for (idx, &line) in lines.iter().enumerate() {
            if let Some(sec) = line.trim().strip_prefix("## ") {
                current_sec = sec.trim();
                continue;
            }
            if line.trim().starts_with('#') {
                continue;
            }
            if !check_all_sections && !current_sec.eq_ignore_ascii_case(preferred_section) {
                continue;
            }
            let clean = normalize_for_compare(line);
            if !clean.is_empty() && clean == want {
                hits.push(idx);
            }
        }
        if !hits.is_empty() {
            return hits;
        }
    }
    // Pass 2: case/typographic-insensitive substring match against whole bullets
    // (preferred section first, then all sections)
    for check_all_sections in [false, true] {
        let mut current_sec = "";
        let mut hits = Vec::new();
        for (idx, &line) in lines.iter().enumerate() {
            if let Some(sec) = line.trim().strip_prefix("## ") {
                current_sec = sec.trim();
                continue;
            }
            if line.trim().starts_with('#') {
                continue;
            }
            if !check_all_sections && !current_sec.eq_ignore_ascii_case(preferred_section) {
                continue;
            }
            let clean = normalize_for_compare(line);
            if !clean.is_empty() && clean.contains(&want) {
                hits.push(idx);
            }
        }
        if !hits.is_empty() {
            return hits;
        }
    }
    Vec::new()
}

/// Replace an entire bullet matching `target_str` with `- {screened} {provenance}`.
fn replace_matching_fact(
    content: &mut String,
    preferred_section: &str,
    target_str: &str,
    screened: &str,
    provenance: &str,
) -> Result<()> {
    let lines: Vec<&str> = content.lines().collect();
    let hits = find_matching_bullet_indices(&lines, preferred_section, target_str);
    match hits.as_slice() {
        [] => bail!(
            "memory replacement text was not found in [{preferred_section}]. {}",
            section_entries_hint(content, preferred_section)
        ),
        [idx] => {
            let mut rebuilt: Vec<String> = lines.iter().map(|&s| s.to_string()).collect();
            rebuilt[*idx] = format!("- {screened} {provenance}");
            *content = rebuilt.join("\n") + "\n";
            Ok(())
        }
        many => {
            let candidates: Vec<String> = many
                .iter()
                .map(|&i| format!("  - {}", normalize_fact(lines[i])))
                .collect();
            bail!(
                "memory replacement text matches {} entries; pass a more specific `replaces` string:\n{}",
                many.len(),
                candidates.join("\n")
            )
        }
    }
}

/// Delete a bullet line matching `target_str` when `text` is empty.
fn delete_matching_fact(
    content: &mut String,
    preferred_section: &str,
    target_str: &str,
) -> Result<()> {
    let lines: Vec<&str> = content.lines().collect();
    let hits = find_matching_bullet_indices(&lines, preferred_section, target_str);
    match hits.as_slice() {
        [] => bail!(
            "memory replacement text was not found in [{preferred_section}]. {}",
            section_entries_hint(content, preferred_section)
        ),
        [idx] => {
            let mut rebuilt: Vec<String> = lines.iter().map(|&s| s.to_string()).collect();
            rebuilt.remove(*idx);
            *content = ensure_sections(&rebuilt.join("\n"));
            Ok(())
        }
        many => {
            let candidates: Vec<String> = many
                .iter()
                .map(|&i| format!("  - {}", normalize_fact(lines[i])))
                .collect();
            bail!(
                "memory replacement text matches {} entries; pass a more specific `replaces` string:\n{}",
                many.len(),
                candidates.join("\n")
            )
        }
    }
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
        assert!(text.contains("Use cargo fmt <!-- session session-1 "));
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

    #[test]
    fn replace_strips_old_provenance_and_delete_removes_entry_and_clean_for_prompt_omits_metadata()
    {
        let dir = root();
        // 1. Insert first fact
        let res1 = apply_proposal_with_stats(
            &dir,
            Scope::Project,
            "Conventions",
            "Use cargo fmt",
            None,
            "session-1",
            3000,
        )
        .unwrap();
        assert_eq!(res1.op, "added");

        // 2. Duplicate insert without `replaces` updates in-place instead of duplicating
        let res_dup = apply_proposal_with_stats(
            &dir,
            Scope::Project,
            "Conventions",
            "- Use cargo fmt",
            None,
            "session-2",
            3000,
        )
        .unwrap();
        assert_eq!(res_dup.op, "updated");
        let on_disk_dup = fs::read_to_string(&res_dup.path).unwrap();
        assert_eq!(
            on_disk_dup.matches("Use cargo fmt").count(),
            1,
            "must not duplicate identical bullet: {on_disk_dup}"
        );
        assert!(
            !on_disk_dup.contains("session-1"),
            "old provenance comment must be replaced, not stacked: {on_disk_dup}"
        );

        // 3. Replace with new text (passing leading `- ` in `replaces`)
        let res_upd = apply_proposal_with_stats(
            &dir,
            Scope::Project,
            "Conventions",
            "Use cargo fmt --all",
            Some("- Use cargo fmt"),
            "session-3",
            3000,
        )
        .unwrap();
        assert_eq!(res_upd.op, "updated");
        let on_disk_upd = fs::read_to_string(&res_upd.path).unwrap();
        assert!(on_disk_upd.contains("- Use cargo fmt --all <!-- session session-3 "));
        assert!(
            !on_disk_upd.contains("session-2"),
            "must not leave dangling provenance comments: {on_disk_upd}"
        );

        // 4. `clean_for_prompt` strips HTML provenance comments and empty sections
        let cleaned = clean_for_prompt(&on_disk_upd).expect("non-empty prompt memory");
        assert!(cleaned.contains("## Conventions\n- Use cargo fmt --all"));
        assert!(!cleaned.contains("<!--"), "comments must be stripped: {cleaned}");
        assert!(
            !cleaned.contains("## Agreements"),
            "empty sections must be omitted: {cleaned}"
        );

        // 5. Delete via empty `text` + `replaces`
        let res_del = apply_proposal_with_stats(
            &dir,
            Scope::Project,
            "Conventions",
            "",
            Some("Use cargo fmt --all"),
            "session-4",
            3000,
        )
        .unwrap();
        assert_eq!(res_del.op, "deleted");
        assert_eq!(res_del.used_chars, 0);
        let on_disk_del = fs::read_to_string(&res_del.path).unwrap();
        assert!(
            clean_for_prompt(&on_disk_del).is_none(),
            "empty template after deleting last entry should produce None for prompt"
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn multiline_collapse_whole_bullet_replace_cross_section_dedup_and_miss_hint() {
        let dir = root();
        // 1. Multi-line text collapses to one atomic bullet line
        apply_proposal_with_stats(
            &dir,
            Scope::Project,
            "Project",
            "Use cargo check\nand targeted cargo test",
            None,
            "s1",
            3000,
        )
        .unwrap();
        let disk1 = fs::read_to_string(project_path(&dir)).unwrap();
        assert!(
            disk1.contains("- Use cargo check and targeted cargo test <!-- session s1 "),
            "{disk1}"
        );

        // 2. Cross-section duplicate moves/updates instead of duplicating across sections
        let res_move = apply_proposal_with_stats(
            &dir,
            Scope::Project,
            "Conventions",
            "Use cargo check and targeted cargo test",
            None,
            "s2",
            3000,
        )
        .unwrap();
        assert_eq!(res_move.op, "updated");
        let disk2 = fs::read_to_string(project_path(&dir)).unwrap();
        assert_eq!(
            disk2.matches("Use cargo check and targeted cargo test").count(),
            1,
            "must not duplicate across sections: {disk2}"
        );

        // 3. Substring `replaces` replaces the WHOLE bullet (not mid-sentence splice)
        apply_proposal_with_stats(
            &dir,
            Scope::Project,
            "Conventions",
            "Always run cargo check before committing",
            Some("Use cargo check"),
            "s3",
            3000,
        )
        .unwrap();
        let disk3 = fs::read_to_string(project_path(&dir)).unwrap();
        assert!(
            disk3.contains("- Always run cargo check before committing <!-- session s3 "),
            "whole bullet must be replaced, not spliced: {disk3}"
        );
        assert!(!disk3.contains("targeted cargo test"), "{disk3}");

        // 4. Missing `replaces` lists current entries in the section
        let miss = apply_proposal_with_stats(
            &dir,
            Scope::Project,
            "Conventions",
            "New text",
            Some("non_existent_entry"),
            "s4",
            3000,
        )
        .unwrap_err()
        .to_string();
        assert!(miss.contains("Current entries in [Conventions]:"), "{miss}");
        assert!(miss.contains("Always run cargo check before committing"), "{miss}");

        fs::remove_dir_all(dir).ok();
    }
}
