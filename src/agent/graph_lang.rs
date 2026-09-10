//! Tree-sitter language adapters (stage C): declarations, scopes, imports
//! and call occurrences for Rust, Python and TypeScript.
//!
//! Deliberately no call-graph resolution: a call site becomes an
//! `occurrence`, never a guessed `calls` edge. Import mapping is
//! lexical-only (relative paths, `mod` linkage, `super`/`crate` chains);
//! ambiguous paths are dropped, not guessed. Unknown node kinds are
//! ignored, so grammar drift degrades to fewer facts, never to a panic
//! or a false edge.

use super::graph::{Node, NodeKind, Occurrence};
use super::graph_index::{GraphBatch, SourceAdapter, edge, file_node};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use tree_sitter::{Language, Node as TsNode, Parser};

pub const RUST_ADAPTER_VERSION: &str = "1";
pub const PYTHON_ADAPTER_VERSION: &str = "1";
pub const TYPESCRIPT_ADAPTER_VERSION: &str = "1";

/// Syntactic capabilities every adapter in this module offers.
pub const TS_CAPABILITIES: &[&str] = &["declarations", "imports"];

/// Guards against pathological nesting (generated or adversarial files).
const MAX_WALK_DEPTH: usize = 128;
const MAX_SIGNATURE_CHARS: usize = 160;
const MAX_NAME_CHARS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsLang {
    Rust,
    Python,
    TypeScript,
    Tsx,
}

impl TsLang {
    pub fn for_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "rs" => Some(TsLang::Rust),
            "py" => Some(TsLang::Python),
            "ts" | "mts" | "cts" => Some(TsLang::TypeScript),
            "tsx" => Some(TsLang::Tsx),
            _ => None,
        }
    }

    fn grammar(&self) -> Language {
        match self {
            TsLang::Rust => tree_sitter_rust::LANGUAGE.into(),
            TsLang::Python => tree_sitter_python::LANGUAGE.into(),
            TsLang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            TsLang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }

    pub fn adapter_name(&self) -> &'static str {
        match self {
            TsLang::Rust => "rust",
            TsLang::Python => "python",
            TsLang::TypeScript | TsLang::Tsx => "typescript",
        }
    }

    pub fn adapter_version(&self) -> &'static str {
        match self {
            TsLang::Rust => RUST_ADAPTER_VERSION,
            TsLang::Python => PYTHON_ADAPTER_VERSION,
            TsLang::TypeScript | TsLang::Tsx => TYPESCRIPT_ADAPTER_VERSION,
        }
    }

    fn file_language(&self) -> &'static str {
        match self {
            TsLang::Rust => "rust",
            TsLang::Python => "python",
            TsLang::TypeScript | TsLang::Tsx => "typescript",
        }
    }
}

/// One scope level: rendered key fragment, whether bare functions under
/// it are methods, whether declarations under it carry the test role, and
/// whether it gets its own node (impl blocks scope without one).
struct ScopeElem {
    render: String,
    is_type_scope: bool,
    cfg_test: bool,
    emits_node: bool,
}

pub struct Decl {
    pub key: String,
    pub parent: String,
    #[allow(dead_code)]
    pub scope: Vec<String>,
    pub kind: NodeKind,
    pub name: String,
    pub roles: Vec<String>,
    pub line_start: u32,
    pub line_end: u32,
    pub signature: Option<String>,
}

/// Candidate target files (project-relative) for one import statement.
pub struct Import {
    pub files: Vec<String>,
}

pub struct Call {
    pub name: String,
    pub line: u32,
}

pub struct TsAnalysis {
    pub decls: Vec<Decl>,
    pub imports: Vec<Import>,
    pub calls: Vec<Call>,
}

struct Ctx<'a> {
    bytes: &'a [u8],
    path: &'a str,
    file_key: String,
    scope: Vec<ScopeElem>,
    ord: HashMap<String, usize>,
    test_file: bool,
    /// `#[test]` / `#[cfg(test)]` seen on a preceding sibling attribute:
    /// attributes are siblings of their item in the grammar, so the walker
    /// stashes them here for the next declaration to consume.
    pending_attr: Option<(bool, bool)>,
    decls: Vec<Decl>,
    imports: Vec<Import>,
    calls: Vec<Call>,
}

impl<'a> Ctx<'a> {
    fn text(&self, node: TsNode) -> Option<&'a str> {
        node.utf8_text(self.bytes).ok()
    }

    /// Record a declaration under the CURRENT scope (call before pushing
    /// the declaration's own scope element). The stable key carries the
    /// full scope chain; a repeated full key falls back to the `#<n>`
    /// ordinal (§2.4.3) instead of colliding.
    fn push_decl(
        &mut self,
        kind: NodeKind,
        word: &'static str,
        name: String,
        mut roles: Vec<String>,
        node: TsNode,
        body: Option<TsNode>,
    ) {
        if name.trim().is_empty() {
            return;
        }
        if self.test_file || self.scope.iter().any(|element| element.cfg_test) {
            roles.push("test".to_string());
        }
        roles.sort();
        roles.dedup();
        let scope: Vec<String> = self.scope.iter().map(|e| e.render.clone()).collect();
        let base = if scope.is_empty() {
            format!("sym:{}::{word}::{name}", self.path)
        } else {
            format!("sym:{}::{}::{word}::{name}", self.path, scope.join("::"))
        };
        let count = self.ord.entry(base.clone()).or_insert(0);
        *count += 1;
        let key = if *count == 1 {
            base
        } else {
            format!("{base}#{}", *count)
        };
        // the parent is the nearest scope level with its own node (impl
        // blocks scope without one), else the file itself
        let mut parent = self.file_key.clone();
        for (index, element) in self.scope.iter().enumerate() {
            if element.emits_node {
                parent = format!(
                    "sym:{}::{}",
                    self.path,
                    self.scope[..=index]
                        .iter()
                        .map(|e| e.render.as_str())
                        .collect::<Vec<_>>()
                        .join("::")
                );
            }
        }
        let line_start = node.start_position().row as u32 + 1;
        let line_end = node.end_position().row as u32 + 1;
        let signature = signature_of(self.bytes, node, body);
        self.decls.push(Decl {
            key,
            parent,
            scope,
            kind,
            name,
            roles,
            line_start,
            line_end: line_end.max(line_start),
            signature,
        });
    }

    fn push_scope(
        &mut self,
        render: String,
        is_type_scope: bool,
        cfg_test: bool,
        emits_node: bool,
    ) {
        let inherited = self.scope.last().is_some_and(|e| e.cfg_test);
        self.scope.push(ScopeElem {
            render,
            is_type_scope,
            cfg_test: cfg_test || inherited,
            emits_node,
        });
    }

    fn pop_scope(&mut self) {
        self.scope.pop();
    }

    /// Nearest enclosing type scope makes a bare function a method.
    fn in_type_scope(&self) -> bool {
        self.scope.last().is_some_and(|e| e.is_type_scope)
    }
}

/// First header line of a declaration (up to the body), whitespace
/// collapsed, truncated. Advisory context, never a fact.
fn signature_of(bytes: &[u8], node: TsNode, body: Option<TsNode>) -> Option<String> {
    let end = body
        .map(|b| b.start_byte())
        .unwrap_or_else(|| node.end_byte());
    let text = std::str::from_utf8(bytes.get(node.start_byte()..end)?).ok()?;
    let first = text.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return None;
    }
    let collapsed: String = first.split_whitespace().collect::<Vec<_>>().join(" ");
    let short: String = collapsed.chars().take(MAX_SIGNATURE_CHARS).collect();
    if short.is_empty() { None } else { Some(short) }
}

fn child_text<'b>(node: TsNode, bytes: &'b [u8], field: &str) -> Option<&'b str> {
    node.child_by_field_name(field)?.utf8_text(bytes).ok()
}

/// Skipped while hunting names: attributes and comments precede the real
/// name, and descending into them would return `tokio` for `#[tokio::test]`.
fn is_name_noise(kind: &str) -> bool {
    matches!(
        kind,
        "attribute_item" | "attribute" | "line_comment" | "block_comment" | "comment"
    )
}

/// First identifier-ish descendant, skipping attributes and comments.
/// Field-name drift degrades to this instead of losing the declaration.
fn first_ident<'b>(node: TsNode, bytes: &'b [u8]) -> Option<&'b str> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        let mut cursor = current.walk();
        let mut children = Vec::new();
        for child in current.children(&mut cursor) {
            if is_name_noise(child.kind()) {
                continue;
            }
            if child.kind() == "identifier" || child.kind().ends_with("_identifier") {
                if child.kind() == "type_identifier" {
                    // a type is not a name; keep looking at this level
                    children.push(child);
                    continue;
                }
                if let Ok(text) = child.utf8_text(bytes) {
                    return Some(text);
                }
            }
            children.push(child);
        }
        // depth-first in document order: push reversed
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    None
}

fn decl_name(node: TsNode, bytes: &[u8]) -> Option<String> {
    let name = child_text(node, bytes, "name")
        .or_else(|| first_ident(node, bytes))
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() || name.len() > MAX_NAME_CHARS {
        None
    } else {
        Some(name)
    }
}

/// Shared body finder: the `body` field first, then known container
/// kinds. Unknown shapes simply have no body to descend into.
fn body_child(node: TsNode) -> Option<TsNode> {
    if let Some(body) = node.child_by_field_name("body") {
        return Some(body);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "block"
            | "statement_block"
            | "declaration_list"
            | "class_body"
            | "interface_body"
            | "enum_body"
            | "field_declaration_list"
            | "compound_statement" => return Some(child),
            _ => {}
        }
    }
    None
}

/// Resolve `a/b/../c` lexically without touching the filesystem.
fn resolve_dots(dir: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn parent_dir(relative_path: &str) -> &str {
    relative_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("")
}

pub fn analyze(lang: TsLang, relative_path: &str, bytes: &[u8]) -> Result<TsAnalysis> {
    if std::str::from_utf8(bytes).is_err() {
        anyhow::bail!("not valid UTF-8; generic fallback covers the file node");
    }
    let mut parser = Parser::new();
    parser
        .set_language(&lang.grammar())
        .map_err(|error| anyhow::anyhow!("tree-sitter language failed: {error}"))?;
    let tree = parser
        .parse(bytes, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter parse failed"))?;
    let file_stem = Path::new(relative_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let in_tests_dir = Path::new(relative_path)
        .components()
        .any(|c| c.as_os_str() == "tests");
    let test_file = in_tests_dir
        || file_stem.starts_with("test_")
        || file_stem.ends_with(".test")
        || file_stem.ends_with(".spec");
    let mut ctx = Ctx {
        bytes,
        path: relative_path,
        file_key: format!("file:{relative_path}"),
        scope: Vec::new(),
        ord: HashMap::new(),
        test_file,
        pending_attr: None,
        decls: Vec::new(),
        imports: Vec::new(),
        calls: Vec::new(),
    };
    match lang {
        TsLang::Rust => walk_rust(tree.root_node(), &mut ctx, 0),
        TsLang::Python => walk_python(tree.root_node(), &mut ctx, 0),
        TsLang::TypeScript | TsLang::Tsx => walk_ts(tree.root_node(), &mut ctx, 0),
    }
    Ok(TsAnalysis {
        decls: ctx.decls,
        imports: ctx.imports,
        calls: ctx.calls,
    })
}

/// Recurse into children for nested declarations, calls and imports.
fn descend(node: TsNode, ctx: &mut Ctx, depth: usize, visit: fn(TsNode, &mut Ctx, usize)) {
    if depth >= MAX_WALK_DEPTH {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, ctx, depth + 1);
    }
}

/// Run `visit` over a body node with a scope pushed for its duration.
fn scoped_body(
    ctx: &mut Ctx,
    scope: (String, bool, bool, bool),
    body: Option<TsNode>,
    depth: usize,
    visit: fn(TsNode, &mut Ctx, usize),
) {
    ctx.push_scope(scope.0, scope.1, scope.2, scope.3);
    if let Some(body) = body {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            visit(child, ctx, depth + 1);
        }
    }
    ctx.pop_scope();
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

/// Rust item attributes: `#[test]` / `#[tokio::test]` mark test roles,
/// `#[cfg(test)]` marks the cfg flag. Textual on purpose: attribute paths
/// are stable text even when grammar field names drift.
fn rust_item_flags(node: TsNode, bytes: &[u8]) -> (bool, bool) {
    let mut is_test = false;
    let mut cfg_test = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "attribute_item" {
            continue;
        }
        let Ok(text) = child.utf8_text(bytes) else {
            continue;
        };
        let squashed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let inner = squashed.strip_prefix("#[").unwrap_or(&squashed);
        let inner = inner.strip_suffix(']').unwrap_or(inner);
        if inner == "test" || inner.ends_with("::test") {
            is_test = true;
        }
        if inner == "cfg(test)" {
            cfg_test = true;
        }
    }
    (is_test, cfg_test)
}

fn rust_impl_scope(node: TsNode, bytes: &[u8]) -> String {
    let ty = node
        .child_by_field_name("type")
        .and_then(|t| t.utf8_text(bytes).ok())
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty() && t.len() <= 96)
        .unwrap_or_else(|| "_".to_string());
    match node
        .child_by_field_name("trait")
        .and_then(|t| t.utf8_text(bytes).ok())
        .map(|t| {
            t.rsplit("::")
                .next()
                .unwrap_or(t)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|t| !t.is_empty())
    {
        Some(trait_name) => format!("impl<{trait_name} for {ty}>"),
        None => format!("impl<{ty}>"),
    }
}

/// Split a `use` tree on top-level `::`, respecting one brace level.
fn split_use_top(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            depth += 1;
            current.push(ch);
        } else if ch == '}' {
            depth = depth.saturating_sub(1);
            current.push(ch);
        } else if ch == ':' && depth == 0 && chars.peek() == Some(&':') {
            chars.next();
            out.push(std::mem::take(&mut current));
        } else {
            current.push(ch);
        }
    }
    out.push(current);
    out
}

fn strip_alias(segment: &str) -> &str {
    match segment.split_once(" as ") {
        Some((name, _)) => name.trim(),
        None => segment.trim(),
    }
}

fn split_group(text: &str) -> Vec<String> {
    text.split(',')
        .map(|member| strip_alias(member.trim()).to_string())
        .filter(|member| !member.is_empty() && !member.contains(['{', '}']))
        .collect()
}

/// Expand one `use` statement (without the leading keyword) into full
/// segment paths. Anything fancier than plain/grouped paths is skipped
/// rather than guessed.
fn expand_use_tree(text: &str) -> Vec<Vec<String>> {
    let text = text.trim().trim_start_matches("use").trim();
    let text = text.trim_end_matches(';').trim();
    if text.is_empty() || text.contains('*') {
        return Vec::new();
    }
    let segments = split_use_top(text);
    let mut out = Vec::new();
    expand_use_rec(&segments, Vec::new(), &mut out);
    out.into_iter()
        .filter(|path| !path.is_empty() && path[0] != "self")
        .collect()
}

fn expand_use_rec(segments: &[String], prefix: Vec<String>, out: &mut Vec<Vec<String>>) {
    let Some((head, tail)) = segments.split_first() else {
        return;
    };
    let head = strip_alias(head.trim());
    if head.is_empty() {
        return;
    }
    if head.starts_with('{') && head.ends_with('}') && tail.is_empty() {
        // bare `{A, B}` group (invalid Rust, but harmless to accept)
        for member in split_group(&head[1..head.len() - 1]) {
            let mut path = prefix.clone();
            path.push(member);
            out.push(path);
        }
        return;
    }
    if tail.is_empty() {
        if head != "self" {
            let mut path = prefix;
            path.push(head.to_string());
            out.push(path);
        }
        return;
    }
    // a group is only handled as the last tail element (`a::{B, C}`);
    // anything nested deeper is skipped rather than guessed
    if let Some((last, init)) = tail.split_last() {
        let last = last.trim();
        if last.starts_with('{') && last.ends_with('}') {
            let mut base = prefix;
            base.push(head.to_string());
            for member in init {
                let member = strip_alias(member.trim());
                if member.is_empty() || member.contains(['{', '}', '*']) {
                    return;
                }
                base.push(member.to_string());
            }
            for member in split_group(&last[1..last.len() - 1]) {
                let mut path = base.clone();
                path.push(member);
                out.push(path);
            }
            return;
        }
    }
    // plain dotted path
    let mut path = prefix;
    path.push(head.to_string());
    for segment in tail {
        let segment = strip_alias(segment.trim());
        if segment.is_empty() || segment.contains(['{', '}', '*']) {
            return;
        }
        path.push(segment.to_string());
    }
    out.push(path);
}

/// Map a `use` segment path to candidate files. Only unambiguous shapes:
/// `super::` chains (lexical), `crate::` under `src/`, `self` (same file,
/// no edge). Bare leading segments may be extern crates — skipped.
fn resolve_rust_use(file: &str, dir: &str, segments: &[String], in_src: bool) -> Vec<String> {
    let stem = file.rsplit('/').next().unwrap_or(file);
    let stem = stem.strip_suffix(".rs").unwrap_or(stem);
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    let mut segments = segments.iter().map(String::as_str).peekable();
    match segments.peek().copied().unwrap_or("") {
        "crate" => {
            if !in_src {
                return Vec::new();
            }
            parts = vec!["src"];
            segments.next();
        }
        "super" => {
            let mut supers = 0usize;
            while segments.peek() == Some(&"super") {
                supers += 1;
                segments.next();
            }
            if stem == "lib" || stem == "main" {
                return Vec::new();
            }
            // `mod.rs` is its directory, so every `super` climbs; `foo.rs`
            // already is its module, so the first `super` is free
            let pops = if stem == "mod" {
                supers
            } else {
                supers.saturating_sub(1)
            };
            for _ in 0..pops {
                if parts.pop().is_none() {
                    return Vec::new();
                }
            }
        }
        _ => return Vec::new(),
    }
    let rest: Vec<&str> = segments.collect();
    if rest.is_empty() {
        return Vec::new();
    }
    // all-but-last addresses the module; a lone segment names it directly
    // (`use foo;` imports the sibling module itself)
    let module: Vec<&str> = if rest.len() == 1 {
        rest.clone()
    } else {
        rest[..rest.len() - 1].to_vec()
    };
    let mut base = parts;
    base.extend_from_slice(&module);
    if base.is_empty() {
        return Vec::new();
    }
    let joined = base.join("/");
    vec![format!("{joined}.rs"), format!("{joined}/mod.rs")]
}

fn walk_rust(node: TsNode, ctx: &mut Ctx, depth: usize) {
    if depth >= MAX_WALK_DEPTH {
        return;
    }
    // Sibling attributes attach to the next declaration; anything else
    // (use items, macros) consumes and discards them.
    if node.kind() == "attribute_item" {
        if let Ok(text) = node.utf8_text(ctx.bytes) {
            let squashed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            if !squashed.starts_with("#![") {
                let inner = squashed.strip_prefix("#[").unwrap_or(&squashed);
                let inner = inner.strip_suffix(']').unwrap_or(inner);
                let is_test = inner == "test" || inner.ends_with("::test");
                let cfg_test = inner == "cfg(test)";
                if is_test || cfg_test {
                    let (test, cfg) = ctx.pending_attr.unwrap_or((false, false));
                    ctx.pending_attr = Some((test || is_test, cfg || cfg_test));
                }
            }
        }
        return;
    }
    // declaration arms take pending flags; use/macro arms drop them
    let take_pending = |ctx: &mut Ctx| ctx.pending_attr.take().unwrap_or((false, false));
    match node.kind() {
        "function_item" => {
            let Some(name) = decl_name(node, ctx.bytes) else {
                return descend(node, ctx, depth, walk_rust);
            };
            let (mut attr_test, mut cfg_test) = rust_item_flags(node, ctx.bytes);
            let (pending_test, pending_cfg) = take_pending(ctx);
            attr_test |= pending_test;
            cfg_test |= pending_cfg;
            let mut roles = Vec::new();
            if attr_test {
                roles.push("test".to_string());
            }
            let method = ctx.in_type_scope();
            let body = body_child(node);
            ctx.push_decl(
                if method {
                    NodeKind::Method
                } else {
                    NodeKind::Function
                },
                "fn",
                name.clone(),
                roles,
                node,
                body,
            );
            ctx.push_scope(format!("fn::{name}"), false, cfg_test, true);
            if let Some(body) = body {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    walk_rust(child, ctx, depth + 1);
                }
            }
            ctx.pop_scope();
        }
        "struct_item" | "enum_item" | "trait_item" | "union_item" => {
            let (kind, word) = match node.kind() {
                "struct_item" => (NodeKind::Struct, "struct"),
                "enum_item" => (NodeKind::Enum, "enum"),
                "trait_item" => (NodeKind::Trait, "trait"),
                _ => (NodeKind::Type, "union"),
            };
            let Some(name) = decl_name(node, ctx.bytes) else {
                return descend(node, ctx, depth, walk_rust);
            };
            let (_, own_cfg) = rust_item_flags(node, ctx.bytes);
            let (_, pending_cfg) = take_pending(ctx);
            let cfg_test = own_cfg || pending_cfg;
            let body = body_child(node);
            ctx.push_decl(kind, word, name.clone(), Vec::new(), node, body);
            scoped_body(
                ctx,
                (format!("{word}::{name}"), true, cfg_test, true),
                body,
                depth,
                walk_rust,
            );
        }
        "impl_item" => {
            // scope without a node: methods key under it, collisions fall
            // back to the `#<n>` ordinal
            let scope = rust_impl_scope(node, ctx.bytes);
            scoped_body(
                ctx,
                (scope, true, false, false),
                body_child(node),
                depth,
                walk_rust,
            );
        }
        "mod_item" => {
            let Some(name) = decl_name(node, ctx.bytes) else {
                return descend(node, ctx, depth, walk_rust);
            };
            // `#[cfg(test)] mod tests` propagates into the whole subtree
            let (_, cfg_test) = take_pending(ctx);
            let body = body_child(node);
            ctx.push_decl(
                NodeKind::Module,
                "mod",
                name.clone(),
                Vec::new(),
                node,
                body,
            );
            if body.is_none() {
                // `mod foo;` links sibling files by convention; the walked
                // set decides which candidate survives
                let dir = parent_dir(ctx.path);
                let base = if dir.is_empty() {
                    name.clone()
                } else {
                    format!("{dir}/{name}")
                };
                ctx.imports.push(Import {
                    files: vec![format!("{base}.rs"), format!("{base}/mod.rs")],
                });
                return;
            }
            scoped_body(
                ctx,
                (format!("mod::{name}"), false, cfg_test, true),
                body,
                depth,
                walk_rust,
            );
        }
        "type_item" | "const_item" | "static_item" | "macro_definition" => {
            let (kind, word) = match node.kind() {
                "type_item" => (NodeKind::Type, "type"),
                "const_item" => (NodeKind::Constant, "const"),
                "static_item" => (NodeKind::Variable, "static"),
                _ => (NodeKind::Macro, "macro"),
            };
            // leaf declarations consume pending attributes without effect
            let _ = take_pending(ctx);
            if let Some(name) = decl_name(node, ctx.bytes) {
                ctx.push_decl(kind, word, name, Vec::new(), node, None);
            }
            descend(node, ctx, depth, walk_rust);
        }
        "use_declaration" => {
            // attributes on imports (e.g. `#[cfg] use`) apply here, never
            // to a later declaration
            let _ = take_pending(ctx);
            let text = ctx.text(node).unwrap_or("");
            for segments in expand_use_tree(text) {
                let in_src = ctx.path == "src/lib.rs"
                    || ctx.path == "src/main.rs"
                    || ctx.path == "src/mod.rs"
                    || ctx.path.starts_with("src/");
                for file in resolve_rust_use(ctx.path, parent_dir(ctx.path), &segments, in_src) {
                    ctx.imports.push(Import { files: vec![file] });
                }
            }
        }
        "call_expression" => {
            if let Some(name) = call_func_name(node, ctx.bytes) {
                ctx.calls.push(Call {
                    name,
                    line: node.start_position().row as u32 + 1,
                });
            }
            descend(node, ctx, depth, walk_rust);
        }
        "macro_invocation" => {
            // `include!` literally includes a file: the one macro worth an
            // import edge. Every other macro is call-site noise.
            if let Some(literal) = include_literal(node, ctx.bytes) {
                let dir = parent_dir(ctx.path);
                let target = if dir.is_empty() {
                    literal
                } else {
                    resolve_dots(dir, &literal).unwrap_or(literal)
                };
                ctx.imports.push(Import {
                    files: vec![target],
                });
            }
        }
        _ => descend(node, ctx, depth, walk_rust),
    }
}

/// Function part of a call: identifier as-is; `a::b::c` and `x.foo`
/// resolve to their last segment. Field-first with an identifier scan
/// fallback, so grammar field drift degrades instead of breaking.
fn call_func_name(node: TsNode, bytes: &[u8]) -> Option<String> {
    let func = node.child_by_field_name("function")?;
    if func.kind() == "identifier" {
        return func.utf8_text(bytes).ok().map(str::to_string);
    }
    for field in ["name", "property", "field", "attribute"] {
        if let Some(child) = func.child_by_field_name(field)
            && let Ok(text) = child.utf8_text(bytes)
        {
            return Some(text.to_string());
        }
    }
    // last identifier descendant, skipping type arguments (`foo::<T>`
    // must yield `foo`, not `T`)
    let mut stack = vec![func];
    let mut last = None;
    while let Some(current) = stack.pop() {
        if current.kind() == "identifier" {
            last = current.utf8_text(bytes).ok();
        }
        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            if !matches!(child.kind(), "type_arguments" | "type_parameters") {
                stack.push(child);
            }
        }
    }
    last.map(str::to_string)
}

/// `include!("path")` literal, if this macro invocation is one.
fn include_literal(node: TsNode, bytes: &[u8]) -> Option<String> {
    let macro_name = node
        .child_by_field_name("macro")
        .or_else(|| {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .find(|c| c.kind() == "identifier")
        })?
        .utf8_text(bytes)
        .ok()?;
    if macro_name != "include" {
        return None;
    }
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "string_literal" {
            let text = current.utf8_text(bytes).ok()?;
            return Some(text.trim_matches('"').to_string());
        }
        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

fn walk_python(node: TsNode, ctx: &mut Ctx, depth: usize) {
    if depth >= MAX_WALK_DEPTH {
        return;
    }
    match node.kind() {
        "function_definition" | "async_function_definition" => {
            let Some(name) = decl_name(node, ctx.bytes) else {
                return descend(node, ctx, depth, walk_python);
            };
            let mut roles = Vec::new();
            if name.starts_with("test_") {
                roles.push("test".to_string());
            }
            let method = ctx.in_type_scope();
            let body = body_child(node);
            ctx.push_decl(
                if method {
                    NodeKind::Method
                } else {
                    NodeKind::Function
                },
                "fn",
                name.clone(),
                roles,
                node,
                body,
            );
            ctx.push_scope(format!("fn::{name}"), false, false, true);
            if let Some(body) = body {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    walk_python(child, ctx, depth + 1);
                }
            }
            ctx.pop_scope();
        }
        "class_definition" => {
            let Some(name) = decl_name(node, ctx.bytes) else {
                return descend(node, ctx, depth, walk_python);
            };
            let mut roles = Vec::new();
            if name.starts_with("Test") {
                roles.push("test".to_string());
            }
            let body = body_child(node);
            ctx.push_decl(NodeKind::Class, "class", name.clone(), roles, node, body);
            scoped_body(
                ctx,
                (format!("class::{name}"), true, false, true),
                body,
                depth,
                walk_python,
            );
        }
        "decorated_definition" => descend(node, ctx, depth, walk_python),
        "import_statement" | "import_from_statement" => {
            // absolute imports need sys.path knowledge the adapter must
            // not guess; relative ones resolve lexically against the file
            if let Some((dots, module, names)) = parse_python_import(node, ctx.bytes)
                && dots > 0
            {
                let dir = parent_dir(ctx.path);
                if module.is_empty() {
                    for name in names {
                        for file in python_module_files(dir, dots, &name) {
                            ctx.imports.push(Import { files: vec![file] });
                        }
                    }
                } else {
                    for file in python_module_files(dir, dots, &module) {
                        ctx.imports.push(Import { files: vec![file] });
                    }
                }
            }
        }
        "call" => {
            if let Some(name) = python_call_name(node, ctx.bytes) {
                ctx.calls.push(Call {
                    name,
                    line: node.start_position().row as u32 + 1,
                });
            }
            descend(node, ctx, depth, walk_python);
        }
        _ => descend(node, ctx, depth, walk_python),
    }
}

/// Textual import parse: `(leading dots, module, names)`. Absolute
/// imports (`dots == 0`) resolve to nothing — sys.path is unknowable here.
fn parse_python_import(node: TsNode, bytes: &[u8]) -> Option<(usize, String, Vec<String>)> {
    let text = node.utf8_text(bytes).ok()?;
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.starts_with("import ") {
        return Some((0, String::new(), Vec::new()));
    }
    let rest = collapsed.strip_prefix("from ")?;
    let (module, tail) = rest.split_once(" import ")?;
    let dots = module.chars().take_while(|c| *c == '.').count();
    let module = module.trim_start_matches('.').to_string();
    let names: Vec<String> = tail
        .trim_start_matches('(')
        .trim_end_matches(')')
        .split(',')
        .map(|name| {
            name.trim()
                .split_once(" as ")
                .map(|(n, _)| n.trim().to_string())
                .unwrap_or_else(|| name.trim().to_string())
        })
        .filter(|name| !name.is_empty() && name != "*")
        .collect();
    Some((dots, module, names))
}

/// `dots` leading dots walk up from the importing file's directory, then
/// the module path maps to `<mod>.py` or `<mod>/__init__.py`.
fn python_module_files(dir: &str, dots: usize, module: &str) -> Vec<String> {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for _ in 1..dots {
        if parts.pop().is_none() {
            return Vec::new();
        }
    }
    let mut parts: Vec<String> = parts.into_iter().map(str::to_string).collect();
    if !module.is_empty() {
        parts.extend(module.split('.').map(str::to_string));
    }
    if parts.is_empty() {
        return Vec::new();
    }
    let joined = parts.join("/");
    vec![format!("{joined}.py"), format!("{joined}/__init__.py")]
}

fn python_call_name(node: TsNode, bytes: &[u8]) -> Option<String> {
    let func = node.child_by_field_name("function")?;
    if func.kind() == "identifier" {
        return func.utf8_text(bytes).ok().map(str::to_string);
    }
    // attribute `a.b.c` → last identifier descendant
    let mut stack = vec![func];
    let mut last = None;
    while let Some(current) = stack.pop() {
        if current.kind() == "identifier" {
            last = current.utf8_text(bytes).ok();
        }
        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            stack.push(child);
        }
    }
    last.map(str::to_string)
}

// ---------------------------------------------------------------------------
// TypeScript / TSX
// ---------------------------------------------------------------------------

fn walk_ts(node: TsNode, ctx: &mut Ctx, depth: usize) {
    if depth >= MAX_WALK_DEPTH {
        return;
    }
    match node.kind() {
        "function_declaration" => {
            let Some(name) = decl_name(node, ctx.bytes) else {
                return descend(node, ctx, depth, walk_ts);
            };
            let body = body_child(node);
            ctx.push_decl(
                NodeKind::Function,
                "fn",
                name.clone(),
                Vec::new(),
                node,
                body,
            );
            ctx.push_scope(format!("fn::{name}"), false, false, true);
            if let Some(body) = body {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    walk_ts(child, ctx, depth + 1);
                }
            }
            ctx.pop_scope();
        }
        "class_declaration" | "interface_declaration" => {
            let (kind, word) = if node.kind() == "class_declaration" {
                (NodeKind::Class, "class")
            } else {
                (NodeKind::Interface, "interface")
            };
            let Some(name) = decl_name(node, ctx.bytes) else {
                return descend(node, ctx, depth, walk_ts);
            };
            let body = body_child(node);
            ctx.push_decl(kind, word, name.clone(), Vec::new(), node, body);
            scoped_body(
                ctx,
                (format!("{word}::{name}"), true, false, true),
                body,
                depth,
                walk_ts,
            );
        }
        "type_alias_declaration" | "enum_declaration" => {
            let (kind, word) = if node.kind() == "enum_declaration" {
                (NodeKind::Enum, "enum")
            } else {
                (NodeKind::Type, "type")
            };
            if let Some(name) = decl_name(node, ctx.bytes) {
                ctx.push_decl(kind, word, name, Vec::new(), node, None);
            }
            descend(node, ctx, depth, walk_ts);
        }
        "method_definition" => {
            let Some(name) = decl_name(node, ctx.bytes) else {
                return descend(node, ctx, depth, walk_ts);
            };
            let body = body_child(node);
            ctx.push_decl(NodeKind::Method, "fn", name.clone(), Vec::new(), node, body);
            ctx.push_scope(format!("method::{name}"), false, false, true);
            if let Some(body) = body {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    walk_ts(child, ctx, depth + 1);
                }
            }
            ctx.pop_scope();
        }
        "variable_declarator" => {
            // `const f = () => {}` is a function for outline purposes
            let value = node.child_by_field_name("value");
            let is_fn = value.is_some_and(|v| {
                matches!(
                    v.kind(),
                    "arrow_function" | "function_expression" | "function"
                )
            });
            if is_fn && let Some(name) = decl_name(node, ctx.bytes) {
                ctx.push_decl(NodeKind::Function, "fn", name, Vec::new(), node, None);
            }
            descend(node, ctx, depth, walk_ts);
        }
        "export_statement" => {
            // transparent for declarations, and `export … from "…"` carries
            // the same import target as a plain import
            if let Some(target) = ts_import_target(node, ctx.bytes) {
                for file in ts_resolve(ctx.path, &target) {
                    ctx.imports.push(Import { files: vec![file] });
                }
            }
            descend(node, ctx, depth, walk_ts);
        }
        "import_statement" | "import_require_declaration" => {
            if let Some(target) = ts_import_target(node, ctx.bytes) {
                for file in ts_resolve(ctx.path, &target) {
                    ctx.imports.push(Import { files: vec![file] });
                }
            }
        }
        "call_expression" | "new_expression" => {
            if let Some(name) = ts_call_name(node, ctx.bytes) {
                ctx.calls.push(Call {
                    name,
                    line: node.start_position().row as u32 + 1,
                });
            }
            descend(node, ctx, depth, walk_ts);
        }
        _ => descend(node, ctx, depth, walk_ts),
    }
}

/// First string literal under an import/export node: the module target.
fn ts_import_target(node: TsNode, bytes: &[u8]) -> Option<String> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "string" {
            let text = current.utf8_text(bytes).ok()?;
            return Some(text.trim_matches(['\'', '"']).to_string());
        }
        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

/// Lexical relative resolution with TS extension probing. Anything else
/// (packages, aliases) resolves to nothing — the adapter drops it.
fn ts_resolve(path: &str, target: &str) -> Vec<String> {
    if !(target.starts_with("./") || target.starts_with("../")) {
        return Vec::new();
    }
    let dir = parent_dir(path);
    let Some(base) = resolve_dots(dir, target) else {
        return Vec::new();
    };
    if base.ends_with(".ts") || base.ends_with(".tsx") || base.ends_with(".js") {
        return vec![base];
    }
    let mut out = Vec::new();
    for ext in ["ts", "tsx", "d.ts"] {
        out.push(format!("{base}.{ext}"));
    }
    for ext in ["ts", "tsx"] {
        out.push(format!("{base}/index.{ext}"));
    }
    out
}

fn ts_call_name(node: TsNode, bytes: &[u8]) -> Option<String> {
    let field = if node.kind() == "new_expression" {
        "constructor"
    } else {
        "function"
    };
    let func = node.child_by_field_name(field)?;
    if func.kind() == "identifier" {
        return func.utf8_text(bytes).ok().map(str::to_string);
    }
    for probe in ["property", "name"] {
        if let Some(child) = func.child_by_field_name(probe)
            && let Ok(text) = child.utf8_text(bytes)
        {
            return Some(text.to_string());
        }
    }
    // last identifier-ish descendant (`a.b.c` → `c`)
    let mut stack = vec![func];
    let mut last = None;
    while let Some(current) = stack.pop() {
        if current.kind() == "identifier" || current.kind().ends_with("_identifier") {
            last = current.utf8_text(bytes).ok();
        }
        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            stack.push(child);
        }
    }
    last.map(str::to_string)
}

// ---------------------------------------------------------------------------
// Adapter glue
// ---------------------------------------------------------------------------

pub struct TsAdapter(pub TsLang);

impl SourceAdapter for TsAdapter {
    fn supports(&self, path: &Path) -> bool {
        TsLang::for_path(path) == Some(self.0)
    }

    fn index(&self, relative_path: &str, content: &[u8]) -> Result<GraphBatch> {
        let analysis = analyze(self.0, relative_path, content)?;
        let adapter = self.0.adapter_name();
        let version = self.0.adapter_version();
        let mut batch = GraphBatch::default();
        let file = format!("file:{relative_path}");
        batch.nodes.push(file_node(
            relative_path,
            content,
            Some(self.0.file_language()),
            adapter,
            version,
            TS_CAPABILITIES,
        ));
        for decl in &analysis.decls {
            batch.nodes.push(Node {
                stable_key: decl.key.clone(),
                kind: decl.kind.clone(),
                name: Some(decl.name.clone()),
                path: Some(relative_path.to_string()),
                language: Some(self.0.file_language().to_string()),
                line_start: Some(decl.line_start),
                line_end: Some(decl.line_end),
                signature: decl.signature.clone(),
                roles: decl.roles.clone(),
                properties: adapter_props(adapter, version),
                content_hash: None,
            });
            batch
                .edges
                .push(edge(&decl.parent, &decl.key, "contains", adapter));
        }
        for import in &analysis.imports {
            for target in &import.files {
                let mut relation = edge(&file, &format!("file:{target}"), "imports", adapter);
                relation.limitations = vec!["lexical-only".to_string()];
                batch.edges.push(relation);
            }
        }
        let source_hash = content_hash(content);
        for call in &analysis.calls {
            batch.occurrences.push(Occurrence {
                path: relative_path.to_string(),
                name: call.name.clone(),
                kind: "call".to_string(),
                line: call.line,
                source_hash: source_hash.clone(),
            });
        }
        Ok(batch)
    }
}

fn adapter_props(
    adapter: &str,
    version: &str,
) -> std::collections::BTreeMap<String, serde_json::Value> {
    let mut props = std::collections::BTreeMap::new();
    props.insert(
        "source_adapter".to_string(),
        serde_json::Value::String(adapter.to_string()),
    );
    props.insert(
        "adapter_version".to_string(),
        serde_json::Value::String(version.to_string()),
    );
    props
}

fn content_hash(content: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(content))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze_str(lang: TsLang, path: &str, src: &str) -> TsAnalysis {
        analyze(lang, path, src.as_bytes()).expect("fixture parses")
    }

    #[test]
    fn rust_declarations_scopes_and_keys() {
        let analysis = analyze_str(
            TsLang::Rust,
            "src/session/mod.rs",
            r#"
pub struct Session { id: u64 }

impl Session {
    pub fn save(&self) {}
}

#[test]
fn roundtrip() {}

mod inner {
    pub fn helper() {}
}
"#,
        );
        let keys: Vec<&str> = analysis.decls.iter().map(|d| d.key.as_str()).collect();
        assert!(
            keys.contains(&"sym:src/session/mod.rs::struct::Session"),
            "{keys:?}"
        );
        assert!(
            keys.contains(&"sym:src/session/mod.rs::impl<Session>::fn::save"),
            "{keys:?}"
        );
        assert!(
            keys.contains(&"sym:src/session/mod.rs::fn::roundtrip"),
            "{keys:?}"
        );
        assert!(
            keys.contains(&"sym:src/session/mod.rs::mod::inner"),
            "{keys:?}"
        );
        assert!(
            keys.contains(&"sym:src/session/mod.rs::mod::inner::fn::helper"),
            "{keys:?}"
        );
        let roundtrip = analysis
            .decls
            .iter()
            .find(|d| d.name == "roundtrip")
            .unwrap();
        assert!(roundtrip.roles.contains(&"test".to_string()));
        let save = analysis.decls.iter().find(|d| d.name == "save").unwrap();
        assert!(matches!(save.kind, NodeKind::Method));
        assert!(save.roles.is_empty());
        assert_eq!(save.scope, vec!["impl<Session>".to_string()]);
        assert_eq!(save.parent, "file:src/session/mod.rs");
    }

    #[test]
    fn rust_use_and_mod_imports_resolve_lexically() {
        let analysis = analyze_str(
            TsLang::Rust,
            "src/agent/worker.rs",
            "use super::graph::{Edge, Node};\nuse crate::plan::Plan;\nmod helper;\n",
        );
        let mut files: Vec<&str> = analysis
            .imports
            .iter()
            .flat_map(|i| i.files.iter().map(String::as_str))
            .collect();
        files.sort();
        assert!(
            files.contains(&"src/agent/graph.rs"),
            "super:: chain: {files:?}"
        );
        assert!(
            files.contains(&"src/plan.rs"),
            "crate:: under src/: {files:?}"
        );
        assert!(
            files.contains(&"src/agent/helper.rs"),
            "mod decl: {files:?}"
        );
    }

    #[test]
    fn rust_calls_become_occurrences_not_edges() {
        let analysis = analyze_str(
            TsLang::Rust,
            "src/main.rs",
            "fn main() {\n    let x = foo::bar();\n    y.baz(1);\n}\n",
        );
        let mut calls: Vec<&str> = analysis.calls.iter().map(|c| c.name.as_str()).collect();
        calls.sort();
        assert_eq!(calls, vec!["bar", "baz"]);
        assert!(
            analysis
                .imports
                .iter()
                .all(|i| i.files.iter().all(|f| !f.is_empty()))
        );
    }

    #[test]
    fn python_decls_imports_calls_and_test_roles() {
        let analysis = analyze_str(
            TsLang::Python,
            "app/models.py",
            "import os\nfrom .util import helper\n\nclass User:\n    def save(self):\n        helper()\n\ndef test_roundtrip():\n    pass\n",
        );
        let keys: Vec<&str> = analysis.decls.iter().map(|d| d.key.as_str()).collect();
        assert!(keys.contains(&"sym:app/models.py::class::User"), "{keys:?}");
        assert!(
            keys.contains(&"sym:app/models.py::class::User::fn::save"),
            "{keys:?}"
        );
        // absolute imports are skipped (sys.path unknowable); relative ones map
        let files: Vec<&str> = analysis
            .imports
            .iter()
            .flat_map(|i| i.files.iter().map(String::as_str))
            .collect();
        assert!(files.contains(&"app/util.py"), "{files:?}");
        assert!(!files.iter().any(|f| f.contains("os")), "{files:?}");
        let save = analysis.decls.iter().find(|d| d.name == "save").unwrap();
        assert!(matches!(save.kind, NodeKind::Method));
        let test = analysis
            .decls
            .iter()
            .find(|d| d.name == "test_roundtrip")
            .unwrap();
        assert!(test.roles.contains(&"test".to_string()));
        let calls: Vec<&str> = analysis.calls.iter().map(|c| c.name.as_str()).collect();
        assert!(calls.contains(&"helper"), "{calls:?}");
    }

    #[test]
    fn typescript_decls_imports_calls() {
        let analysis = analyze_str(
            TsLang::TypeScript,
            "src/app.ts",
            "import { helper } from './util';\n\nexport class App {\n  run() {\n    helper();\n  }\n}\n\nexport function boot(): void {}\n\nconst lazy = async () => {};\n",
        );
        let keys: Vec<&str> = analysis.decls.iter().map(|d| d.key.as_str()).collect();
        assert!(keys.contains(&"sym:src/app.ts::class::App"), "{keys:?}");
        assert!(
            keys.contains(&"sym:src/app.ts::class::App::fn::run"),
            "{keys:?}"
        );
        assert!(keys.contains(&"sym:src/app.ts::fn::boot"), "{keys:?}");
        assert!(keys.contains(&"sym:src/app.ts::fn::lazy"), "{keys:?}");
        let files: Vec<&str> = analysis
            .imports
            .iter()
            .flat_map(|i| i.files.iter().map(String::as_str))
            .collect();
        assert!(files.contains(&"src/util.ts"), "{files:?}");
        let calls: Vec<&str> = analysis.calls.iter().map(|c| c.name.as_str()).collect();
        assert!(calls.contains(&"helper"), "{calls:?}");
    }

    #[test]
    fn tsx_and_malformed_inputs_stay_honest() {
        // tsx grammar parses component files
        let tsx = analyze_str(
            TsLang::Tsx,
            "src/view.tsx",
            "export function View() {\n  return null;\n}\n",
        );
        assert!(tsx.decls.iter().any(|d| d.name == "View"));
        // garbage bytes: no panic, whatever survives is returned
        for (lang, bytes) in [
            (TsLang::Rust, b"fn broken( { (((".to_vec()),
            (TsLang::Python, b"def broken(:\n  ???".to_vec()),
            (TsLang::TypeScript, b"function ((( ".to_vec()),
        ] {
            let analysis = analyze(lang, "x", &bytes).expect("never fails");
            let _ = analysis.decls.len() + analysis.calls.len();
        }
        // non-UTF8 bails to the generic fallback
        assert!(analyze(TsLang::Rust, "x", &[0xff, 0xfe]).is_err());
    }

    #[test]
    fn registry_maps_extensions_without_core_changes() {
        use std::path::Path;
        assert_eq!(TsLang::for_path(Path::new("a.rs")), Some(TsLang::Rust));
        assert_eq!(TsLang::for_path(Path::new("a.py")), Some(TsLang::Python));
        assert_eq!(
            TsLang::for_path(Path::new("a.ts")),
            Some(TsLang::TypeScript)
        );
        assert_eq!(TsLang::for_path(Path::new("a.tsx")), Some(TsLang::Tsx));
        assert_eq!(TsLang::for_path(Path::new("a.go")), None);
        assert_eq!(TsLang::for_path(Path::new("a.md")), None);
        for lang in [
            TsLang::Rust,
            TsLang::Python,
            TsLang::TypeScript,
            TsLang::Tsx,
        ] {
            assert_eq!(lang.adapter_version(), "1");
        }
    }
}
