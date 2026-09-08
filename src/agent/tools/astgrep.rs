//! `ast_grep`: structural code search with ast-grep-style patterns.
//!
//! A pattern like `Ok($VAL)` or `f($$$ARGS)` is parsed with the same
//! tree-sitter grammar as the searched code, then matched structurally:
//! `$NAME` captures exactly one node (any kind, any size), `$$$NAME`
//! captures zero or more consecutive siblings, uppercase names are
//! metavariables, everything else must match node-for-node. Comments are
//! ignored on both sides, so a pattern still matches across a trailing
//! comment.

use super::{Outcome, ToolCtx};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use tree_sitter::Node;

const MAX_FILE_BYTES: u64 = 512_000;
const DEFAULT_MAX_MATCHES: usize = 50;
const HARD_MAX_MATCHES: usize = 200;
/// placeholders replace metavariables before parsing; valid identifiers in
/// every supported grammar, and unlikely to occur in real code
fn placeholder(i: usize) -> String {
    format!("ZqMeta{i}Zq")
}

#[derive(Clone, Copy, PartialEq)]
enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Bash,
    C,
    Cpp,
    CSharp,
    Java,
}

impl Lang {
    fn from_name(name: &str) -> Option<Lang> {
        Some(match name {
            "rust" => Lang::Rust,
            "python" => Lang::Python,
            "javascript" => Lang::JavaScript,
            "typescript" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "go" => Lang::Go,
            "bash" => Lang::Bash,
            "c" => Lang::C,
            "cpp" | "c++" => Lang::Cpp,
            "csharp" | "c#" | "cs" => Lang::CSharp,
            "java" => Lang::Java,
            _ => return None,
        })
    }

    fn from_extension(ext: &str) -> Option<Lang> {
        Some(match ext {
            "rs" => Lang::Rust,
            "py" => Lang::Python,
            "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "go" => Lang::Go,
            "sh" | "bash" => Lang::Bash,
            "c" | "h" => Lang::C,
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Lang::Cpp,
            "cs" => Lang::CSharp,
            "java" => Lang::Java,
            _ => return None,
        })
    }

    fn name(&self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::JavaScript => "javascript",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::Go => "go",
            Lang::Bash => "bash",
            Lang::C => "c",
            Lang::Cpp => "cpp",
            Lang::CSharp => "csharp",
            Lang::Java => "java",
        }
    }

    fn ts(&self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Bash => tree_sitter_bash::LANGUAGE.into(),
            Lang::C => tree_sitter_c::LANGUAGE.into(),
            Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Lang::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
        }
    }

    fn is_comment(&self, kind: &str) -> bool {
        match self {
            Lang::Rust => matches!(kind, "line_comment" | "block_comment"),
            _ => kind == "comment" || kind == "line_comment" || kind == "block_comment",
        }
    }
}

#[derive(Debug, Clone)]
enum Meta {
    Single(String),
    Multi(String),
}

/// The parsed pattern, mirrored into an owned tree.
#[derive(Debug, Clone)]
struct PNode {
    kind: u16,
    kind_name: String,
    /// named nodes match structurally; unnamed ones are literal tokens
    named: bool,
    /// exact text for leaves (no children)
    text: Option<String>,
    children: Vec<PNode>,
    meta: Option<Meta>,
}

#[derive(Default, Clone)]
struct Binds {
    single: HashMap<String, String>,
    multi: HashMap<String, String>,
}

impl Binds {
    fn render(&self) -> String {
        let mut parts: Vec<String> = self
            .single
            .iter()
            .map(|(k, v)| format!("${k} = {}", clip(v, 40)))
            .collect();
        for (k, v) in &self.multi {
            parts.push(format!("$$${k} = {}", clip(v, 60)));
        }
        parts.sort();
        parts.join(", ")
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let taken: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{taken}…")
}

/// Replace `$NAME` / `$$$NAME` metavariables with parseable placeholders and
/// return the mapping. Lowercase `$name` is left alone (a literal `$` token).
fn extract_metas(pattern: &str) -> Result<(String, Vec<Meta>), String> {
    use std::sync::OnceLock;
    static META_RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = META_RE.get_or_init(|| {
        regex::Regex::new(r"\$\$\$[A-Z_][A-Z0-9_]*|\$\$[A-Z_][A-Z0-9_]*|\$[A-Z_][A-Z0-9_]*")
            .unwrap()
    });
    let mut metas: Vec<Meta> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let replaced = re.replace_all(pattern, |caps: &regex::Captures| {
        let raw = &caps[0];
        let (name, multi) = if let Some(rest) = raw.strip_prefix("$$$") {
            (rest.to_string(), true)
        } else {
            (raw.trim_start_matches('$').to_string(), false)
        };
        let next = metas.len();
        let i = *index.entry(raw.to_string()).or_insert(next);
        if i == next {
            metas.push(if multi {
                Meta::Multi(name)
            } else {
                Meta::Single(name)
            });
        }
        placeholder(i)
    });
    Ok((replaced.into_owned(), metas))
}

fn build_pattern(node: Node, src: &str, lang: Lang, metas: &[Meta]) -> Option<PNode> {
    let text = node.utf8_text(src.as_bytes()).ok()?.to_string();
    // deepest node spanning exactly a placeholder is the metavariable
    let is_meta_leaf = metas
        .iter()
        .enumerate()
        .any(|(i, _)| text == placeholder(i))
        && !{
            let mut cursor = node.walk();
            node.children(&mut cursor).any(|child| {
                child
                    .utf8_text(src.as_bytes())
                    .map(|t| metas.iter().enumerate().any(|(i, _)| t == placeholder(i)))
                    .unwrap_or(false)
            })
        };
    if is_meta_leaf {
        // placeholders are numbered: find which one this node spans
        let idx = (0..metas.len()).find(|&i| text == placeholder(i))?;
        return Some(PNode {
            kind: node.kind_id(),
            kind_name: node.kind().to_string(),
            named: node.is_named(),
            text: None,
            children: Vec::new(),
            meta: Some(metas[idx].clone()),
        });
    }
    let mut children = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_missing() || lang.is_comment(child.kind()) {
            continue;
        }
        children.push(build_pattern(child, src, lang, metas)?);
    }
    if children.is_empty() {
        Some(PNode {
            kind: node.kind_id(),
            kind_name: node.kind().to_string(),
            named: node.is_named(),
            text: Some(text),
            children: Vec::new(),
            meta: None,
        })
    } else {
        Some(PNode {
            kind: node.kind_id(),
            kind_name: node.kind().to_string(),
            named: node.is_named(),
            text: None,
            children,
            meta: None,
        })
    }
}

/// Compile a pattern into a PNode root for `lang`.
fn compile_pattern(pattern: &str, lang: Lang) -> Result<PNode, String> {
    let (replaced, metas) = extract_metas(pattern)?;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.ts())
        .map_err(|e| format!("grammar load failed for {}: {e}", lang.name()))?;
    let tree = parser
        .parse(&replaced, None)
        .ok_or_else(|| "pattern parse failed".to_string())?;
    let root = tree.root_node();
    if !contains_reject_node(root) {
        let mut pat = build_pattern(root, &replaced, lang, &metas)
            .ok_or_else(|| "pattern build failed (invalid utf-8?)".to_string())?;
        while pat.text.is_none() && pat.children.len() == 1 {
            pat = pat.children.pop().expect("len checked");
        }
        return Ok(pat);
    }

    // Many languages (C, C++, Java, C#, Go) do not allow bare expressions or
    // statements at the file root. Try wrapping in synthetic wrappers.
    let candidates = match lang {
        Lang::C | Lang::Cpp => vec![
            format!("void _ZqWrap() {{\n{replaced}\n;}}"),
            format!("{replaced};"),
        ],
        Lang::Java | Lang::CSharp => vec![
            format!("class _ZqWrap {{\n{replaced}\n}}"),
            format!("class _ZqWrap {{\nvoid _ZqWrap() {{\n{replaced}\n;}}\n}}"),
        ],
        Lang::Go => vec![
            format!("package _zq\n{replaced}"),
            format!("package _zq\nfunc _ZqWrap() {{\n{replaced}\n}}"),
        ],
        _ => Vec::new(),
    };
    for wrapped in candidates {
        if let Some(wrapped_tree) = parser.parse(&wrapped, None)
            && !contains_reject_node(wrapped_tree.root_node())
        {
            let offset = wrapped.find(&replaced).unwrap_or(0);
            let root = wrapped_tree.root_node();
            let target_node = root
                .descendant_for_byte_range(offset, offset + replaced.len())
                .unwrap_or(root);
            if let Some(mut pat) = build_pattern(target_node, &wrapped, lang, &metas) {
                while pat.text.is_none() && pat.children.len() == 1 {
                    pat = pat.children.pop().expect("len checked");
                }
                return Ok(pat);
            }
        }
    }

    Err(format!(
        "pattern does not parse as {}; check the syntax (metavariables: $NAME, $$$NAME with uppercase names)",
        lang.name()
    ))
}

fn contains_reject_node(root: Node) -> bool {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_error() {
            return true;
        }
        // a missing `;` closing an expression_statement is the normal shape
        // of a snippet; every other missing token means truncated syntax
        if node.is_missing()
            && !(node.kind() == ";"
                && node
                    .parent()
                    .is_some_and(|p| p.kind() == "expression_statement"))
        {
            return true;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}

fn target_children<'t>(node: Node<'t>, lang: Lang) -> Vec<Node<'t>> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if lang.is_comment(child.kind()) {
            continue;
        }
        out.push(child);
    }
    out
}

fn bind_single(binds: &mut Binds, name: &str, text: &str) -> bool {
    match binds.single.get(name) {
        Some(prev) => prev == text,
        None => {
            binds.single.insert(name.to_string(), text.to_string());
            true
        }
    }
}

fn bind_multi(binds: &mut Binds, name: &str, joined: &str) -> bool {
    match binds.multi.get(name) {
        Some(prev) => prev == joined,
        None => {
            binds.multi.insert(name.to_string(), joined.to_string());
            true
        }
    }
}

fn match_one(pat: &PNode, tree: Node, lang: Lang, src: &[u8], binds: &mut Binds) -> bool {
    if let Some(meta) = &pat.meta {
        let text = match tree.utf8_text(src) {
            Ok(t) => t.to_string(),
            Err(_) => return false,
        };
        return match meta {
            Meta::Single(name) => bind_single(binds, name, &text),
            Meta::Multi(_) => false, // multi metas are matched in sequences
        };
    }
    let kinds_match = pat.kind == tree.kind_id() || {
        let tk = tree.kind();
        let pk = &pat.kind_name;
        ((pk == "identifier" || pk == "field_identifier" || pk == "property_identifier")
            && (tk == "identifier" || tk == "field_identifier" || tk == "property_identifier"))
            || ((pk == "method_declaration" || pk == "local_function_statement")
                && (tk == "method_declaration" || tk == "local_function_statement"))
    };
    if !kinds_match {
        return false;
    }
    if pat.children.is_empty() {
        // leaf: exact text, and the target must be a leaf too
        if tree.child_count() != 0 {
            return false;
        }
        return tree
            .utf8_text(src)
            .map(|t| pat.text.as_deref() == Some(t))
            .unwrap_or(false);
    }
    let tchildren = target_children(tree, lang);
    match_seq(&pat.children, &tchildren, lang, src, binds)
}

fn match_seq(pats: &[PNode], trees: &[Node], lang: Lang, src: &[u8], binds: &mut Binds) -> bool {
    let Some((first, rest)) = pats.split_first() else {
        return trees.is_empty();
    };
    if let Some(Meta::Multi(name)) = &first.meta {
        let name = name.clone();
        for k in 0..=trees.len() {
            let mut trial = binds.clone();
            let joined = trees[..k]
                .iter()
                .map(|t| t.utf8_text(src).unwrap_or_default().to_string())
                .collect::<Vec<_>>()
                .join(" ");
            if bind_multi(&mut trial, &name, &joined)
                && match_seq(rest, &trees[k..], lang, src, &mut trial)
            {
                *binds = trial;
                return true;
            }
        }
        return false;
    }
    if trees.is_empty() {
        return false;
    }
    let mut trial = binds.clone();
    if match_one(first, trees[0], lang, src, &mut trial)
        && match_seq(rest, &trees[1..], lang, src, &mut trial)
    {
        *binds = trial;
        return true;
    }
    false
}

/// Try the pattern at `node` and every descendant, return the first matching
/// node and its bindings.
fn search_tree<'t>(
    root: Node<'t>,
    pat: &PNode,
    lang: Lang,
    src: &[u8],
) -> Option<(Node<'t>, Binds)> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut binds = Binds::default();
        if match_one(pat, node, lang, src, &mut binds) {
            return Some((node, binds));
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

/// The `ast_grep` tool: structural search over the project tree.
pub fn ast_grep(ctx: &mut ToolCtx, args: &Value) -> Outcome {
    let Some(pattern) = args["pattern"].as_str().map(str::trim) else {
        return Outcome::err("ast_grep requires a 'pattern'");
    };
    if pattern.is_empty() {
        return Outcome::err("ast_grep pattern is empty");
    }
    if pattern.len() > 500 {
        return Outcome::err("ast_grep pattern is too long (maximum 500 bytes)");
    }
    let lang_arg = args["lang"].as_str().and_then(Lang::from_name);
    if args["lang"].as_str().is_some_and(|_| lang_arg.is_none()) {
        return Outcome::err(
            "unknown lang: use rust, python, javascript, typescript, tsx, go, bash, c, cpp, csharp, or java",
        );
    }
    let path_arg = args["path"].as_str().unwrap_or(".");
    let target = match ctx.resolve(path_arg) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    if !target.exists() {
        return Outcome::err(format!("path '{path_arg}' does not exist"));
    }
    let max = args["max"]
        .as_u64()
        .unwrap_or(DEFAULT_MAX_MATCHES as u64)
        .clamp(1, HARD_MAX_MATCHES as u64) as usize;
    let include = match args["include"].as_str() {
        Some(g) if !g.trim().is_empty() => {
            let glob = globset::Glob::new(g.trim())
                .ok()
                .map(|g| g.compile_matcher());
            if glob.is_none() {
                return Outcome::err(format!("bad include glob '{g}'"));
            }
            glob
        }
        _ => None,
    };

    // collect candidate files
    let mut files: Vec<PathBuf> = Vec::new();
    if target.is_file() {
        files.push(target.clone());
    } else {
        let walker = ignore::WalkBuilder::new(&target)
            .hidden(true)
            .git_ignore(true)
            .build();
        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let rel = path.strip_prefix(&target).unwrap_or(path);
            if let Some(glob) = &include
                && !glob.is_match(rel)
                && !glob.is_match(path)
            {
                continue;
            }
            let ext_ok = path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(Lang::from_extension)
                .map(|l| lang_arg.is_none_or(|chosen| l == chosen))
                .unwrap_or(false);
            if ext_ok {
                files.push(path.to_path_buf());
            }
            if files.len() >= 2_000 {
                break;
            }
        }
    }

    let mut roots: Vec<(Lang, PNode)> = Vec::new();
    let mut compiled = |lang: Lang, pattern: &str| -> Option<PNode> {
        if let Some((_, cached)) = roots.iter().find(|(l, _)| *l == lang) {
            return Some(cached.clone());
        }
        match compile_pattern(pattern, lang) {
            Ok(p) => {
                roots.push((lang, p.clone()));
                Some(p)
            }
            Err(_) => None,
        }
    };

    let mut lines: Vec<String> = Vec::new();
    let mut files_scanned = 0usize;
    let mut files_skipped = 0usize;
    let mut pattern_error: Option<String> = None;
    'files: for file in &files {
        let Ok(meta) = std::fs::metadata(file) else {
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            files_skipped += 1;
            continue;
        }
        let Ok(src) = std::fs::read_to_string(file) else {
            files_skipped += 1;
            continue;
        };
        let lang = match lang_arg {
            Some(chosen) => chosen,
            None => {
                let ext = file
                    .extension()
                    .and_then(|e| e.to_str())
                    .and_then(Lang::from_extension);
                match ext {
                    Some(l) => l,
                    None => {
                        return Outcome::err(format!(
                            "cannot infer the language of {} — pass 'lang' explicitly",
                            file.display()
                        ));
                    }
                }
            }
        };
        let Some(pat) = compiled(lang, pattern) else {
            pattern_error = match compile_pattern(pattern, lang) {
                Err(e) => Some(e),
                Ok(_) => Some(format!("pattern does not compile as {}", lang.name())),
            };
            break;
        };
        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&lang.ts()).is_err() {
            files_skipped += 1;
            continue;
        }
        let Some(tree) = parser.parse(&src, None) else {
            files_skipped += 1;
            continue;
        };
        files_scanned += 1;
        let disp = file
            .strip_prefix(&ctx.root)
            .unwrap_or(file)
            .to_string_lossy();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            let mut binds = Binds::default();
            if match_one(&pat, node, lang, src.as_bytes(), &mut binds) {
                let text = node
                    .utf8_text(src.as_bytes())
                    .unwrap_or_default()
                    .replace('\n', " ");
                let text = clip(text.trim(), 160);
                let binding_note = binds.render();
                lines.push(format!(
                    "{}:{}:{}: {}{}",
                    disp,
                    node.start_position().row + 1,
                    node.start_position().column + 1,
                    text,
                    if binding_note.is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", binding_note)
                    }
                ));
                if lines.len() >= max {
                    break 'files;
                }
            }
            let mut c = node.walk();
            for child in node.children(&mut c) {
                stack.push(child);
            }
        }
    }

    if let Some(err) = pattern_error {
        return Outcome::err(format!(
            "{err}; the first file with a supported extension decides the language — \
             pass 'lang' explicitly to control this"
        ));
    }
    if lines.is_empty() {
        return Outcome::ok(format!(
            "0 matches for pattern `{pattern}` ({} file(s) scanned, {files_skipped} skipped)",
            files.len()
        ));
    }
    let mut out = format!(
        "{} match(es) for `{pattern}` ({} file(s) scanned, {files_skipped} skipped):",
        lines.len(),
        files_scanned
    );
    if files_scanned == 0 && files_skipped > 0 {
        out.push_str("; note: files were skipped (too large or unreadable)");
    }
    out.push('\n');
    out.push_str(&lines.join("\n"));
    Outcome::ok(out)
}
