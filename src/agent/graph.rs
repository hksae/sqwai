//! Project knowledge graph foundation.
//!
//! The backend is intentionally hidden behind a small universal model. Indexers,
//! agent tools, and UI projections must not depend on CozoDB-specific APIs.

use anyhow::{Context, Result, anyhow, bail};
use cozo::{DataValue, DbInstance, MultiTransaction, ScriptMutability};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

pub const GRAPH_SCHEMA_VERSION: u32 = 1;
const MAX_QUERY_DEPTH: u8 = 8;
const MAX_QUERY_RESULTS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    File,
    Folder,
    Document,
    Section,
    Module,
    Namespace,
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Interface,
    Trait,
    Variable,
    Constant,
    Type,
    Macro,
    Test,
    Memory,
    Decision,
    Commit,
    Branch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub stable_key: String,
    pub kind: NodeKind,
    pub name: Option<String>,
    pub path: Option<String>,
    pub language: Option<String>,
    pub line_start: Option<u32>,
    pub line_end: Option<u32>,
    pub signature: Option<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, Value>,
    pub content_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub confidence: Option<u8>,
    pub source: Option<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outgoing,
    Incoming,
    Both,
}

#[derive(Debug, Clone)]
pub struct NeighborQuery {
    pub direction: Direction,
    pub depth: u8,
    pub limit: usize,
}

impl Default for NeighborQuery {
    fn default() -> Self {
        Self {
            direction: Direction::Both,
            depth: 1,
            limit: 50,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphProjection {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub truncated: bool,
}

pub trait GraphStore {
    fn schema_version(&self) -> Result<u32>;
    fn upsert_node(&mut self, node: &Node) -> Result<()>;
    fn upsert_edge(&mut self, edge: &Edge) -> Result<()>;
    fn apply_batch(&mut self, nodes: &[Node], edges: &[Edge]) -> Result<()>;
    fn replace_file_subgraph(&mut self, path: &str, nodes: &[Node], edges: &[Edge]) -> Result<()>;
    fn prune_file_subgraphs(&mut self, retained_paths: &BTreeSet<String>) -> Result<usize>;
    fn find_node(&self, stable_key: &str) -> Result<Option<Node>>;
    fn neighbors(&self, stable_key: &str, query: NeighborQuery) -> Result<GraphProjection>;
}

pub struct CozoGraphStore {
    db: DbInstance,
    project_root: PathBuf,
}

impl CozoGraphStore {
    pub fn open(project_root: impl Into<PathBuf>) -> Result<Self> {
        let project_root = project_root.into();
        let graph_dir = project_root.join(".sqwai").join("graph");
        std::fs::create_dir_all(&graph_dir)
            .with_context(|| format!("create graph directory {}", graph_dir.display()))?;
        let db_path = graph_dir.join("graph.db");
        let db = DbInstance::new("sqlite", &db_path, "{}")
            .map_err(|error| anyhow!(error.to_string()))
            .with_context(|| format!("open graph database {}", db_path.display()))?;
        let store = Self { db, project_root };
        store.initialize_schema()?;
        Ok(store)
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    fn initialize_schema(&self) -> Result<()> {
        self.run_mut(
            r#"
%ignore_error { :create graph_meta {key: String => value: Int} }
%ignore_error { :create graph_nodes {stable_key: String => kind: String, name: String?, path: String?, language: String?, line_start: Int?, line_end: Int?, signature: String?, properties: String, content_hash: String?} }
%ignore_error { :create graph_edges {from: String, to: String, kind: String => confidence: Int?, source: String?, properties: String} }
"#,
            BTreeMap::new(),
        )?;

        let rows = self.run_read(
            "?[value] := *graph_meta['schema_version', value]",
            BTreeMap::new(),
        )?;
        match rows
            .rows
            .first()
            .and_then(|row| row.first())
            .and_then(DataValue::get_int)
        {
            None => self.run_mut(
                "?[key, value] <- [['schema_version', $version]] :insert graph_meta {key => value}",
                params([("version", DataValue::from(GRAPH_SCHEMA_VERSION as i64))]),
            ),
            Some(version) if version == GRAPH_SCHEMA_VERSION as i64 => Ok(()),
            Some(version) => bail!(
                "graph schema version {version} is incompatible with supported version {GRAPH_SCHEMA_VERSION}; rebuild the project graph"
            ),
        }
    }

    fn run_mut(&self, script: &str, params: BTreeMap<String, DataValue>) -> Result<()> {
        self.db
            .run_script(script, params, ScriptMutability::Mutable)
            .map(|_| ())
            .map_err(|error| anyhow!(error.to_string()))
    }

    fn run_read(
        &self,
        script: &str,
        params: BTreeMap<String, DataValue>,
    ) -> Result<cozo::NamedRows> {
        self.db
            .run_script(script, params, ScriptMutability::Immutable)
            .map_err(|error| anyhow!(error.to_string()))
    }

    fn incident_edges(
        &self,
        stable_key: &str,
        direction: Direction,
        limit: usize,
    ) -> Result<Vec<Edge>> {
        let rule = match direction {
            Direction::Outgoing => "from == $stable_key",
            Direction::Incoming => "to == $stable_key",
            Direction::Both => "from == $stable_key or to == $stable_key",
        };
        let script = format!(
            "?[from, to, kind, confidence, source, properties] := *graph_edges[from, to, kind, confidence, source, properties], {rule} :sort from, to, kind :limit {limit}"
        );
        let rows = self.run_read(
            &script,
            params([("stable_key", DataValue::from(stable_key))]),
        )?;
        rows.rows.iter().map(|row| row_to_edge(row)).collect()
    }
}

impl GraphStore for CozoGraphStore {
    fn schema_version(&self) -> Result<u32> {
        let rows = self.run_read(
            "?[value] := *graph_meta['schema_version', value]",
            BTreeMap::new(),
        )?;
        let version = rows
            .rows
            .first()
            .and_then(|row| row.first())
            .and_then(DataValue::get_int)
            .ok_or_else(|| anyhow!("graph schema version is missing or invalid"))?;
        u32::try_from(version).map_err(|_| anyhow!("graph schema version is out of range"))
    }

    fn upsert_node(&mut self, node: &Node) -> Result<()> {
        validate_node(node)?;
        write_node(&self.db, node)
    }

    fn upsert_edge(&mut self, edge: &Edge) -> Result<()> {
        validate_edge(edge)?;
        write_edge(&self.db, edge)
    }

    fn apply_batch(&mut self, nodes: &[Node], edges: &[Edge]) -> Result<()> {
        let transaction = self.db.multi_transaction(true);
        let result = (|| {
            for node in nodes {
                validate_node(node)?;
                write_node(&transaction, node)?;
            }
            for edge in edges {
                validate_edge(edge)?;
                write_edge(&transaction, edge)?;
            }
            transaction
                .commit()
                .map_err(|error| anyhow!(error.to_string()))
        })();
        if result.is_err() {
            let _ = transaction.abort();
        }
        result
    }

    fn replace_file_subgraph(&mut self, path: &str, nodes: &[Node], edges: &[Edge]) -> Result<()> {
        if path.trim().is_empty() {
            bail!("graph file path must not be empty");
        }
        for node in nodes {
            validate_node(node)?;
        }
        for edge in edges {
            validate_edge(edge)?;
        }

        let transaction = self.db.multi_transaction(true);
        let result = (|| {
            remove_file_subgraph(&transaction, path)?;
            for node in nodes {
                write_node(&transaction, node)?;
            }
            for edge in edges {
                write_edge(&transaction, edge)?;
            }
            transaction
                .commit()
                .map_err(|error| anyhow!(error.to_string()))
        })();
        if result.is_err() {
            let _ = transaction.abort();
        }
        result
    }

    fn prune_file_subgraphs(&mut self, retained_paths: &BTreeSet<String>) -> Result<usize> {
        let rows = self.run_read(
            "?[path] := *graph_nodes[_, 'file', _, path, _, _, _, _, _, _] :sort path",
            BTreeMap::new(),
        )?;
        let stale_paths = rows
            .rows
            .iter()
            .map(|row| {
                row.first()
                    .ok_or_else(|| anyhow!("invalid graph file path row"))
                    .and_then(|value| required_str(value, "file path"))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|path| !retained_paths.contains(path))
            .collect::<Vec<_>>();
        if stale_paths.is_empty() {
            return Ok(0);
        }

        let transaction = self.db.multi_transaction(true);
        let result = (|| {
            for path in &stale_paths {
                remove_file_subgraph(&transaction, path)?;
            }
            transaction
                .commit()
                .map_err(|error| anyhow!(error.to_string()))
        })();
        if result.is_err() {
            let _ = transaction.abort();
        }
        result.map(|()| stale_paths.len())
    }

    fn find_node(&self, stable_key: &str) -> Result<Option<Node>> {
        let rows = self.run_read(
            "?[stable_key, kind, name, path, language, line_start, line_end, signature, properties, content_hash] := *graph_nodes[stable_key, kind, name, path, language, line_start, line_end, signature, properties, content_hash], stable_key == $stable_key",
            params([("stable_key", DataValue::from(stable_key))]),
        )?;
        rows.rows.first().map(row_to_node).transpose()
    }

    fn neighbors(&self, stable_key: &str, query: NeighborQuery) -> Result<GraphProjection> {
        let depth = query.depth.clamp(1, MAX_QUERY_DEPTH);
        let limit = query.limit.clamp(1, MAX_QUERY_RESULTS);
        let mut queue = VecDeque::from([(stable_key.to_string(), 0_u8)]);
        let mut visited = BTreeSet::from([stable_key.to_string()]);
        let mut edge_keys = BTreeSet::new();
        let mut edges = Vec::new();
        let mut truncated = false;

        while let Some((key, current_depth)) = queue.pop_front() {
            if current_depth >= depth {
                continue;
            }
            let candidates = self.incident_edges(&key, query.direction, limit + 1)?;
            for edge in candidates {
                let edge_key = (edge.from.clone(), edge.to.clone(), edge.kind.clone());
                if !edge_keys.insert(edge_key) {
                    continue;
                }
                if edges.len() == limit {
                    truncated = true;
                    break;
                }
                let adjacent = if edge.from == key {
                    edge.to.clone()
                } else {
                    edge.from.clone()
                };
                if visited.insert(adjacent.clone()) {
                    queue.push_back((adjacent, current_depth + 1));
                }
                edges.push(edge);
            }
            if truncated {
                break;
            }
        }

        let mut nodes = Vec::new();
        for key in visited {
            if let Some(node) = self.find_node(&key)? {
                nodes.push(node);
            }
        }
        nodes.sort_by(|left, right| left.stable_key.cmp(&right.stable_key));

        Ok(GraphProjection {
            nodes,
            edges,
            truncated,
        })
    }
}

trait ScriptRunner {
    fn execute(&self, script: &str, params: BTreeMap<String, DataValue>) -> Result<()>;
}

impl ScriptRunner for DbInstance {
    fn execute(&self, script: &str, params: BTreeMap<String, DataValue>) -> Result<()> {
        self.run_script(script, params, ScriptMutability::Mutable)
            .map(|_| ())
            .map_err(|error| anyhow!(error.to_string()))
    }
}

impl ScriptRunner for MultiTransaction {
    fn execute(&self, script: &str, params: BTreeMap<String, DataValue>) -> Result<()> {
        self.run_script(script, params)
            .map(|_| ())
            .map_err(|error| anyhow!(error.to_string()))
    }
}

fn remove_file_subgraph(runner: &impl ScriptRunner, path: &str) -> Result<()> {
    runner.execute(
        "stale[key] := *graph_nodes[key, _, _, path, _, _, _, _, _, _], path == $path\n?[from, to, kind] := *graph_edges[from, to, kind, _, _, _], stale[from]\n?[from, to, kind] := *graph_edges[from, to, kind, _, _, _], stale[to]\n:rm graph_edges {from, to, kind}",
        params([("path", DataValue::from(path))]),
    )?;
    runner.execute(
        "?[stable_key] := *graph_nodes[stable_key, _, _, path, _, _, _, _, _, _], path == $path :rm graph_nodes {stable_key}",
        params([("path", DataValue::from(path))]),
    )
}

fn write_node(runner: &impl ScriptRunner, node: &Node) -> Result<()> {
    runner.execute(
        "?[stable_key, kind, name, path, language, line_start, line_end, signature, properties, content_hash] <- [[$stable_key, $kind, $name, $path, $language, $line_start, $line_end, $signature, $properties, $content_hash]] :put graph_nodes {stable_key => kind, name, path, language, line_start, line_end, signature, properties, content_hash}",
        params([
            ("stable_key", DataValue::from(node.stable_key.as_str())),
            ("kind", DataValue::from(node_kind_name(&node.kind))),
            ("name", optional_string(node.name.as_deref())),
            ("path", optional_string(node.path.as_deref())),
            ("language", optional_string(node.language.as_deref())),
            ("line_start", optional_u32(node.line_start)),
            ("line_end", optional_u32(node.line_end)),
            ("signature", optional_string(node.signature.as_deref())),
            ("properties", DataValue::from(serde_json::to_string(&node.properties)?)),
            ("content_hash", optional_string(node.content_hash.as_deref())),
        ]),
    )
}

fn write_edge(runner: &impl ScriptRunner, edge: &Edge) -> Result<()> {
    runner.execute(
        "?[from, to, kind, confidence, source, properties] <- [[$from, $to, $kind, $confidence, $source, $properties]] :put graph_edges {from, to, kind => confidence, source, properties}",
        params([
            ("from", DataValue::from(edge.from.as_str())),
            ("to", DataValue::from(edge.to.as_str())),
            ("kind", DataValue::from(edge.kind.as_str())),
            ("confidence", edge.confidence.map_or(DataValue::Null, |value| DataValue::from(value as i64))),
            ("source", optional_string(edge.source.as_deref())),
            ("properties", DataValue::from(serde_json::to_string(&edge.properties)?)),
        ]),
    )
}

fn validate_node(node: &Node) -> Result<()> {
    if node.stable_key.trim().is_empty() {
        bail!("graph node stable_key must not be empty");
    }
    if node
        .line_start
        .zip(node.line_end)
        .is_some_and(|(start, end)| start > end)
    {
        bail!("graph node line_start must not exceed line_end");
    }
    Ok(())
}

fn validate_edge(edge: &Edge) -> Result<()> {
    if edge.from.trim().is_empty() || edge.to.trim().is_empty() || edge.kind.trim().is_empty() {
        bail!("graph edge requires from, to, and kind");
    }
    Ok(())
}

fn params<const N: usize>(values: [(&str, DataValue); N]) -> BTreeMap<String, DataValue> {
    values
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

fn optional_string(value: Option<&str>) -> DataValue {
    value.map_or(DataValue::Null, DataValue::from)
}

fn optional_u32(value: Option<u32>) -> DataValue {
    value.map_or(DataValue::Null, |value| DataValue::from(value as i64))
}

fn required_str(value: &DataValue, field: &str) -> Result<String> {
    value
        .get_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("graph {field} is missing or invalid"))
}

fn optional_str(value: &DataValue) -> Option<String> {
    value.get_str().map(str::to_string)
}

fn optional_int(value: &DataValue, field: &str) -> Result<Option<i64>> {
    match value {
        DataValue::Null => Ok(None),
        _ => value
            .get_int()
            .map(Some)
            .ok_or_else(|| anyhow!("graph {field} is invalid")),
    }
}

fn properties(value: &DataValue) -> Result<BTreeMap<String, Value>> {
    let encoded = required_str(value, "properties")?;
    serde_json::from_str(&encoded).context("decode graph properties")
}

fn row_to_node(row: &Vec<DataValue>) -> Result<Node> {
    if row.len() != 10 {
        bail!("invalid graph node row");
    }
    Ok(Node {
        stable_key: required_str(&row[0], "node stable_key")?,
        kind: parse_node_kind(&required_str(&row[1], "node kind")?)?,
        name: optional_str(&row[2]),
        path: optional_str(&row[3]),
        language: optional_str(&row[4]),
        line_start: optional_int(&row[5], "node line_start")?
            .map(u32::try_from)
            .transpose()
            .context("graph node line_start is out of range")?,
        line_end: optional_int(&row[6], "node line_end")?
            .map(u32::try_from)
            .transpose()
            .context("graph node line_end is out of range")?,
        signature: optional_str(&row[7]),
        properties: properties(&row[8])?,
        content_hash: optional_str(&row[9]),
    })
}

fn row_to_edge(row: &[DataValue]) -> Result<Edge> {
    if row.len() != 6 {
        bail!("invalid graph edge row");
    }
    Ok(Edge {
        from: required_str(&row[0], "edge from")?,
        to: required_str(&row[1], "edge to")?,
        kind: required_str(&row[2], "edge kind")?,
        confidence: optional_int(&row[3], "edge confidence")?
            .map(u8::try_from)
            .transpose()
            .context("graph edge confidence is out of range")?,
        source: optional_str(&row[4]),
        properties: properties(&row[5])?,
    })
}

fn node_kind_name(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::File => "file",
        NodeKind::Folder => "folder",
        NodeKind::Document => "document",
        NodeKind::Section => "section",
        NodeKind::Module => "module",
        NodeKind::Namespace => "namespace",
        NodeKind::Function => "function",
        NodeKind::Method => "method",
        NodeKind::Class => "class",
        NodeKind::Struct => "struct",
        NodeKind::Enum => "enum",
        NodeKind::Interface => "interface",
        NodeKind::Trait => "trait",
        NodeKind::Variable => "variable",
        NodeKind::Constant => "constant",
        NodeKind::Type => "type",
        NodeKind::Macro => "macro",
        NodeKind::Test => "test",
        NodeKind::Memory => "memory",
        NodeKind::Decision => "decision",
        NodeKind::Commit => "commit",
        NodeKind::Branch => "branch",
    }
}

fn parse_node_kind(value: &str) -> Result<NodeKind> {
    let kind = match value {
        "file" => NodeKind::File,
        "folder" => NodeKind::Folder,
        "document" => NodeKind::Document,
        "section" => NodeKind::Section,
        "module" => NodeKind::Module,
        "namespace" => NodeKind::Namespace,
        "function" => NodeKind::Function,
        "method" => NodeKind::Method,
        "class" => NodeKind::Class,
        "struct" => NodeKind::Struct,
        "enum" => NodeKind::Enum,
        "interface" => NodeKind::Interface,
        "trait" => NodeKind::Trait,
        "variable" => NodeKind::Variable,
        "constant" => NodeKind::Constant,
        "type" => NodeKind::Type,
        "macro" => NodeKind::Macro,
        "test" => NodeKind::Test,
        "memory" => NodeKind::Memory,
        "decision" => NodeKind::Decision,
        "commit" => NodeKind::Commit,
        "branch" => NodeKind::Branch,
        _ => bail!("unknown graph node kind {value:?}"),
    };
    Ok(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn node(key: &str, kind: NodeKind) -> Node {
        Node {
            stable_key: key.into(),
            kind,
            name: Some(key.into()),
            path: None,
            language: None,
            line_start: None,
            line_end: None,
            signature: None,
            properties: BTreeMap::new(),
            content_hash: None,
        }
    }

    fn edge(from: &str, to: &str, kind: &str) -> Edge {
        Edge {
            from: from.into(),
            to: to.into(),
            kind: kind.into(),
            confidence: Some(100),
            source: Some("test".into()),
            properties: BTreeMap::new(),
        }
    }

    #[test]
    fn creates_store_in_project_graph_directory() {
        let dir = tempdir().unwrap();
        let store = CozoGraphStore::open(dir.path()).unwrap();
        assert_eq!(store.project_root(), dir.path());
        assert_eq!(store.schema_version().unwrap(), GRAPH_SCHEMA_VERSION);
        assert!(dir.path().join(".sqwai/graph/graph.db").exists());
    }

    #[test]
    fn node_upsert_is_idempotent_and_persistent() {
        let dir = tempdir().unwrap();
        let mut store = CozoGraphStore::open(dir.path()).unwrap();
        let mut item = node("file:a", NodeKind::File);
        store.upsert_node(&item).unwrap();
        item.name = Some("renamed".into());
        store.upsert_node(&item).unwrap();
        assert_eq!(store.find_node("file:a").unwrap().unwrap(), item);
        drop(store);

        let reopened = CozoGraphStore::open(dir.path()).unwrap();
        assert_eq!(reopened.find_node("file:a").unwrap().unwrap(), item);
    }

    #[test]
    fn persists_edges_and_bounds_neighbor_projection() {
        let dir = tempdir().unwrap();
        let mut store = CozoGraphStore::open(dir.path()).unwrap();
        let nodes = [
            node("file:a", NodeKind::File),
            node("fn:a", NodeKind::Function),
            node("fn:b", NodeKind::Function),
        ];
        let edges = [
            edge("file:a", "fn:a", "contains"),
            edge("fn:a", "fn:b", "calls"),
        ];
        store.apply_batch(&nodes, &edges).unwrap();

        let projection = store
            .neighbors(
                "file:a",
                NeighborQuery {
                    direction: Direction::Outgoing,
                    depth: 2,
                    limit: 1,
                },
            )
            .unwrap();
        assert_eq!(projection.edges.len(), 1);
        assert!(projection.truncated);
        assert_eq!(projection.nodes.len(), 2);

        let incoming = store
            .neighbors(
                "fn:b",
                NeighborQuery {
                    direction: Direction::Incoming,
                    depth: 1,
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(incoming.edges, vec![edges[1].clone()]);
    }

    #[test]
    fn failed_batch_rolls_back_all_writes() {
        let dir = tempdir().unwrap();
        let mut store = CozoGraphStore::open(dir.path()).unwrap();
        let invalid = edge("file:a", "", "contains");
        assert!(
            store
                .apply_batch(&[node("file:a", NodeKind::File)], &[invalid])
                .is_err()
        );
        assert!(store.find_node("file:a").unwrap().is_none());
    }

    #[test]
    fn validates_graph_records() {
        let dir = tempdir().unwrap();
        let mut store = CozoGraphStore::open(dir.path()).unwrap();
        assert!(store.upsert_node(&node("", NodeKind::File)).is_err());
        assert!(store.upsert_edge(&edge("", "file:a", "contains")).is_err());
    }
}
