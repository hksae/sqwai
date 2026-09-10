//! Language-independent project graph indexing.

use super::graph::{Edge, GraphStore, Node, NodeKind, Occurrence};
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
    pub occurrences: Vec<Occurrence>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexReport {
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub removed_files: usize,
    pub warnings: Vec<String>,
}

pub trait SourceAdapter {
    fn supports(&self, path: &Path) -> bool;
    fn index(&self, relative_path: &str, content: &[u8]) -> Result<GraphBatch>;
}

/// Bump when an adapter's output changes shape; a bump should trigger a
/// full reindex (§2.4.4).
pub const GENERIC_ADAPTER_VERSION: &str = "1";
pub const MARKDOWN_ADAPTER_VERSION: &str = "1";

/// §2.4.4 Level 1: a file node plus `references` edges for path-like
/// mentions (imports and relative paths) that resolve inside the project.
/// The indexer drops mentions that point outside the walked file set.
pub struct GenericAdapter;

/// Path-like tokens: must contain a separator (`/` or `\`) or start with
/// `./` / `../`, so bare words like `cargo` never match. URLs are excluded
/// by the `://` check in the caller.
fn path_mentions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // token characters for a path mention
        let is_token = |c: char| c.is_alphanumeric() || matches!(c, '.' | '/' | '\\' | '-' | '_');
        if !is_token(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_token(chars[i]) {
            i += 1;
        }
        let token: String = chars[start..i].iter().collect();
        // a path mention has a separator, never a scheme, and a known extension
        let has_sep = token.contains('/') || token.starts_with("./") || token.starts_with("../");
        let has_ext = token.rsplit('.').next().is_some_and(|ext| {
            matches!(
                ext,
                "rs" | "py"
                    | "js"
                    | "jsx"
                    | "mjs"
                    | "cjs"
                    | "ts"
                    | "tsx"
                    | "go"
                    | "java"
                    | "c"
                    | "h"
                    | "cc"
                    | "cpp"
                    | "cxx"
                    | "hpp"
                    | "json"
                    | "toml"
                    | "yaml"
                    | "yml"
                    | "md"
                    | "markdown"
                    | "sh"
            )
        });
        if has_sep && !token.contains("://") && has_ext {
            out.push(token);
        }
    }
    out
}

impl SourceAdapter for GenericAdapter {
    fn supports(&self, _path: &Path) -> bool {
        true
    }

    fn index(&self, relative_path: &str, content: &[u8]) -> Result<GraphBatch> {
        let mut batch = GraphBatch::default();
        let language = language_for_path(Path::new(relative_path));
        batch.nodes.push(file_node(
            relative_path,
            content,
            language,
            "generic",
            GENERIC_ADAPTER_VERSION,
            &[],
        ));
        let text = String::from_utf8_lossy(content);
        for mention in path_mentions(&text) {
            // `./` and `../` are relative to the file's directory; everything
            // else is treated as project-root relative (the usual shape of
            // module and include paths)
            let joined = if mention.starts_with("./") || mention.starts_with("../") {
                Path::new(relative_path)
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .join(&mention)
            } else {
                PathBuf::from(&mention)
            };
            let target = match normalize_relative(joined) {
                Some(target) => target,
                None => continue,
            };
            if target == relative_path {
                continue;
            }
            let mut relation = edge(
                &file_key(relative_path),
                &file_key(&target),
                "references",
                "generic",
            );
            relation
                .properties
                .insert("mention".into(), Value::String(mention));
            batch.edges.push(relation);
        }
        Ok(batch)
    }
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
        batch.nodes.push(file_node(
            relative_path,
            content,
            Some("markdown"),
            "markdown",
            MARKDOWN_ADAPTER_VERSION,
            &["declarations"],
        ));
        batch.nodes.push(Node {
            stable_key: document_key.clone(),
            kind: NodeKind::Document,
            name: file_name(relative_path),
            path: Some(relative_path.to_string()),
            language: Some("markdown".into()),
            line_start: Some(1),
            line_end: Some(text.lines().count().max(1) as u32),
            signature: None,
            roles: Vec::new(),
            properties: properties([
                ("source_adapter", json!("markdown")),
                ("adapter_version", json!(MARKDOWN_ADAPTER_VERSION)),
            ]),
            content_hash: Some(content_hash(content)),
        });
        batch.edges.push(edge(
            &source_file_key,
            &document_key,
            "contains",
            "markdown",
        ));

        let headings = markdown_headings(text);
        let mut slug_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for (index, heading) in headings.iter().enumerate() {
            let end_line = headings
                .get(index + 1)
                .map_or_else(|| text.lines().count().max(1) as u32, |next| next.line - 1);
            // deterministic key: `section:<path>#<slug>`, `-2` on collision —
            // stable under line shifts (§2.4.3)
            let base = slug(&heading.title);
            let count = slug_counts.entry(base.clone()).or_insert(0);
            *count += 1;
            let section_key = if *count == 1 {
                format!("section:{relative_path}#{base}")
            } else {
                format!("section:{relative_path}#{base}-{count}")
            };
            batch.nodes.push(Node {
                stable_key: section_key.clone(),
                kind: NodeKind::Section,
                name: Some(heading.title.clone()),
                path: Some(relative_path.to_string()),
                language: Some("markdown".into()),
                line_start: Some(heading.line),
                line_end: Some(end_line.max(heading.line)),
                signature: Some(format!("h{}", heading.level)),
                roles: Vec::new(),
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

/// Index the project, skipping paths that match `exclude_globs`.
///
/// §2.3.6 keeps credential files out of the index: their contents would land
/// in graph node properties and in the FTS table, which is durable state the
/// screening in `agent::secrets` never sees.
///
/// Two passes: the first collects the walked file set so that Level 1
/// `references` edges can be resolved against it — an adapter never needs to
/// know what else exists, and no dangling `file:` edges are stored.
pub fn index_project_excluding(
    store: &mut impl GraphStore,
    root: &Path,
    exclude_globs: &[String],
) -> Result<IndexReport> {
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize project root {}", root.display()))?;
    let excluded = build_globset(exclude_globs);
    let markdown = MarkdownAdapter;
    let generic = GenericAdapter;
    let mut report = IndexReport::default();

    let mut collect = WalkBuilder::new(&root);
    collect.hidden(true).require_git(false);
    let mut retained_paths = std::collections::BTreeSet::new();
    for entry in collect.build().flatten() {
        let path = entry.path();
        if !path.is_file() || is_internal_graph_path(&root, path) {
            continue;
        }
        if let Some(excluded) = &excluded {
            let name = path.file_name().map(Path::new).unwrap_or(path);
            let relative = path.strip_prefix(&root).unwrap_or(path);
            if excluded.is_match(name) || excluded.is_match(relative) {
                report.skipped_files += 1;
                continue;
            }
        }
        if let Ok(relative) = relative_path(&root, path) {
            retained_paths.insert(relative);
        }
    }
    let resolve_edge = |edge: &Edge| {
        !(edge.to.starts_with("file:") && !retained_paths.contains(&edge.to["file:".len()..]))
    };

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
        if let Some(excluded) = &excluded {
            let name = path.file_name().map(Path::new).unwrap_or(path);
            let relative = path.strip_prefix(&root).unwrap_or(path);
            if excluded.is_match(name) || excluded.is_match(relative) {
                continue; // counted in the first pass
            }
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
        let mut batch = if markdown.supports(path) {
            match markdown.index(&relative_path, &content) {
                Ok(batch) => batch,
                Err(error) => {
                    report.warnings.push(format!("{relative_path}: {error}"));
                    GraphBatch {
                        nodes: vec![file_node(
                            &relative_path,
                            &content,
                            None,
                            "generic",
                            GENERIC_ADAPTER_VERSION,
                            &[],
                        )],
                        edges: vec![],
                        occurrences: vec![],
                    }
                }
            }
        } else {
            match generic.index(&relative_path, &content) {
                Ok(batch) => batch,
                Err(error) => {
                    report.warnings.push(format!("{relative_path}: {error}"));
                    GraphBatch {
                        nodes: vec![file_node(
                            &relative_path,
                            &content,
                            None,
                            "generic",
                            GENERIC_ADAPTER_VERSION,
                            &[],
                        )],
                        edges: vec![],
                        occurrences: vec![],
                    }
                }
            }
        };
        batch.edges.retain(resolve_edge);
        store
            .replace_file_subgraph(
                &relative_path,
                &batch.nodes,
                &batch.edges,
                &batch.occurrences,
            )
            .with_context(|| format!("index {relative_path}"))?;
        report.indexed_files += 1;
    }

    report.removed_files = store.prune_file_subgraphs(&retained_paths)?;
    Ok(report)
}

/// Full rebuild with §2.4.2 atomicity: index into `graph.db.new`, then swap
/// it over `graph.db` on success, so a half-built graph is never published.
pub fn rebuild_project(root: &Path) -> Result<IndexReport> {
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize project root {}", root.display()))?;
    let graph_dir = root.join(".sqwai").join("graph");
    std::fs::create_dir_all(&graph_dir)
        .with_context(|| format!("create graph directory {}", graph_dir.display()))?;
    let new_db = graph_dir.join("graph.db.new");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", new_db.display())));
    }

    let report = {
        let mut store = super::graph::SqliteGraphStore::open_unmanaged(&new_db, root.clone())
            .context("open fresh graph database for rebuild")?;
        // bump before indexing so every row of this generation is stamped
        // with the generation being published
        store.bump_generation().context("bump graph generation")?;
        index_project_excluding(&mut store, &root, &secret_exclude_globs())?
    };

    let db_path = graph_dir.join("graph.db");
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
    }
    // the old database is only removed once the new one is fully built
    let _ = std::fs::remove_file(&db_path);
    std::fs::rename(&new_db, &db_path)
        .with_context(|| format!("publish rebuilt graph {}", db_path.display()))?;
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", new_db.display())));
    }

    let previous = super::graph::read_meta(&graph_dir);
    super::graph::write_meta(
        &graph_dir,
        &super::graph::GraphMeta {
            schema_version: super::graph::GRAPH_SCHEMA_VERSION,
            generation: previous
                .as_ref()
                .map(|meta| meta.generation + 1)
                .unwrap_or(1),
            built_at: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or_default(),
            ),
            status: super::graph::GraphStatus::Ok,
        },
    )?;
    Ok(report)
}

fn secret_exclude_globs() -> Vec<String> {
    crate::config::SecretsConfig::default().exclude_globs
}

fn file_node(
    relative_path: &str,
    content: &[u8],
    language: Option<&str>,
    adapter: &str,
    adapter_version: &str,
    capabilities: &[&str],
) -> Node {
    Node {
        stable_key: file_key(relative_path),
        kind: NodeKind::File,
        name: file_name(relative_path),
        path: Some(relative_path.to_string()),
        language: language.map(str::to_string),
        line_start: None,
        line_end: None,
        signature: None,
        roles: Vec::new(),
        properties: properties([
            ("source_adapter", json!(adapter)),
            (
                "capabilities",
                Value::Array(
                    capabilities
                        .iter()
                        .map(|c| Value::String(c.to_string()))
                        .collect(),
                ),
            ),
            ("adapter_version", json!(adapter_version)),
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
        source_hash: None,
        limitations: Vec::new(),
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

/// Compile the exclude patterns, ignoring ones that do not parse rather than
/// failing the whole index for a typo in config.
fn build_globset(patterns: &[String]) -> Option<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    let mut any = false;
    for pattern in patterns {
        if let Ok(glob) = globset::Glob::new(pattern) {
            builder.add(glob);
            any = true;
        }
    }
    any.then(|| builder.build().ok()).flatten()
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
    use crate::agent::graph::{Direction, NeighborQuery, SqliteGraphStore};
    use std::fs;
    use tempfile::tempdir;

    /// Incremental indexing with the default secret exclusions.
    fn index_project(store: &mut impl GraphStore, root: &Path) -> Result<IndexReport> {
        index_project_excluding(store, root, &secret_exclude_globs())
    }

    /// §2.3.6: credential files stay out of the index. Their contents would
    /// land in node properties and the FTS table, which is durable state the
    /// screening in `agent::secrets` never sees.
    #[test]
    fn excluded_globs_are_not_indexed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "# ok\n").unwrap();
        std::fs::write(dir.path().join(".env"), "ANTHROPIC_API_KEY=sk-secret\n").unwrap();
        std::fs::write(
            dir.path().join("server.pem"),
            "-----BEGIN PRIVATE KEY-----\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("id_rsa"), "private\n").unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("config/app_secret.toml"), "token = 1\n").unwrap();

        let mut store = SqliteGraphStore::open(dir.path()).unwrap();
        let globs = crate::config::SecretsConfig::default().exclude_globs;
        index_project_excluding(&mut store, dir.path(), &globs).unwrap();

        assert!(
            store.find_node("file:README.md").unwrap().is_some(),
            "an ordinary file is still indexed"
        );
        for excluded in [
            "file:.env",
            "file:server.pem",
            "file:id_rsa",
            "file:config/app_secret.toml",
        ] {
            assert!(
                store.find_node(excluded).unwrap().is_none(),
                "{excluded} must not be indexed"
            );
        }
    }

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
                .any(|node| node.stable_key == "section:docs/guide.md#guide")
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

        let mut store = SqliteGraphStore::open(dir.path()).unwrap();
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
        let mut store = SqliteGraphStore::open(dir.path()).unwrap();
        index_project(&mut store, dir.path()).unwrap();
        assert!(store.find_node("section:README.md#old").unwrap().is_some());

        fs::write(&path, "# New\n").unwrap();
        index_project(&mut store, dir.path()).unwrap();
        assert!(store.find_node("section:README.md#old").unwrap().is_none());
        assert!(store.find_node("section:README.md#new").unwrap().is_some());
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
    fn full_reindex_removes_deleted_file_subgraphs() {
        let dir = tempdir().unwrap();
        let stale_path = dir.path().join("stale.md");
        fs::write(&stale_path, "# Stale\n").unwrap();
        let mut store = SqliteGraphStore::open(dir.path()).unwrap();
        index_project(&mut store, dir.path()).unwrap();
        assert!(store.find_node("file:stale.md").unwrap().is_some());

        fs::remove_file(stale_path).unwrap();
        let report = index_project(&mut store, dir.path()).unwrap();
        assert_eq!(report.removed_files, 1);
        assert!(store.find_node("file:stale.md").unwrap().is_none());
        assert!(store.find_node("document:stale.md").unwrap().is_none());
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

    #[test]
    fn generic_adapter_level1_emits_reference_edges() {
        let batch = GenericAdapter
            .index(
                "src/main.rs",
                b"include!(\"src/lib.rs\");\n// docs live in docs/guide.md\nfn main() {}\n",
            )
            .unwrap();
        let mentions: Vec<_> = batch
            .edges
            .iter()
            .filter(|edge| edge.kind == "references")
            .map(|edge| edge.to.as_str())
            .collect();
        assert!(mentions.contains(&"file:src/lib.rs"), "{mentions:?}");
        assert!(mentions.contains(&"file:docs/guide.md"), "{mentions:?}");
        // bare words and schemes never become edges
        assert!(!mentions.iter().any(|target| !target.starts_with("file:")));
    }

    #[test]
    fn reference_edges_only_target_walked_files() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "include!(\"src/b.rs\");\n").unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/b.rs"), "pub fn b() {}\n").unwrap();
        let mut store = SqliteGraphStore::open(dir.path()).unwrap();
        index_project(&mut store, dir.path()).unwrap();

        let projection = store
            .neighbors(
                "file:a.rs",
                NeighborQuery {
                    direction: Direction::Outgoing,
                    depth: 1,
                    limit: 10,
                },
            )
            .unwrap();
        assert!(
            projection
                .edges
                .iter()
                .any(|edge| edge.kind == "references" && edge.to == "file:src/b.rs"),
            "{:?}",
            projection.edges
        );
        // a mention of a file that does not exist is dropped by the indexer
        fs::write(dir.path().join("c.rs"), "// see src/ghost.rs\n").unwrap();
        index_project(&mut store, dir.path()).unwrap();
        let projection = store
            .neighbors(
                "file:c.rs",
                NeighborQuery {
                    direction: Direction::Outgoing,
                    depth: 1,
                    limit: 10,
                },
            )
            .unwrap();
        assert!(
            !projection
                .edges
                .iter()
                .any(|edge| edge.to == "file:src/ghost.rs"),
            "{:?}",
            projection.edges
        );
    }

    #[test]
    fn full_rebuild_publishes_database_atomically() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "# Hello\n").unwrap();

        let report = rebuild_project(dir.path()).unwrap();
        assert_eq!(report.indexed_files, 1);
        let graph_dir = dir.path().join(".sqwai/graph");
        assert!(graph_dir.join("graph.db").exists());
        assert!(!graph_dir.join("graph.db.new").exists());
        let meta = crate::agent::graph::read_meta(&graph_dir).unwrap();
        assert_eq!(meta.status, crate::agent::graph::GraphStatus::Ok);
        assert_eq!(meta.generation, 1);

        fs::write(dir.path().join("README.md"), "# Changed\n").unwrap();
        rebuild_project(dir.path()).unwrap();
        let meta = crate::agent::graph::read_meta(&graph_dir).unwrap();
        assert_eq!(meta.generation, 2);
        let store = SqliteGraphStore::open(dir.path()).unwrap();
        assert!(
            store
                .find_node("section:README.md#changed")
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .find_node("section:README.md#hello")
                .unwrap()
                .is_none()
        );
    }
}
