//! Graph memory adapter (§2.4.5).
//!
//! Reads `.sqwai/memory/*.md` and `.sqwai/journal/*.jsonl`, emitting `memory`
//! and `decision` nodes with `source: "model"`, `about` edges to resolved symbols,
//! `mentions` edges to unresolved references (kept for stale detection), and
//! `supersedes` edges for corrections and resolved assumptions.

use super::graph::{Edge, GraphStore, Node, NodeKind};
use anyhow::Result;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Extract backticked symbols/paths from prose, e.g. "`Session`", "`src/calc.rs`".
pub fn extract_backticks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_backtick = false;
    let mut current = String::new();

    for c in text.chars() {
        if c == '`' {
            if in_backtick {
                let trimmed = current.trim();
                if !trimmed.is_empty() {
                    // if it contains types or arguments like `Session.todos: Vec<String>`,
                    // extract the primary identifier or path
                    let clean = if let Some((head, _)) = trimmed.split_once(':') {
                        head.trim()
                    } else if let Some((head, _)) = trimmed.split_once('(') {
                        head.trim()
                    } else {
                        trimmed
                    };
                    if !clean.is_empty() && !out.contains(&clean.to_string()) {
                        out.push(clean.to_string());
                    }
                }
                current.clear();
                in_backtick = false;
            } else {
                in_backtick = true;
            }
        } else if in_backtick {
            current.push(c);
        }
    }
    out
}

/// Extract journal sequence references like `j#16` or `(j#16)`.
pub fn extract_journal_refs(text: &str) -> Vec<u64> {
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c == 'j' || c == 'J' {
            if let Some(&(_, '#')) = chars.peek() {
                chars.next(); // consume '#'
                let mut num_str = String::new();
                while let Some(&(_, digit)) = chars.peek() {
                    if digit.is_ascii_digit() {
                        num_str.push(digit);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if let Ok(seq) = num_str.parse::<u64>() {
                    if !out.contains(&seq) {
                        out.push(seq);
                    }
                }
            }
        }
    }
    out
}

/// Parse diary entries in a day file (`.sqwai/memory/YYYY-MM-DD.md`).
///
/// Headings: `## HH:MM · session <id> ...`
/// Sections: `### Done`, `### Decisions`, `### Rejected`, `### Open`, `### Corrections`
pub fn parse_diary(
    relative_path: &str,
    date: &str,
    content: &str,
) -> Vec<(Node, Vec<String>, Option<String>)> {
    let mut result = Vec::new();
    let mut current_time = "00-00".to_string();
    let mut current_session: Option<String> = None;
    let mut current_section: Option<String> = None;
    let mut section_counts: BTreeMap<String, usize> = BTreeMap::new();

    for (line_idx, line) in content.lines().enumerate() {
        let line_num = (line_idx + 1) as u32;
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix("## ") {
            // e.g. "18:47 · session a8f2 · plan 01J…"
            let parts: Vec<&str> = rest.split('·').map(str::trim).collect();
            if let Some(time_part) = parts.first() {
                current_time = time_part.replace(':', "-");
            }
            current_session = parts.iter().find_map(|p| {
                p.strip_prefix("session ")
                    .or_else(|| p.strip_prefix("session:"))
                    .map(|s| s.trim().to_string())
            });
            current_section = None;
            section_counts.clear();
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("### ") {
            current_section = Some(rest.trim().to_string());
            continue;
        }

        if trimmed.starts_with('-') || trimmed.starts_with('*') {
            let bullet_text = trimmed[1..].trim();
            if bullet_text.is_empty() {
                continue;
            }

            let section_name = current_section.as_deref().unwrap_or("Notes");
            let is_decision = section_name.eq_ignore_ascii_case("Decisions");
            let is_correction = section_name.eq_ignore_ascii_case("Corrections");

            let sec_key = section_name.to_lowercase();
            let count = section_counts.entry(sec_key.clone()).or_insert(0);
            *count += 1;

            let (kind, stable_key) = if is_decision {
                (
                    NodeKind::Decision,
                    format!("mem:{date}#{current_time}:decision:{count}"),
                )
            } else {
                (
                    NodeKind::Memory,
                    format!("mem:{date}#{current_time}:{sec_key}:{count}"),
                )
            };

            let j_refs = extract_journal_refs(bullet_text);
            let backticks = extract_backticks(bullet_text);

            let mut props = BTreeMap::new();
            props.insert("author".into(), serde_json::json!("model"));
            props.insert("text".into(), serde_json::json!(bullet_text));
            props.insert("section".into(), serde_json::json!(section_name));
            props.insert("date".into(), serde_json::json!(date));
            props.insert("time".into(), serde_json::json!(current_time));
            if let Some(session) = &current_session {
                props.insert("session".into(), serde_json::json!(session));
            }
            if let Some(first_j) = j_refs.first() {
                props.insert("journal_ref".into(), serde_json::json!(format!("j#{first_j}")));
            }

            let name = if bullet_text.len() > 60 {
                format!("{}...", &bullet_text[..bullet_text.floor_char_boundary(57)])
            } else {
                bullet_text.to_string()
            };

            let node = Node {
                stable_key,
                kind,
                name: Some(name),
                path: Some(relative_path.to_string()),
                language: Some("markdown".into()),
                line_start: Some(line_num),
                line_end: Some(line_num),
                signature: None,
                roles: Vec::new(),
                properties: props,
                content_hash: None,
            };

            let supersedes_target = if is_correction {
                if let Some(first_j) = j_refs.first() {
                    current_session.as_ref().map(|s| format!("mem:journal:{s}:{first_j}"))
                } else {
                    None
                }
            } else {
                None
            };

            result.push((node, backticks, supersedes_target));
        }
    }

    result
}

/// Parse `MEMORY.md` (`.sqwai/memory/MEMORY.md`).
///
/// Sections: `## Project`, `## Conventions`, `## User`, `## Agreements`
pub fn parse_memory_md(
    relative_path: &str,
    content: &str,
) -> Vec<(Node, Vec<String>)> {
    let mut result = Vec::new();
    let mut current_section: Option<String> = None;
    let mut section_counts: BTreeMap<String, usize> = BTreeMap::new();

    for (line_idx, line) in content.lines().enumerate() {
        let line_num = (line_idx + 1) as u32;
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix("## ") {
            current_section = Some(rest.trim().to_string());
            continue;
        }

        if trimmed.starts_with('-') || trimmed.starts_with('*') {
            let bullet_text = trimmed[1..].trim();
            if bullet_text.is_empty() {
                continue;
            }

            let section_name = current_section.as_deref().unwrap_or("General");
            let sec_key = section_name.to_lowercase();
            let count = section_counts.entry(sec_key.clone()).or_insert(0);
            *count += 1;

            let stable_key = format!("mem:MEMORY.md:{sec_key}:{count}");
            let backticks = extract_backticks(bullet_text);
            let j_refs = extract_journal_refs(bullet_text);

            let author = if section_name.eq_ignore_ascii_case("User") {
                "user"
            } else {
                "model"
            };

            let mut props = BTreeMap::new();
            props.insert("author".into(), serde_json::json!(author));
            props.insert("text".into(), serde_json::json!(bullet_text));
            props.insert("section".into(), serde_json::json!(section_name));
            if let Some(first_j) = j_refs.first() {
                props.insert("journal_ref".into(), serde_json::json!(format!("j#{first_j}")));
            }

            let name = if bullet_text.len() > 60 {
                format!("{}...", &bullet_text[..bullet_text.floor_char_boundary(57)])
            } else {
                bullet_text.to_string()
            };

            let node = Node {
                stable_key,
                kind: NodeKind::Memory,
                name: Some(name),
                path: Some(relative_path.to_string()),
                language: Some("markdown".into()),
                line_start: Some(line_num),
                line_end: Some(line_num),
                signature: None,
                roles: Vec::new(),
                properties: props,
                content_hash: None,
            };

            result.push((node, backticks));
        }
    }

    result
}

/// Parse journal `.jsonl` note records into memory nodes.
pub fn parse_journal_notes(
    relative_path: &str,
    session_id: &str,
    content: &str,
) -> Vec<(Node, Vec<String>, Option<u64>)> {
    let mut result = Vec::new();

    for (line_idx, line) in content.lines().enumerate() {
        let line_num = (line_idx + 1) as u32;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        if value.get("kind").and_then(|v| v.as_str()) != Some("note") {
            continue;
        }

        let seq = value.get("seq").and_then(|v| v.as_u64()).unwrap_or(line_num as u64);
        let note_kind = value
            .get("note")
            .or_else(|| value.get("fields").and_then(|f| f.get("note")))
            .and_then(|v| v.as_str())
            .unwrap_or("note");

        let text = value
            .get("text")
            .or_else(|| value.get("fields").and_then(|f| f.get("text")))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if text.trim().is_empty() {
            continue;
        }

        let by = value
            .get("by")
            .or_else(|| value.get("fields").and_then(|f| f.get("by")))
            .and_then(|v| v.as_str())
            .unwrap_or("model");

        let resolves = value
            .get("resolves")
            .or_else(|| value.get("fields").and_then(|f| f.get("resolves")))
            .and_then(|v| v.as_u64());

        let is_decision = note_kind.eq_ignore_ascii_case("decision");
        let (kind, stable_key) = if is_decision {
            (
                NodeKind::Decision,
                format!("mem:journal:{session_id}:{seq}"),
            )
        } else {
            (
                NodeKind::Memory,
                format!("mem:journal:{session_id}:{seq}"),
            )
        };

        let backticks = extract_backticks(text);
        let mut props = BTreeMap::new();
        props.insert("author".into(), serde_json::json!(by));
        props.insert("text".into(), serde_json::json!(text));
        props.insert("note_kind".into(), serde_json::json!(note_kind));
        props.insert("journal_ref".into(), serde_json::json!(format!("j#{seq}")));
        props.insert("session".into(), serde_json::json!(session_id));
        if let Some(step) = value.get("step").and_then(|v| v.as_str()) {
            props.insert("step".into(), serde_json::json!(step));
        }

        let name = if text.len() > 60 {
            format!("{}...", &text[..text.floor_char_boundary(57)])
        } else {
            text.to_string()
        };

        let node = Node {
            stable_key,
            kind,
            name: Some(name),
            path: Some(relative_path.to_string()),
            language: Some("json".into()),
            line_start: Some(line_num),
            line_end: Some(line_num),
            signature: None,
            roles: Vec::new(),
            properties: props,
            content_hash: None,
        };

        result.push((node, backticks, resolves));
    }

    result
}

/// Resolve backticked symbol/path against the store.
/// Returns (edge_kind, target_key):
/// - ("about", target_key) if symbol or file resolves
/// - ("mentions", target_key) if unresolved (candidate preserved for stale detection)
fn resolve_mention(store: &impl GraphStore, token: &str) -> (String, String) {
    // 1. Direct node match
    if let Ok(Some(n)) = store.find_node(token) {
        return ("about".into(), n.stable_key);
    }
    // 2. Direct file match
    let file_target = format!("file:{token}");
    if let Ok(Some(n)) = store.find_node(&file_target) {
        return ("about".into(), n.stable_key);
    }
    // 3. Direct symbol match
    let sym_target = format!("sym:{token}");
    if let Ok(Some(n)) = store.find_node(&sym_target) {
        return ("about".into(), n.stable_key);
    }
    // 4. Recall by exact name or key
    if let Ok(items) = store.recall(token, 1) {
        if let Some(first) = items.first() {
            if first.name.as_deref() == Some(token) || first.key == token {
                return ("about".into(), first.key.clone());
            }
        }
    }

    // Unresolved: emit mentions edge
    let key = if token.contains('/') || token.contains('\\') || token.ends_with(".rs") || token.ends_with(".py") || token.ends_with(".ts") {
        format!("file:{token}")
    } else {
        format!("sym:{token}")
    };
    ("mentions".into(), key)
}

/// Index memory files (`.sqwai/memory/*.md`) and journal notes (`.sqwai/journal/*.jsonl`).
pub fn index_memory_subsystem(store: &mut impl GraphStore, root: &Path) -> Result<usize> {
    let mut total_indexed = 0;
    let mem_dir = root.join(".sqwai").join("memory");
    let journal_dir = root.join(".sqwai").join("journal");

    // 1. Index .sqwai/memory/*.md
    if mem_dir.exists() && mem_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&mem_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !file_name.ends_with(".md") {
                    continue;
                }

                let Ok(content) = fs::read_to_string(&path) else {
                    continue;
                };

                let rel_path = format!(".sqwai/memory/{file_name}");

                if file_name.eq_ignore_ascii_case("MEMORY.md") {
                    let parsed = parse_memory_md(&rel_path, &content);
                    let mut nodes = Vec::new();
                    let mut edges = Vec::new();

                    for (node, backticks) in parsed {
                        let from_key = node.stable_key.clone();
                        nodes.push(node);
                        for token in backticks {
                            let (kind, to_key) = resolve_mention(store, &token);
                            edges.push(Edge {
                                from: from_key.clone(),
                                to: to_key,
                                kind,
                                confidence: Some(100),
                                source: Some("memory".into()),
                                source_hash: None,
                                limitations: Vec::new(),
                                properties: BTreeMap::new(),
                            });
                        }
                    }

                    if store.replace_file_subgraph(&rel_path, &nodes, &edges, &[])? {
                        total_indexed += 1;
                    }
                } else {
                    // Date diary: YYYY-MM-DD.md
                    let date = file_name.trim_end_matches(".md");
                    let parsed = parse_diary(&rel_path, date, &content);
                    let mut nodes = Vec::new();
                    let mut edges = Vec::new();

                    for (node, backticks, supersedes_target) in parsed {
                        let from_key = node.stable_key.clone();
                        nodes.push(node);

                        for token in backticks {
                            let (kind, to_key) = resolve_mention(store, &token);
                            edges.push(Edge {
                                from: from_key.clone(),
                                to: to_key,
                                kind,
                                confidence: Some(100),
                                source: Some("memory".into()),
                                source_hash: None,
                                limitations: Vec::new(),
                                properties: BTreeMap::new(),
                            });
                        }

                        if let Some(target) = supersedes_target {
                            edges.push(Edge {
                                from: from_key,
                                to: target,
                                kind: "supersedes".into(),
                                confidence: Some(100),
                                source: Some("memory".into()),
                                source_hash: None,
                                limitations: Vec::new(),
                                properties: BTreeMap::new(),
                            });
                        }
                    }

                    if store.replace_file_subgraph(&rel_path, &nodes, &edges, &[])? {
                        total_indexed += 1;
                    }
                }
            }
        }
    }

    // 2. Index .sqwai/journal/*.jsonl note records
    if journal_dir.exists() && journal_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&journal_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !file_name.ends_with(".jsonl") {
                    continue;
                }

                let session_id = file_name.trim_end_matches(".jsonl");
                let Ok(content) = fs::read_to_string(&path) else {
                    continue;
                };

                let rel_path = format!(".sqwai/journal/{file_name}");
                let parsed = parse_journal_notes(&rel_path, session_id, &content);
                if parsed.is_empty() {
                    continue;
                }

                let mut nodes = Vec::new();
                let mut edges = Vec::new();

                for (node, backticks, resolves) in parsed {
                    let from_key = node.stable_key.clone();
                    nodes.push(node);

                    for token in backticks {
                        let (kind, to_key) = resolve_mention(store, &token);
                        edges.push(Edge {
                            from: from_key.clone(),
                            to: to_key,
                            kind,
                            confidence: Some(100),
                            source: Some("memory".into()),
                            source_hash: None,
                            limitations: Vec::new(),
                            properties: BTreeMap::new(),
                        });
                    }

                    if let Some(target_seq) = resolves {
                        edges.push(Edge {
                            from: from_key,
                            to: format!("mem:journal:{session_id}:{target_seq}"),
                            kind: "supersedes".into(),
                            confidence: Some(100),
                            source: Some("memory".into()),
                            source_hash: None,
                            limitations: Vec::new(),
                            properties: BTreeMap::new(),
                        });
                    }
                }

                if store.replace_file_subgraph(&rel_path, &nodes, &edges, &[])? {
                    total_indexed += 1;
                }
            }
        }
    }

    Ok(total_indexed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::graph::{Direction, GraphQuery, SqliteGraphStore};
    use tempfile::tempdir;

    #[test]
    fn extract_backticks_and_j_refs() {
        let text = "Decided `Session` and `src/session/mod.rs` use `save()`. Checked (j#16).";
        let bt = extract_backticks(text);
        assert_eq!(bt, vec!["Session", "src/session/mod.rs", "save"]);

        let j = extract_journal_refs(text);
        assert_eq!(j, vec![16]);
    }

    #[test]
    fn parse_diary_entry_and_resolve_edges() {
        let dir = tempdir().unwrap();
        let mut store = SqliteGraphStore::open(dir.path()).unwrap();

        // Add a code struct node in the store first
        let code_node = Node {
            stable_key: "sym:src/session/mod.rs::struct::Session".into(),
            kind: NodeKind::Struct,
            name: Some("Session".into()),
            path: Some("src/session/mod.rs".into()),
            language: Some("rust".into()),
            line_start: Some(1),
            line_end: Some(10),
            signature: Some("pub struct Session".into()),
            roles: Vec::new(),
            properties: BTreeMap::new(),
            content_hash: None,
        };
        store.replace_file_subgraph("src/session/mod.rs", &[code_node], &[], &[]).unwrap();

        // Create .sqwai/memory/2026-09-10.md
        let mem_dir = dir.path().join(".sqwai").join("memory");
        fs::create_dir_all(&mem_dir).unwrap();
        let diary_content = r#"
## 18:47 · session a8f2 · plan 01J123 · "Persist todos"

### Done
- Implemented `Session.todos` field.

### Decisions
- Todos live inside the session file for `Session`. (j#16)

### Corrections
- Correction regarding previous note (j#10).
"#;
        fs::write(mem_dir.join("2026-09-10.md"), diary_content).unwrap();

        let count = index_memory_subsystem(&mut store, dir.path()).unwrap();
        assert_eq!(count, 1);

        // Verify decision node exists and is connected to Session with `about` edge
        let query = GraphQuery {
            direction: Direction::Incoming,
            preset: None,
            depth: 1,
            max_nodes: 30,
            max_edges: 10,
            limit: 10,
            relations: vec!["about".into()],
            kinds: vec!["decision".into()],
        };
        let proj = store.graph_query("sym:src/session/mod.rs::struct::Session", query).unwrap();
        assert_eq!(proj.edges.len(), 1);
        assert_eq!(proj.edges[0].kind, "about");

        let dec_key = &proj.edges[0].from;
        let dec_node = store.find_node(dec_key).unwrap().expect("decision node must exist");
        assert_eq!(dec_node.kind, NodeKind::Decision);
        assert_eq!(dec_node.properties.get("author").and_then(|v| v.as_str()), Some("model"));
        assert_eq!(dec_node.properties.get("journal_ref").and_then(|v| v.as_str()), Some("j#16"));
        assert!(dec_node.properties.get("text").and_then(|v| v.as_str()).unwrap().contains("Todos live inside"));
    }

    #[test]
    fn journal_notes_supersedes_assumption() {
        let dir = tempdir().unwrap();
        let mut store = SqliteGraphStore::open(dir.path()).unwrap();

        let journal_dir = dir.path().join(".sqwai").join("journal");
        fs::create_dir_all(&journal_dir).unwrap();
        let notes_content = r#"{"seq":5,"ts":"2026-09-10T10:00:00Z","step":"1","kind":"note","by":"model","note":"assumption","text":"assuming `Cache` is warm"}
{"seq":8,"ts":"2026-09-10T10:05:00Z","step":"1","kind":"note","by":"model","note":"decision","text":"verified cache not warm, resolves assumption","resolves":5}
"#;
        fs::write(journal_dir.join("sess1.jsonl"), notes_content).unwrap();

        let count = index_memory_subsystem(&mut store, dir.path()).unwrap();
        assert_eq!(count, 1);

        // Find the decision node and verify supersedes edge to assumption node
        let proj = store.graph_query(
            "mem:journal:sess1:8",
            GraphQuery {
                direction: Direction::Outgoing,
                preset: None,
                depth: 1,
                max_nodes: 30,
                max_edges: 10,
                limit: 10,
                relations: vec!["supersedes".into()],
                kinds: Vec::new(),
            },
        ).unwrap();
        assert_eq!(proj.edges.len(), 1);
        assert_eq!(proj.edges[0].to, "mem:journal:sess1:5");
    }
}
