//! Language-independent project graph indexing.

use super::graph::{Edge, GraphStore, Node, NodeKind};
use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const MAX_INDEX_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphBatch {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexReport {
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub warnings: Vec<String>,
}

pub trait SourceAdapter {
    fn supports(&self, path: &Path) -> bool;
    fn index(&self, relative_path: &str, content: &[u8]) -> Result<GraphBatch>;
}

pub struct MarkdownAdapter;

impl SourceAdapter for MarkdownAdapter {
    fn supports(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some(extension) if extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown")
        )
    }

    fn index(&self, relative_path: &str, content: &[u8]) -> Result<GraphBatch> {
        let text = std::str::from_utf8(content).context("Markdown file is not valid UTF-8")?;
        let source_file_key = file_key(relative_path);
        let document_key = format!("document:{relative_path}");
        let mut batch = GraphBatch::default();
        batch
            .nodes
            .push(file_node(relative_path, content, Some("markdown")));
        batch.nodes.push(Node {
            stable_key: document_key.clone(),
            kind: NodeKind::Document,
            name: file_name(relative_path),
            path: Some(relative_path.to_string()),
            language: Some("markdown".into()),
            line_start: Some(1),
            line_end: Some(text.lines().count().max(1) as u32),
            signature: None,
            properties: properties([("source_adapter", json!("markdown"))]),
            content_hash: Some(content_hash(content)),
        });
        batch.edges.push(edge(
            &source_file_key,
            &document_key,
            "contains",
            "markdown",
        ));

        let headings = markdown_headings(text);
        for (index, heading) in headings.iter().enumerate() {
            let end_line = headings
                .get(index + 1)
                .map_or_else(|| text.lines().count().max(1) as u32, |next| next.line - 1);
            let section_key = format!(
                "section:{relative_path}::{}::{}",
                slug(&heading.title),
                heading.line
            );
            batch.nodes.push(Node {
                stable_key: section_key.clone(),
                kind: NodeKind::Section,
                name: Some(heading.title.clone()),
                path: Some(relative_path.to_string()),
                language: Some("markdown".into()),
                line_start: Some(heading.line),
                line_end: Some(end_line.max(heading.line)),
                signature: Some(format!("h{}", heading.level)),
                properties: properties([
                    ("source_adapter", json!("markdown")),
                    ("heading_level", json!(heading.level)),
                ]),
                content_hash: None,
            });
            batch
                .edges
                .push(edge(&document_key, &section_key, "contains", "markdown"));
        }

        for link in markdown_links(text) {
            let target = normalize_link(relative_path, &link.target);
            let target_key = target
                .as_deref()
                .map(file_key)
                .unwrap_or_else(|| format!("external:{}", link.target));
            let mut relation = edge(&document_key, &target_key, "links_to", "markdown");
            relation
                .properties
                .insert("target".into(), Value::String(link.target));
            relation.properties.insert("line".into(), json!(link.line));
            batch.edges.push(relation);
        }

        Ok(batch)
    }
}

pub fn index_project(store: &mut impl GraphStore, root: &Path) -> Result<IndexReport> {
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize project root {}", root.display()))?;
    let adapter = MarkdownAdapter;
    let mut report = IndexReport::default();

    let mut walker = WalkBuilder::new(&root);
    walker.hidden(true).require_git(false);
    for entry in walker.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report.warnings.push(error.to_string());
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() || is_internal_graph_path(&root, path) {
            continue;
        }
        let relative_path = match relative_path(&root, path) {
            Ok(path) => path,
            Err(error) => {
                report.warnings.push(error.to_string());
                report.skipped_files += 1;
                continue;
            }
        };
        let content = match read_bounded(path) {
            Ok(content) => content,
            Err(error) => {
                report.warnings.push(format!("{relative_path}: {error}"));
                report.skipped_files += 1;
                continue;
            }
        };
        let batch = if adapter.supports(path) {
            match adapter.index(&relative_path, &content) {
                Ok(batch) => batch,
                Err(error) => {
                    report.warnings.push(format!("{relative_path}: {error}"));
                    GraphBatch {
                        nodes: vec![file_node(&relative_path, &content, None)],
                        edges: vec![],
                    }
                }
            }
        } else {
            GraphBatch {
                nodes: vec![file_node(&relative_path, &content, language_for_path(path))],
                edges: vec![],
            }
        };
        store
            .replace_file_subgraph(&relative_path, &batch.nodes, &batch.edges)
            .with_context(|| format!("index {relative_path}"))?;
        report.indexed_files += 1;
    }

    Ok(report)
}

fn file_node(relative_path: &str, content: &[u8], language: Option<&str>) -> Node {
    Node {
        stable_key: file_key(relative_path),
        kind: NodeKind::File,
        name: file_name(relative_path),
        path: Some(relative_path.to_string()),
        language: language.map(str::to_string),
        line_start: None,
        line_end: None,
        signature: None,
        properties: properties([
            ("source_adapter", json!("generic")),
            ("size_bytes", json!(content.len())),
        ]),
        content_hash: Some(content_hash(content)),
    }
}

fn edge(from: &str, to: &str, kind: &str, source: &str) -> Edge {
    Edge {
        from: from.into(),
        to: to.into(),
        kind: kind.into(),
        confidence: Some(100),
        source: Some(source.into()),
        properties: BTreeMap::new(),
    }
}

fn file_key(relative_path: &str) -> String {
    format!("file:{relative_path}")
}

fn file_name(relative_path: &str) -> Option<String> {
    Path::new(relative_path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

fn relative_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("{} is outside project root", path.display()))?;
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("invalid project-relative path {}", relative.display());
    }
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let size = path.metadata()?.len();
    if size > MAX_INDEX_FILE_BYTES {
        bail!("file exceeds {MAX_INDEX_FILE_BYTES} byte indexing limit");
    }
    let mut bytes = Vec::with_capacity(size as usize);
    File::open(path)?.read_to_end(&mut bytes)?;
    if bytes.contains(&0) {
        bail!("binary file skipped");
    }
    Ok(bytes)
}

fn is_internal_graph_path(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|relative| {
        let mut components = relative.components();
        matches!(components.next(), Some(Component::Normal(first)) if first == ".sqwai")
    })
}

fn language_for_path(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "js" | "jsx" => Some("javascript"),
        "ts" | "tsx" => Some("typescript"),
        "go" => Some("go"),
        "java" => Some("java"),
        "c" | "h" => Some("c"),
        "cc" | "cpp" | "cxx" | "hpp" => Some("cpp"),
        "json" => Some("json"),
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yaml"),
        _ => None,
    }
}

fn content_hash(content: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(content))
}

fn properties<const N: usize>(items: [(&str, Value); N]) -> BTreeMap<String, Value> {
    items
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

#[derive(Debug)]
struct Heading {
    level: u8,
    title: String,
    line: u32,
}

fn markdown_headings(text: &str) -> Vec<Heading> {
    let mut headings = Vec::new();
    let mut fenced = false;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let hashes = trimmed
            .chars()
            .take_while(|character| *character == '#')
            .count();
        if !(1..=6).contains(&hashes) || !trimmed[hashes..].starts_with(' ') {
            continue;
        }
        let title = trimmed[hashes..]
            .trim()
            .trim_end_matches('#')
            .trim()
            .to_string();
        if !title.is_empty() {
            headings.push(Heading {
                level: hashes as u8,
                title,
                line: index as u32 + 1,
            });
        }
    }
    headings
}

#[derive(Debug)]
struct MarkdownLink {
    target: String,
    line: u32,
}

fn markdown_links(text: &str) -> Vec<MarkdownLink> {
    let mut links = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        let mut rest = line;
        while let Some(open) = rest.find("](") {
            let after = &rest[open + 2..];
            let Some(close) = after.find(')') else {
                break;
            };
            let target = after[..close]
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches('<')
                .trim_matches('>');
            if !target.is_empty() && !target.starts_with('#') {
                links.push(MarkdownLink {
                    target: target.to_string(),
                    line: line_index as u32 + 1,
                });
            }
            rest = &after[close + 1..];
        }
    }
    links
}

fn normalize_link(source_path: &str, target: &str) -> Option<String> {
    if target.contains("://") || target.starts_with("mailto:") {
        return None;
    }
    let path = target.split('#').next().unwrap_or_default();
    if path.is_empty() {
        return None;
    }
    let parent = Path::new(source_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    normalize_relative(parent.join(path))
}

fn normalize_relative(path: PathBuf) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn slug(title: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in title.chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() || character == '_' {
            if separator && !output.is_empty() {
                output.push('-');
            }
            output.push(character);
            separator = false;
        } else {
            separator = true;
        }
    }
    if output.is_empty() {
        "section".into()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::graph::{CozoGraphStore, Direction, NeighborQuery};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn markdown_adapter_emits_document_sections_and_links() {
        let batch = MarkdownAdapter
            .index(
                "docs/guide.md",
                b"# Guide\n\nSee [API](../API.md#calls).\n\n```md\n# ignored\n```\n\n## Setup\n",
            )
            .unwrap();
        assert_eq!(
            batch
                .nodes
                .iter()
                .filter(|node| node.kind == NodeKind::Section)
                .count(),
            2
        );
        assert!(
            batch
                .nodes
                .iter()
                .any(|node| node.stable_key == "section:docs/guide.md::guide::1")
        );
        assert!(
            batch
                .edges
                .iter()
                .any(|edge| edge.to == "file:API.md" && edge.kind == "links_to")
        );
    }

    #[test]
    fn project_index_respects_gitignore_and_skips_binary_files() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.path().join("README.md"), "# Project\n").unwrap();
        fs::write(dir.path().join("main.py"), "print('ok')\n").unwrap();
        fs::write(dir.path().join("ignored.txt"), "secret\n").unwrap();
        fs::write(dir.path().join("binary.bin"), b"a\0b").unwrap();

        let mut store = CozoGraphStore::open(dir.path()).unwrap();
        let report = index_project(&mut store, dir.path()).unwrap();
        assert_eq!(report.indexed_files, 2);
        assert_eq!(report.skipped_files, 1);
        assert!(store.find_node("file:README.md").unwrap().is_some());
        assert!(store.find_node("file:main.py").unwrap().is_some());
        assert!(store.find_node("file:ignored.txt").unwrap().is_none());
        assert!(
            store
                .find_node("file:.sqwai/graph/graph.db")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reindex_replaces_stale_markdown_sections_atomically() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("README.md");
        fs::write(&path, "# Old\n").unwrap();
        let mut store = CozoGraphStore::open(dir.path()).unwrap();
        index_project(&mut store, dir.path()).unwrap();
        assert!(
            store
                .find_node("section:README.md::old::1")
                .unwrap()
                .is_some()
        );

        fs::write(&path, "# New\n").unwrap();
        index_project(&mut store, dir.path()).unwrap();
        assert!(
            store
                .find_node("section:README.md::old::1")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .find_node("section:README.md::new::1")
                .unwrap()
                .is_some()
        );
        let projection = store
            .neighbors(
                "document:README.md",
                NeighborQuery {
                    direction: Direction::Outgoing,
                    depth: 1,
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(
            projection
                .edges
                .iter()
                .filter(|edge| edge.kind == "contains")
                .count(),
            1
        );
    }

    #[test]
    fn link_normalization_stays_inside_project() {
        assert_eq!(
            normalize_link("docs/a.md", "../README.md#x"),
            Some("README.md".into())
        );
        assert_eq!(normalize_link("a.md", "../outside.md"), None);
        assert_eq!(normalize_link("a.md", "https://example.com"), None);
    }
}
