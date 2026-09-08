#![allow(dead_code)] // parts of the store API stay ahead of their consumers (recall/graph_query land in I5)

//! Project knowledge graph foundation (§2.4).
//!
//! SQLite (rusqlite, bundled) behind the `GraphStore` contract: indexers,
//! agent tools, and UI projections never touch SQL directly. Edges are stored
//! by stable keys rather than node ids, so adapters may emit edges whose
//! target is not (yet) an indexed node — the same freedom §2.4.5 relies on
//! for `mentions` edges.

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
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
    pub properties: std::collections::BTreeMap<String, Value>,
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
    pub properties: std::collections::BTreeMap<String, Value>,
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

/// The status sidecar next to `graph.db` (§2.4.2). The database itself stays
/// authoritative for data; this file is what a UI can read cheaply and what
/// survives a database swap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphMeta {
    pub schema_version: u32,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub built_at: Option<u64>,
    pub status: GraphStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphStatus {
    Ok,
    Building,
    Stale,
    Corrupt,
}

pub fn read_meta(graph_dir: &Path) -> Option<GraphMeta> {
    let text = std::fs::read_to_string(graph_dir.join("meta.json")).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write_meta(graph_dir: &Path, meta: &GraphMeta) -> Result<()> {
    let path = graph_dir.join("meta.json");
    let text = serde_json::to_string_pretty(meta)?;
    std::fs::write(&path, text).with_context(|| format!("write graph meta {}", path.display()))
}

pub struct SqliteGraphStore {
    conn: Connection,
    project_root: PathBuf,
    graph_dir: PathBuf,
}

impl std::fmt::Debug for SqliteGraphStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteGraphStore")
            .field("project_root", &self.project_root)
            .field("graph_dir", &self.graph_dir)
            .finish_non_exhaustive()
    }
}

impl SqliteGraphStore {
    /// Open (creating if needed) the project graph. A database that fails to
    /// open or initialize is quarantined as `corrupt-<unix_ts>.db` and the
    /// error points at `/graph-rebuild` (§2.4.2); a valid database with a
    /// newer or older schema version is reported as an incompatible version
    /// instead — it is not corrupt, just foreign.
    pub fn open(project_root: impl Into<PathBuf>) -> Result<Self> {
        let project_root = project_root.into();
        let graph_dir = project_root.join(".sqwai").join("graph");
        std::fs::create_dir_all(&graph_dir)
            .with_context(|| format!("create graph directory {}", graph_dir.display()))?;
        let db_path = graph_dir.join("graph.db");

        match Self::connect(&db_path).and_then(Self::initialize) {
            Ok(conn) => {
                // a fresh or rebuilt store reports itself healthy
                let existing = read_meta(&graph_dir);
                if existing
                    .as_ref()
                    .is_none_or(|meta| meta.status != GraphStatus::Ok)
                {
                    let _ = write_meta(
                        &graph_dir,
                        &GraphMeta {
                            schema_version: GRAPH_SCHEMA_VERSION,
                            generation: existing.map(|meta| meta.generation).unwrap_or(0),
                            built_at: None,
                            status: GraphStatus::Ok,
                        },
                    );
                }
                Ok(Self {
                    conn,
                    project_root,
                    graph_dir,
                })
            }
            Err(error) => {
                if is_version_mismatch(&error) {
                    return Err(error);
                }
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or_default();
                for suffix in ["", "-wal", "-shm"] {
                    let from = PathBuf::from(format!("{}{suffix}", db_path.display()));
                    if from.exists() {
                        let _ = std::fs::rename(
                            &from,
                            graph_dir.join(format!("corrupt-{stamp}{suffix}.db")),
                        );
                    }
                }
                let _ = write_meta(
                    &graph_dir,
                    &GraphMeta {
                        schema_version: GRAPH_SCHEMA_VERSION,
                        generation: 0,
                        built_at: None,
                        status: GraphStatus::Corrupt,
                    },
                );
                bail!(
                    "graph database failed to open ({error:#}) and was quarantined; \
                     run /graph-rebuild to rebuild it"
                )
            }
        }
    }

    fn connect(db_path: &Path) -> Result<Connection> {
        let conn = Connection::open(db_path)
            .with_context(|| format!("open graph database {}", db_path.display()))?;
        conn.busy_timeout(std::time::Duration::from_millis(5_000))
            .context("set graph busy timeout")?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("set graph journal mode")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("set graph synchronous mode")?;
        Ok(conn)
    }

    fn initialize(conn: Connection) -> Result<Connection> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS files(
                path TEXT PRIMARY KEY,
                hash TEXT,
                size INTEGER,
                mtime INTEGER,
                lang TEXT,
                level INTEGER,
                adapter TEXT,
                adapter_version TEXT,
                indexed_at INTEGER,
                status TEXT NOT NULL DEFAULT 'ok',
                error TEXT
            );
            CREATE TABLE IF NOT EXISTS nodes(
                id INTEGER PRIMARY KEY,
                key TEXT UNIQUE NOT NULL,
                kind TEXT NOT NULL,
                name TEXT,
                path TEXT,
                lang TEXT,
                line_start INTEGER,
                line_end INTEGER,
                signature TEXT,
                props TEXT NOT NULL DEFAULT '{}',
                hash TEXT,
                source TEXT,
                confidence INTEGER,
                generation INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_nodes_kind ON nodes(kind);
            CREATE INDEX IF NOT EXISTS idx_nodes_path ON nodes(path);
            CREATE INDEX IF NOT EXISTS idx_nodes_name ON nodes(name);
            CREATE TABLE IF NOT EXISTS edges(
                from_key TEXT NOT NULL,
                to_key TEXT NOT NULL,
                kind TEXT NOT NULL,
                source TEXT NOT NULL DEFAULT '',
                confidence INTEGER,
                props TEXT NOT NULL DEFAULT '{}',
                PRIMARY KEY(from_key, to_key, kind, source)
            );
            CREATE INDEX IF NOT EXISTS idx_edges_to ON edges(to_key);
            CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
                key, name, path, signature, text
            );
            CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);
            "#,
        )
        .context("initialize graph schema")?;

        let version: Option<String> = conn
            .query_row("SELECT v FROM meta WHERE k = 'schema_version'", [], |row| {
                row.get(0)
            })
            .optional()?;
        match version {
            None => {
                conn.execute(
                    "INSERT INTO meta(k, v) VALUES ('schema_version', ?1)",
                    [GRAPH_SCHEMA_VERSION.to_string()],
                )
                .context("record graph schema version")?;
                Ok(conn)
            }
            Some(v) if v == GRAPH_SCHEMA_VERSION.to_string() => Ok(conn),
            Some(v) => bail!(
                "graph schema version {v} is incompatible with supported version \
                 {GRAPH_SCHEMA_VERSION}; rebuild the project graph"
            ),
        }
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Open a store at an explicit database path without the corrupt-file
    /// quarantine and meta sidecar management of `open` — used by the atomic
    /// full rebuild, which manages its own lifecycle.
    pub fn open_unmanaged(db_path: &Path, project_root: PathBuf) -> Result<Self> {
        let conn = Self::connect(db_path)?;
        let conn = Self::initialize(conn)?;
        let graph_dir = db_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(Self {
            conn,
            project_root,
            graph_dir,
        })
    }

    pub fn graph_dir(&self) -> &Path {
        &self.graph_dir
    }

    /// Bump the rebuild generation (full rebuilds only).
    pub fn bump_generation(&mut self) -> Result<u64> {
        self.conn
            .execute(
                "INSERT INTO meta(k, v) VALUES ('generation', '1')
                 ON CONFLICT(k) DO UPDATE SET v = CAST(CAST(v AS INTEGER) + 1 AS TEXT)",
                [],
            )
            .context("bump graph generation")?;
        let generation: String = self
            .conn
            .query_row("SELECT v FROM meta WHERE k = 'generation'", [], |row| {
                row.get(0)
            })
            .context("read graph generation")?;
        generation
            .parse()
            .map_err(|e| anyhow!("graph generation: {e}"))
    }

    fn incident_edges(
        &self,
        stable_key: &str,
        direction: Direction,
        limit: usize,
    ) -> Result<Vec<Edge>> {
        let rule = match direction {
            Direction::Outgoing => "from_key = ?1",
            Direction::Incoming => "to_key = ?1",
            Direction::Both => "(from_key = ?1 OR to_key = ?1)",
        };
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT from_key, to_key, kind, source, confidence, props
                 FROM edges WHERE {rule}
                 ORDER BY from_key, to_key, kind, source LIMIT ?2"
            ))
            .context("prepare incident edges query")?;
        let rows = stmt
            .query_map(rusqlite::params![stable_key, limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .context("read incident edges")?;
        let mut edges = Vec::new();
        for row in rows {
            let (from, to, kind, source, confidence, props) = row?;
            edges.push(Edge {
                from,
                to,
                kind,
                confidence: confidence.map(|c| c as u8),
                source: (!source.is_empty()).then_some(source),
                properties: parse_props(&props)?,
            });
        }
        Ok(edges)
    }
}

fn is_version_mismatch(error: &anyhow::Error) -> bool {
    error
        .to_string()
        .contains("incompatible with supported version")
}

fn parse_props(text: &str) -> Result<std::collections::BTreeMap<String, Value>> {
    serde_json::from_str(text).context("decode graph properties")
}

impl GraphStore for SqliteGraphStore {
    fn schema_version(&self) -> Result<u32> {
        let version: Option<String> = self
            .conn
            .query_row("SELECT v FROM meta WHERE k = 'schema_version'", [], |row| {
                row.get(0)
            })
            .optional()
            .context("read graph schema version")?;
        let version = version.ok_or_else(|| anyhow!("graph schema version is missing"))?;
        version
            .parse()
            .map_err(|e| anyhow!("graph schema version is invalid: {e}"))
    }

    fn upsert_node(&mut self, node: &Node) -> Result<()> {
        validate_node(node)?;
        let tx = self.conn.transaction().context("begin graph transaction")?;
        let id = write_node(&tx, node)?;
        write_fts(&tx, id, node)?;
        tx.commit().context("commit graph transaction")
    }

    fn upsert_edge(&mut self, edge: &Edge) -> Result<()> {
        validate_edge(edge)?;
        let tx = self.conn.transaction().context("begin graph transaction")?;
        write_edge(&tx, edge)?;
        tx.commit().context("commit graph transaction")
    }

    fn apply_batch(&mut self, nodes: &[Node], edges: &[Edge]) -> Result<()> {
        for node in nodes {
            validate_node(node)?;
        }
        for edge in edges {
            validate_edge(edge)?;
        }
        let tx = self.conn.transaction().context("begin graph transaction")?;
        let result = (|| {
            let mut ids = std::collections::HashMap::new();
            for node in nodes {
                let id = write_node(&tx, node)?;
                write_fts(&tx, id, node)?;
                ids.insert(node.stable_key.as_str(), id);
            }
            for edge in edges {
                write_edge(&tx, edge)?;
            }
            Ok(())
        })();
        finish(tx, result)
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

        let tx = self.conn.transaction().context("begin graph transaction")?;
        let result = (|| {
            remove_file_subgraph(&tx, path)?;
            let mut file_meta = None;
            for node in nodes {
                let id = write_node(&tx, node)?;
                write_fts(&tx, id, node)?;
                if node.kind == NodeKind::File && node.path.as_deref() == Some(path) {
                    file_meta = Some(node.clone());
                }
            }
            for edge in edges {
                write_edge(&tx, edge)?;
            }
            record_file(&tx, path, file_meta.as_ref())?;
            Ok(())
        })();
        finish(tx, result)
    }

    fn prune_file_subgraphs(&mut self, retained_paths: &BTreeSet<String>) -> Result<usize> {
        let known = {
            let mut stmt = self
                .conn
                .prepare("SELECT DISTINCT path FROM nodes WHERE kind = 'file' AND path IS NOT NULL")
                .context("list indexed file paths")?;
            stmt.query_map([], |row| row.get::<_, String>(0))
                .context("read indexed file paths")?
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("read indexed file paths")?
        };
        let stale: Vec<&String> = known
            .iter()
            .filter(|path| !retained_paths.contains(*path))
            .collect();
        if stale.is_empty() {
            return Ok(0);
        }

        let tx = self.conn.transaction().context("begin graph transaction")?;
        let result = (|| {
            for path in &stale {
                remove_file_subgraph(&tx, path)?;
                tx.execute("DELETE FROM files WHERE path = ?1", [path])
                    .context("remove stale file record")?;
            }
            Ok(())
        })();
        finish(tx, result)?;
        Ok(stale.len())
    }

    fn find_node(&self, stable_key: &str) -> Result<Option<Node>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT key, kind, name, path, lang, line_start, line_end, signature, props, hash
                 FROM nodes WHERE key = ?1",
            )
            .context("prepare node lookup")?;
        let node = stmt
            .query_row([stable_key], row_to_node)
            .optional()
            .context("look up graph node")?;
        Ok(node)
    }

    fn neighbors(&self, stable_key: &str, query: NeighborQuery) -> Result<GraphProjection> {
        let depth = query.depth.clamp(1, MAX_QUERY_DEPTH) as i64;
        let limit = query.limit.clamp(1, MAX_QUERY_RESULTS);
        let join = match query.direction {
            Direction::Outgoing => "e.from_key = w.key",
            Direction::Incoming => "e.to_key = w.key",
            Direction::Both => "(e.from_key = w.key OR e.to_key = w.key)",
        };
        let edge_rule = match query.direction {
            Direction::Outgoing => "e.from_key IN (SELECT key FROM walk)",
            Direction::Incoming => "e.to_key IN (SELECT key FROM walk)",
            Direction::Both => {
                "(e.from_key IN (SELECT key FROM walk) OR e.to_key IN (SELECT key FROM walk))"
            }
        };
        // UNION (not UNION ALL) dedups visited keys, so cycles terminate;
        // BFS row order makes the first discovery of a key the shortest one.
        let sql = format!(
            "WITH RECURSIVE walk(key, depth) AS (
                 SELECT ?1, 0
                 UNION
                 SELECT CASE WHEN e.from_key = w.key THEN e.to_key ELSE e.from_key END,
                        w.depth + 1
                 FROM walk w JOIN edges e ON {join}
                 WHERE w.depth < ?2
             )
             SELECT e.from_key, e.to_key, e.kind, e.source, e.confidence, e.props
             FROM edges e
             WHERE {edge_rule}
                 AND e.from_key IN (SELECT key FROM walk)
                 AND e.to_key IN (SELECT key FROM walk)
             ORDER BY e.from_key, e.to_key, e.kind, e.source
             LIMIT ?3"
        );

        let mut stmt = self.conn.prepare(&sql).context("prepare neighbor query")?;
        let rows = stmt
            .query_map(
                rusqlite::params![stable_key, depth, limit as i64 + 1],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .context("read neighbor edges")?;

        let mut edges = Vec::new();
        let mut truncated = false;
        let mut visited: BTreeSet<String> = BTreeSet::from([stable_key.to_string()]);
        for row in rows {
            let (from, to, kind, source, confidence, props) = row?;
            if edges.len() == limit {
                truncated = true;
                break;
            }
            visited.insert(from.clone());
            visited.insert(to.clone());
            edges.push(Edge {
                from,
                to,
                kind,
                confidence: confidence.map(|c| c as u8),
                source: (!source.is_empty()).then_some(source),
                properties: parse_props(&props)?,
            });
        }

        let mut nodes = Vec::new();
        for key in &visited {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT key, kind, name, path, lang, line_start, line_end, signature, props, hash
                     FROM nodes WHERE key = ?1",
                )
                .context("prepare node lookup")?;
            if let Some(node) = stmt
                .query_row([key], row_to_node)
                .optional()
                .context("look up graph node")?
            {
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

fn finish(tx: rusqlite::Transaction, result: Result<()>) -> Result<()> {
    if result.is_err() {
        let _ = tx.rollback();
        return result;
    }
    tx.commit().context("commit graph transaction")
}

fn write_node(tx: &rusqlite::Transaction, node: &Node) -> Result<i64> {
    tx.execute(
        "INSERT INTO nodes(key, kind, name, path, lang, line_start, line_end, signature, props, hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(key) DO UPDATE SET
            kind = excluded.kind, name = excluded.name, path = excluded.path,
            lang = excluded.lang, line_start = excluded.line_start,
            line_end = excluded.line_end, signature = excluded.signature,
            props = excluded.props, hash = excluded.hash",
        rusqlite::params![
            node.stable_key,
            node_kind_name(&node.kind),
            node.name,
            node.path,
            node.language,
            node.line_start.map(i64::from),
            node.line_end.map(i64::from),
            node.signature,
            serde_json::to_string(&node.properties)?,
            node.content_hash,
        ],
    )
    .context("write graph node")?;
    Ok(tx.query_row(
        "SELECT id FROM nodes WHERE key = ?1",
        [&node.stable_key],
        |row| row.get(0),
    )?)
}

fn write_fts(tx: &rusqlite::Transaction, node_id: i64, node: &Node) -> Result<()> {
    tx.execute("DELETE FROM nodes_fts WHERE rowid = ?1", [node_id])
        .context("refresh graph fts row")?;
    tx.execute(
        "INSERT INTO nodes_fts(rowid, key, name, path, signature, text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            node_id,
            node.stable_key,
            node.name,
            node.path,
            node.signature,
            "",
        ],
    )
    .map(|_| ())
    .context("write graph fts row")
}

fn write_edge(tx: &rusqlite::Transaction, edge: &Edge) -> Result<()> {
    tx.execute(
        "INSERT INTO edges(from_key, to_key, kind, source, confidence, props)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(from_key, to_key, kind, source) DO UPDATE SET
            confidence = excluded.confidence, props = excluded.props",
        rusqlite::params![
            edge.from,
            edge.to,
            edge.kind,
            edge.source.clone().unwrap_or_default(),
            edge.confidence.map(i64::from),
            serde_json::to_string(&edge.properties)?,
        ],
    )
    .map(|_| ())
    .context("write graph edge")
}

/// Record the `files` row from the file node of a batch, if present.
fn record_file(tx: &rusqlite::Transaction, path: &str, file_node: Option<&Node>) -> Result<()> {
    let adapter = file_node
        .and_then(|node| node.properties.get("source_adapter"))
        .and_then(Value::as_str)
        .unwrap_or("generic");
    let level = file_node
        .and_then(|node| node.properties.get("adapter_level"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as i64;
    let version = file_node
        .and_then(|node| node.properties.get("adapter_version"))
        .and_then(Value::as_str)
        .unwrap_or("1");
    let hash = file_node.and_then(|node| node.content_hash.as_deref());
    let size = file_node
        .and_then(|node| node.properties.get("size_bytes"))
        .and_then(Value::as_u64);
    tx.execute(
        "INSERT INTO files(path, hash, size, lang, level, adapter, adapter_version, indexed_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'ok')
         ON CONFLICT(path) DO UPDATE SET
            hash = excluded.hash, size = excluded.size, lang = excluded.lang,
            level = excluded.level, adapter = excluded.adapter,
            adapter_version = excluded.adapter_version,
            indexed_at = excluded.indexed_at, status = 'ok', error = NULL",
        rusqlite::params![
            path,
            hash,
            size.map(|value| value as i64),
            file_node.and_then(|node| node.language.as_deref()),
            level,
            adapter,
            version,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or_default(),
        ],
    )
    .map(|_| ())
    .context("record indexed file")
}

fn remove_file_subgraph(tx: &rusqlite::Transaction, path: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM nodes_fts WHERE rowid IN (SELECT id FROM nodes WHERE path = ?1)",
        [path],
    )
    .context("remove graph fts rows")?;
    tx.execute(
        "DELETE FROM edges WHERE from_key IN (SELECT key FROM nodes WHERE path = ?1)
            OR to_key IN (SELECT key FROM nodes WHERE path = ?1)",
        [path],
    )
    .context("remove graph edges")?;
    tx.execute("DELETE FROM nodes WHERE path = ?1", [path])
        .map(|_| ())
        .context("remove graph nodes")
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

fn row_to_node(row: &rusqlite::Row) -> rusqlite::Result<Node> {
    Ok(Node {
        stable_key: row.get(0)?,
        kind: kind_from_str(&row.get::<_, String>(1)?).ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(1, "kind".into(), rusqlite::types::Type::Text)
        })?,
        name: row.get(2)?,
        path: row.get(3)?,
        language: row.get(4)?,
        line_start: row.get::<_, Option<i64>>(5)?.map(|v| v as u32),
        line_end: row.get::<_, Option<i64>>(6)?.map(|v| v as u32),
        signature: row.get(7)?,
        properties: parse_props(&row.get::<_, String>(8)?).unwrap_or_default(),
        content_hash: row.get(9)?,
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
    kind_from_str(value).ok_or_else(|| anyhow!("unknown graph node kind {value:?}"))
}

fn kind_from_str(value: &str) -> Option<NodeKind> {
    Some(match value {
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
        _ => return None,
    })
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
            properties: std::collections::BTreeMap::new(),
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
            properties: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn creates_store_in_project_graph_directory() {
        let dir = tempdir().unwrap();
        let store = SqliteGraphStore::open(dir.path()).unwrap();
        assert_eq!(store.project_root(), dir.path());
        assert_eq!(store.schema_version().unwrap(), GRAPH_SCHEMA_VERSION);
        assert!(dir.path().join(".sqwai/graph/graph.db").exists());
    }

    #[test]
    fn node_upsert_is_idempotent_and_persistent() {
        let dir = tempdir().unwrap();
        let mut store = SqliteGraphStore::open(dir.path()).unwrap();
        let mut item = node("file:a", NodeKind::File);
        store.upsert_node(&item).unwrap();
        item.name = Some("renamed".into());
        store.upsert_node(&item).unwrap();
        assert_eq!(store.find_node("file:a").unwrap().unwrap(), item);
        drop(store);

        let reopened = SqliteGraphStore::open(dir.path()).unwrap();
        assert_eq!(reopened.find_node("file:a").unwrap().unwrap(), item);
    }

    #[test]
    fn persists_edges_and_bounds_neighbor_projection() {
        let dir = tempdir().unwrap();
        let mut store = SqliteGraphStore::open(dir.path()).unwrap();
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
    fn neighbor_walk_terminates_on_cycles() {
        let dir = tempdir().unwrap();
        let mut store = SqliteGraphStore::open(dir.path()).unwrap();
        let nodes = [node("a", NodeKind::Function), node("b", NodeKind::Function)];
        let edges = [edge("a", "b", "calls"), edge("b", "a", "calls")];
        store.apply_batch(&nodes, &edges).unwrap();
        let projection = store.neighbors("a", NeighborQuery::default()).unwrap();
        assert_eq!(projection.nodes.len(), 2);
        assert_eq!(projection.edges.len(), 2);
    }

    #[test]
    fn failed_batch_rolls_back_all_writes() {
        let dir = tempdir().unwrap();
        let mut store = SqliteGraphStore::open(dir.path()).unwrap();
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
        let mut store = SqliteGraphStore::open(dir.path()).unwrap();
        assert!(store.upsert_node(&node("", NodeKind::File)).is_err());
        assert!(store.upsert_edge(&edge("", "file:a", "contains")).is_err());
    }

    #[test]
    fn corrupt_database_is_quarantined_and_reported() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join(".sqwai/graph");
        std::fs::create_dir_all(&db_path).unwrap();
        std::fs::write(db_path.join("graph.db"), b"this is not a sqlite database").unwrap();

        let error = SqliteGraphStore::open(dir.path()).unwrap_err();
        assert!(error.to_string().contains("graph-rebuild"), "{error}");
        let corrupt: Vec<_> = std::fs::read_dir(&db_path)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("corrupt-"))
            .collect();
        assert_eq!(corrupt.len(), 1, "the bad db file is kept as corrupt-*.db");
        assert!(!db_path.join("graph.db").exists());

        // after quarantine, opening creates a fresh usable store
        let store = SqliteGraphStore::open(dir.path()).unwrap();
        assert_eq!(store.schema_version().unwrap(), GRAPH_SCHEMA_VERSION);
        let meta = read_meta(&db_path).unwrap();
        assert_eq!(meta.status, GraphStatus::Ok);
    }

    #[test]
    fn schema_version_mismatch_is_reported_not_quarantined() {
        let dir = tempdir().unwrap();
        {
            let store = SqliteGraphStore::open(dir.path()).unwrap();
            store
                .conn
                .execute("UPDATE meta SET v = '99' WHERE k = 'schema_version'", [])
                .unwrap();
        }
        let error = SqliteGraphStore::open(dir.path()).unwrap_err();
        assert!(error.to_string().contains("incompatible"), "{error}");
        // the database is left in place
        assert!(dir.path().join(".sqwai/graph/graph.db").exists());
    }
}
