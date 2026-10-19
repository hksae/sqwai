//! `outline`: structural file outline with signatures and line numbers.
//!
//! Provides a compact summary of code structure (functions, methods, classes,
//! structs, enums, interfaces, modules) with line numbers to save tokens.
//! Uses tree-sitter AST for 11 languages (Rust, Python, JS, TS, TSX, Go, Bash,
//! C, C++, C#, Java) and an indentation/keyword fallback for all other files.

use super::super::graph_lang::TsLang as Lang;
use super::{Outcome, ToolCtx};
use serde_json::Value;
use tree_sitter::Node;

const MAX_FILE_BYTES: u64 = 2_097_152;
const MAX_SIG_LEN: usize = 120;

#[derive(Debug, Clone)]
pub(crate) struct OutlineItem {
    pub(crate) line: usize,
    pub(crate) end_line: usize,
    pub(crate) depth: usize,
    pub(crate) signature: String,
    pub(crate) parent: Option<String>,
}

pub fn outline(ctx: &mut ToolCtx, args: &Value) -> Outcome {
    let Some(path_arg) = args["path"].as_str() else {
        return Outcome::err("outline requires a 'path'");
    };
    if path_arg.trim().is_empty() {
        return Outcome::err("outline path cannot be empty");
    }

    let max_depth = args["max_depth"]
        .as_u64()
        .map(|d| (d as usize).clamp(1, 10))
        .unwrap_or(2);

    let full_path = match ctx.resolve(path_arg) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };

    if !full_path.exists() {
        return Outcome::err(format!("file not found: {path_arg}"));
    }
    if full_path.is_dir() {
        return Outcome::err(format!(
            "outline expects a file, found a directory: {path_arg}"
        ));
    }

    let meta = match std::fs::metadata(&full_path) {
        Ok(m) => m,
        Err(e) => return Outcome::err(format!("cannot stat {path_arg}: {e}")),
    };
    if meta.len() > MAX_FILE_BYTES {
        return Outcome::err("file too large for outline (max 2MB)");
    }

    let bytes = match std::fs::read(&full_path) {
        Ok(b) => b,
        Err(e) => return Outcome::err(format!("cannot read {path_arg}: {e}")),
    };
    if bytes.iter().take(8000).any(|&b| b == 0) {
        return Outcome::err("file is binary or not valid UTF-8");
    }

    let decoded = String::from_utf8_lossy(&bytes);
    let src = decoded.strip_prefix('\u{feff}').unwrap_or(&decoded);

    let ext = full_path.extension().and_then(|e| e.to_str()).unwrap_or("");

    let items = match Lang::from_extension(ext) {
        Some(lang) => {
            let ts_items = extract_ts(src, lang, max_depth);
            if ts_items.is_empty() && !src.trim().is_empty() {
                extract_fallback(src, max_depth)
            } else {
                ts_items
            }
        }
        None => extract_fallback(src, max_depth),
    };

    if items.is_empty() {
        return Outcome::ok(format!("{path_arg} (0 items)"));
    }

    let mut out = format!("{path_arg} ({} items):\n", items.len());
    let skip_deps = args["skip_deps"].as_bool().unwrap_or(false);
    if !skip_deps {
        let imports = extract_file_imports(src, ext);
        if !imports.is_empty() {
            out.push_str(&format!("Imports: {}\n", imports.join(", ")));
        }
        let importers = find_importers(&ctx.root, &full_path);
        if !importers.is_empty() {
            let shown: Vec<&str> = importers.iter().take(12).map(String::as_str).collect();
            let extra = if importers.len() > 12 {
                format!(" (+{} more)", importers.len() - 12)
            } else {
                String::new()
            };
            out.push_str(&format!(
                "Imported by ({}): {}{extra}\n",
                importers.len(),
                shown.join(", ")
            ));
        }
    }
    for item in items {
        let indent = "  ".repeat(item.depth.saturating_sub(1));
        out.push_str(&format!("{:>4}: {indent}{}\n", item.line, item.signature));
    }

    Outcome::ok(out.trim_end().to_string())
}

// ---------------------------------------------------------------------------
// Tree-sitter extraction
// ---------------------------------------------------------------------------

fn extract_ts(src: &str, lang: Lang, max_depth: usize) -> Vec<OutlineItem> {
    thread_local! {
        static TS_PARSER: std::cell::RefCell<tree_sitter::Parser> =
            std::cell::RefCell::new(tree_sitter::Parser::new());
    }
    let tree = TS_PARSER.with(|cell| {
        let mut parser = cell.borrow_mut();
        if parser.set_language(&lang.grammar()).is_err() {
            return None;
        }
        parser.parse(src, None)
    });
    let Some(tree) = tree else {
        return Vec::new();
    };

    let mut items = Vec::new();
    walk_node(tree.root_node(), src, lang, 1, max_depth, 0, None, &mut items);
    items
}

/// Hard bound on raw AST recursion. `depth` only counts declaration
/// nesting; wrapper-expression chains (`(((...)))`, deep binary trees,
/// ERROR nodes) recurse at the same `depth`, so the Rust call stack
/// tracked the raw tree — a 512KB file of nested parens overflowed it
/// (audit H14).
const RAW_RECURSION_LIMIT: usize = 256;

#[allow(clippy::too_many_arguments)]
fn walk_node(
    node: Node,
    src: &str,
    lang: Lang,
    depth: usize,
    max_depth: usize,
    raw_depth: usize,
    parent_sig: Option<&str>,
    out: &mut Vec<OutlineItem>,
) {
    if depth > max_depth || raw_depth > RAW_RECURSION_LIMIT {
        return;
    }

    let kind = node.kind();

    // Transparent containers / file roots: descend without increasing depth
    if is_transparent_container(kind, lang) {
        let mut cursor = node.walk();
        for c in node.children(&mut cursor) {
            walk_node(c, src, lang, depth, max_depth, raw_depth + 1, parent_sig, out);
        }
        return;
    }

    // Check if this node is a declaration
    if is_declaration_node(node, lang) {
        let body_opt = find_body_node(node, lang);
        let (line, sig) = extract_signature(node, src, body_opt, lang);

        if !sig.is_empty() {
            let end_line = (node.end_position().row + 1).max(line);
            out.push(OutlineItem {
                line,
                end_line,
                depth,
                signature: sig.clone(),
                parent: parent_sig.map(str::to_string),
            });

            // If it has a body and depth < max_depth, recurse into inner declarations
            if let Some(body) = body_opt
                && depth < max_depth
            {
                // C# and C++ namespaces: children stay at current depth (top-level classes)
                let (next_depth, next_parent) = if is_namespace(kind, lang) {
                    (depth, parent_sig)
                } else {
                    (depth + 1, Some(sig.as_str()))
                };
                let mut cursor = body.walk();
                for c in body.children(&mut cursor) {
                    walk_node(
                        c,
                        src,
                        lang,
                        next_depth,
                        max_depth,
                        raw_depth + 1,
                        next_parent,
                        out,
                    );
                }
            }
        }
        return;
    }

    // Default: visit children at same depth (e.g. wrapper expressions, blocks at root)
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        walk_node(c, src, lang, depth, max_depth, raw_depth + 1, parent_sig, out);
    }
}

fn is_namespace(kind: &str, lang: Lang) -> bool {
    match lang {
        Lang::CSharp => matches!(
            kind,
            "namespace_declaration" | "file_scoped_namespace_declaration"
        ),
        Lang::Cpp | Lang::C => kind == "namespace_definition",
        _ => false,
    }
}

fn is_transparent_container(kind: &str, lang: Lang) -> bool {
    matches!(
        kind,
        "source_file" | "compilation_unit" | "translation_unit" | "program" | "script" | "module"
    ) || (matches!(lang, Lang::JavaScript | Lang::TypeScript | Lang::Tsx)
        && matches!(
            kind,
            "export_statement" | "lexical_declaration" | "variable_declaration"
        ))
        || (lang == Lang::Python && kind == "decorated_definition")
        || (matches!(lang, Lang::C | Lang::Cpp) && kind == "template_declaration")
        || (lang == Lang::Go
            && matches!(
                kind,
                "type_declaration" | "var_declaration" | "const_declaration"
            ))
}

fn is_declaration_node(node: Node, lang: Lang) -> bool {
    let kind = node.kind();
    match lang {
        Lang::Rust => matches!(
            kind,
            "function_item"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "impl_item"
                | "mod_item"
                | "type_item"
                | "const_item"
                | "static_item"
                | "macro_definition"
                | "enum_variant"
        ),
        Lang::Python => matches!(kind, "function_definition" | "class_definition"),
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
            if matches!(
                kind,
                "function_declaration"
                    | "class_declaration"
                    | "abstract_class_declaration"
                    | "interface_declaration"
                    | "type_alias_declaration"
                    | "enum_declaration"
                    | "method_definition"
                    | "method_signature"
                    | "abstract_method_signature"
                    | "property_signature"
                    | "enum_assignment"
            ) {
                return true;
            }
            if matches!(
                kind,
                "variable_declarator" | "public_field_definition" | "field_definition"
            ) {
                return find_js_fn_value(node).is_some();
            }
            false
        }
        Lang::Go => matches!(
            kind,
            "function_declaration"
                | "method_declaration"
                | "type_spec"
                // The grouped `var (...)` / `const (...)` forms have no name of
                // their own; each spec inside them does, and that is the line the
                // reader wants — the way `enum_variant` is for a Rust enum.
                | "var_spec"
                | "const_spec"
        ),
        Lang::Bash => matches!(kind, "function_definition"),
        Lang::C | Lang::Cpp => {
            if matches!(
                kind,
                "function_definition"
                    | "class_specifier"
                    | "struct_specifier"
                    | "enum_specifier"
                    | "namespace_definition"
                    | "enumerator"
            ) {
                return true;
            }
            if kind == "declaration" {
                // Check if it declares a function prototype, struct, class, or enum
                let mut cursor = node.walk();
                for c in node.children(&mut cursor) {
                    let ck = c.kind();
                    if ck == "function_declarator"
                        || ck == "struct_specifier"
                        || ck == "class_specifier"
                        || ck == "enum_specifier"
                    {
                        return true;
                    }
                }
            }
            false
        }
        Lang::CSharp => matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "struct_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "method_declaration"
                | "constructor_declaration"
                | "property_declaration"
                | "namespace_declaration"
                | "file_scoped_namespace_declaration"
                | "local_function_statement"
                | "enum_member_declaration"
        ),
        Lang::Java => matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "method_declaration"
                | "constructor_declaration"
                | "enum_constant"
        ),
    }
}

/// For a JS/TS `variable_declarator` or class field (`const Foo = (x) => { ... }`
/// or `const Bar = React.memo((props) => { ... })`), return the underlying
/// function node (`arrow_function` / `function_expression` / `function`).
fn find_js_fn_value<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let val = node.child_by_field_name("value").or_else(|| {
        let mut cursor = node.walk();
        node.children(&mut cursor).find(|c| {
            matches!(
                c.kind(),
                "arrow_function"
                    | "function_expression"
                    | "function"
                    | "generator_function"
                    | "call_expression"
            )
        })
    })?;
    match val.kind() {
        "arrow_function" | "function_expression" | "function" | "generator_function" => Some(val),
        "call_expression" => {
            let args = val.child_by_field_name("arguments")?;
            let mut cursor = args.walk();
            args.children(&mut cursor).find(|c| {
                matches!(
                    c.kind(),
                    "arrow_function" | "function_expression" | "function" | "generator_function"
                )
            })
        }
        _ => None,
    }
}

fn find_body_node<'a>(node: Node<'a>, _lang: Lang) -> Option<Node<'a>> {
    if let Some(b) = node.child_by_field_name("body") {
        return Some(b);
    }
    if matches!(
        node.kind(),
        "variable_declarator" | "public_field_definition" | "field_definition"
    ) && let Some(fn_node) = find_js_fn_value(node)
        && let Some(b) = fn_node.child_by_field_name("body")
    {
        return Some(b);
    }
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        let k = c.kind();
        if matches!(
            k,
            "block"
                | "declaration_list"
                | "class_body"
                | "interface_body"
                | "enum_body"
                | "enum_variant_list"
                | "enum_member_declaration_list"
                | "field_declaration_list"
                | "compound_statement"
                | "statement_block"
        ) {
            return Some(c);
        }
        if k == "struct_type" || k == "interface_type" {
            let mut inner_cursor = c.walk();
            for inner in c.children(&mut inner_cursor) {
                let ik = inner.kind();
                if ik == "field_declaration_list" || ik == "method_spec_list" {
                    return Some(inner);
                }
            }
        }
    }
    None
}

fn extract_signature(node: Node, src: &str, body_opt: Option<Node>, lang: Lang) -> (usize, String) {
    let mut start = node.start_byte();
    let mut start_row = node.start_position().row;

    let mut has_parent_prefix = false;
    // If wrapped in export_statement, lexical_declaration, template_declaration, or decorated_definition, include parent prefix
    if let Some(p) = node.parent() {
        let pk = p.kind();
        if pk == "export_statement" || pk == "template_declaration" || pk == "decorated_definition"
        {
            start = p.start_byte();
            start_row = p.start_position().row;
            has_parent_prefix = true;
        } else if matches!(pk, "lexical_declaration" | "variable_declaration") {
            if let Some(gp) = p.parent()
                && gp.kind() == "export_statement"
            {
                start = gp.start_byte();
                start_row = gp.start_position().row;
            } else {
                start = p.start_byte();
                start_row = p.start_position().row;
            }
            has_parent_prefix = true;
        } else if pk == "type_declaration" && p.start_byte() < node.start_byte() {
            let prefix = &src[p.start_byte()..node.start_byte()];
            if prefix.contains("type") && !prefix.contains('(') {
                start = p.start_byte();
                start_row = p.start_position().row;
                has_parent_prefix = true;
            }
        }
    }

    if !has_parent_prefix {
        // Skip leading comments or attributes inside the node itself
        let mut cursor = node.walk();
        for c in node.children(&mut cursor) {
            let ck = c.kind();
            if ck == "attribute_item"
                || ck == "attribute_list"
                || ck == "comment"
                || ck == "line_comment"
                || ck == "block_comment"
            {
                continue;
            }
            if start < c.start_byte() {
                start = c.start_byte();
                start_row = c.start_position().row;
            }
            break;
        }
    }

    let line = start_row + 1;

    let raw_end = if let Some(b) = body_opt {
        b.start_byte()
    } else {
        node.end_byte()
    };

    let slice = if start < raw_end && raw_end <= src.len() {
        &src[start..raw_end]
    } else {
        ""
    };

    let mut sig = clean_signature(slice);
    if lang == Lang::Python && !sig.ends_with(':') && !sig.is_empty() {
        sig.push(':');
    }

    (line, sig)
}

// ---------------------------------------------------------------------------
// Universal Indent / Keyword Fallback
// ---------------------------------------------------------------------------

fn extract_fallback(src: &str, max_depth: usize) -> Vec<OutlineItem> {
    let mut items = Vec::new();
    let mut indent_stack: Vec<(usize, String)> = Vec::new();
    let lines: Vec<&str> = src.lines().collect();

    for (idx, &line) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Markdown headings
        if trimmed.starts_with('#') {
            let hashes = trimmed.chars().take_while(|&c| c == '#').count();
            if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
                if hashes <= max_depth {
                    items.push(OutlineItem {
                        line: line_no,
                        end_line: line_no,
                        depth: hashes,
                        signature: clean_signature(trimmed),
                        parent: None,
                    });
                }
                continue;
            }
        }

        // Skip comments
        if trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
            || trimmed.starts_with("--")
            || trimmed.starts_with('#')
            || trimmed.starts_with(';')
        {
            continue;
        }

        if let Some(raw_sig) = match_fallback_declaration(trimmed) {
            let indent_cols = count_indent(line);

            while let Some((top_indent, _)) = indent_stack.last() {
                if indent_cols <= *top_indent {
                    indent_stack.pop();
                } else {
                    break;
                }
            }
            let parent = indent_stack.last().map(|(_, s)| s.clone());
            let sig = clean_signature(&raw_sig);
            if !sig.is_empty() {
                indent_stack.push((indent_cols, sig.clone()));
                let depth = indent_stack.len();
                if depth <= max_depth {
                    items.push(OutlineItem {
                        line: line_no,
                        end_line: line_no,
                        depth,
                        signature: sig,
                        parent,
                    });
                }
            }
        }
    }

    let total_lines = lines.len().max(1);
    for i in 0..items.len() {
        let cur_depth = items[i].depth;
        let start_line = items[i].line;
        let bound_line = items[i + 1..]
            .iter()
            .find(|next| next.depth <= cur_depth)
            .map(|next| next.line.saturating_sub(1))
            .unwrap_or(total_lines)
            .max(start_line);
        items[i].end_line = refine_fallback_end_line(&lines, start_line, bound_line);
    }

    items
}

/// Refine a fallback declaration's `end_line` within `start_line..=bound_line`
/// using brace balance `{ ... }` when braces are present, or trimming trailing
/// blank/comment lines before the next sibling declaration.
fn refine_fallback_end_line(lines: &[&str], start_line: usize, bound_line: usize) -> usize {
    let mut brace_depth = 0isize;
    let mut saw_open_brace = false;
    for line_no in start_line..=bound_line.min(lines.len()) {
        let line = lines[line_no - 1];
        for ch in line.chars() {
            if ch == '{' {
                brace_depth += 1;
                saw_open_brace = true;
            } else if ch == '}' {
                brace_depth -= 1;
                if saw_open_brace && brace_depth <= 0 {
                    return line_no;
                }
            }
        }
    }
    let mut end = bound_line.min(lines.len());
    while end > start_line {
        let t = lines[end - 1].trim();
        if t.is_empty() || t.starts_with("//") || t.starts_with('#') || t.starts_with("--") {
            end -= 1;
        } else {
            break;
        }
    }
    end
}

fn count_indent(line: &str) -> usize {
    let mut cols = 0;
    for ch in line.chars() {
        match ch {
            ' ' => cols += 1,
            '\t' => cols += 4,
            _ => break,
        }
    }
    cols
}

fn match_fallback_declaration(line: &str) -> Option<String> {
    let stripped = strip_modifiers(line);
    let decl_keywords = [
        "fn ",
        "fun ",
        "func ",
        "function ",
        "def ",
        "defp ",
        "defmodule ",
        "class ",
        "struct ",
        "enum ",
        "interface ",
        "trait ",
        "type ",
        "record ",
        "protocol ",
        "extension ",
        "actor ",
        "module ",
        "namespace ",
        "package ",
        "impl ",
        "mod ",
        "proc ",
        "sub ",
    ];

    for kw in decl_keywords {
        if stripped.starts_with(kw) {
            return Some(cut_before_body(line));
        }
    }

    // Zig style: `const X = struct {` or `const X = enum {`
    if stripped.starts_with("const ") && (stripped.contains("struct") || stripped.contains("enum"))
    {
        return Some(cut_before_body(line));
    }

    None
}

fn strip_modifiers(mut s: &str) -> &str {
    let modifiers = [
        "pub ",
        "pub(crate) ",
        "pub(super) ",
        "public ",
        "private ",
        "protected ",
        "internal ",
        "open ",
        "final ",
        "override ",
        "abstract ",
        "static ",
        "async ",
        "const ",
        "export ",
        "default ",
        "mut ",
        "lazy ",
        "inline ",
        "extern ",
        "virtual ",
        "explicit ",
        "friend ",
        "constexpr ",
    ];
    loop {
        let mut stripped = false;
        for m in modifiers {
            if let Some(rest) = s.strip_prefix(m) {
                s = rest.trim_start();
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    s
}

fn cut_before_body(line: &str) -> String {
    let mut end = line.len();
    if let Some(pos) = line.find('{') {
        end = end.min(pos);
    }
    if let Some(pos) = line.find(" do") {
        end = end.min(pos);
    }
    line[..end].trim_end().to_string()
}

// ---------------------------------------------------------------------------
// Signature formatting & cleaning
// ---------------------------------------------------------------------------

fn clean_signature(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let mut res = String::with_capacity(trimmed.len());
    let mut last_was_space = false;

    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !last_was_space && !res.is_empty() && !res.ends_with('(') {
                res.push(' ');
                last_was_space = true;
            }
        } else {
            if last_was_space && (ch == ',' || ch == ';' || ch == ')') {
                res.pop();
            }
            res.push(ch);
            last_was_space = false;
        }
    }

    // Trim trailing syntax noise
    while res.ends_with('{')
        || res.ends_with('}')
        || res.ends_with(';')
        || res.ends_with("=>")
        || res.ends_with("->")
    {
        if res.ends_with("=>") || res.ends_with("->") {
            res.truncate(res.len() - 2);
        } else {
            res.pop();
        }
        let t = res.trim_end().to_string();
        res = t;
    }

    let res = res.trim();
    if res.chars().count() > MAX_SIG_LEN {
        let s: String = res.chars().take(MAX_SIG_LEN - 3).collect();
        format!("{s}...")
    } else {
        res.to_string()
    }
}

/// Locate exact 1-based `(signature, start_line, end_line)` spans for the
/// requested symbol names in `src` using tree-sitter AST declarations (with
/// fallback to keyword/indentation scan). Expands `start_line` upward to
/// include immediately preceding doc comments or attributes.
pub(crate) fn find_symbol_ranges(
    src: &str,
    ext: &str,
    symbols: &[String],
) -> Vec<(String, usize, usize)> {
    let items = match Lang::from_extension(ext) {
        Some(lang) => {
            let ts_items = extract_ts(src, lang, 3);
            if ts_items.is_empty() && !src.trim().is_empty() {
                extract_fallback(src, 3)
            } else {
                ts_items
            }
        }
        None => extract_fallback(src, 3),
    };
    if items.is_empty() {
        return Vec::new();
    }

    let lines: Vec<&str> = src.lines().collect();
    let total_lines = lines.len().max(1);
    let mut results: Vec<(String, usize, usize)> = Vec::new();

    for raw_sym in symbols {
        let cleaned = raw_sym
            .trim()
            .trim_start_matches("fn ")
            .trim_start_matches("pub fn ")
            .trim_start_matches("struct ")
            .trim_start_matches("enum ")
            .trim_start_matches("trait ")
            .trim_start_matches("class ")
            .trim_start_matches("def ")
            .trim_end_matches("()");
        let (qualifier, leaf) = if let Some((q, l)) = cleaned.rsplit_once("::") {
            (Some(q.trim()), l.trim())
        } else if let Some((q, l)) = cleaned.rsplit_once('.') {
            (Some(q.trim()), l.trim())
        } else {
            (None, cleaned.trim())
        };
        if leaf.is_empty() {
            continue;
        }
        let qual_leaf = qualifier.map(|q| {
            q.rsplit("::")
                .next()
                .unwrap_or(q)
                .rsplit('.')
                .next()
                .unwrap_or(q)
                .trim()
        });

        // If a qualifier (`Type::method` or `Class.method`) was given, prefer items
        // whose enclosing parent declaration matches that qualifier.
        let has_qualified_match = qual_leaf.is_some_and(|ql| {
            !ql.is_empty()
                && items.iter().any(|item| {
                    contains_ident(&item.signature, leaf)
                        && item
                            .parent
                            .as_deref()
                            .is_some_and(|p| contains_ident(p, ql))
                })
        });

        for item in &items {
            let parent_ok = if has_qualified_match {
                let ql = qual_leaf.unwrap_or_default();
                item.parent
                    .as_deref()
                    .is_some_and(|p| contains_ident(p, ql))
            } else {
                true
            };
            if parent_ok && contains_ident(&item.signature, leaf) {
                let mut start = item.line.clamp(1, total_lines);
                let end = item.end_line.clamp(start, total_lines);
                // Expand upward up to 6 lines for doc comments (`///`, `#`) and attributes (`#[...]`, `@`).
                let min_start = start.saturating_sub(6).max(1);
                while start > min_start {
                    let prev = lines[start - 2].trim();
                    if prev.starts_with("///")
                        || prev.starts_with("#[")
                        || prev.starts_with('@')
                        || prev.starts_with("//!")
                        || prev.starts_with("*/")
                        || prev.starts_with('*')
                        || prev.starts_with("/**")
                    {
                        start -= 1;
                    } else {
                        break;
                    }
                }
                if !results.iter().any(|(_, s, e)| *s == start && *e == end) {
                    results.push((item.signature.clone(), start, end));
                }
            }
        }
    }
    results
}

fn contains_ident(haystack: &str, ident: &str) -> bool {
    if ident.is_empty() {
        return false;
    }
    let is_id_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let h_bytes = haystack.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = haystack[from..].find(ident) {
        let start = from + rel;
        let end = start + ident.len();
        let prev_ok = start == 0 || !is_id_char(h_bytes[start - 1]);
        let next_ok = end >= h_bytes.len() || !is_id_char(h_bytes[end]);
        if prev_ok && next_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Extract compact outgoing imports / module declarations from `src`.
fn extract_file_imports(src: &str, ext: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let lower_ext = ext.to_ascii_lowercase();
    for line in src.lines().take(250) {
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") || t.starts_with('#') && !t.starts_with("#include") {
            continue;
        }
        let target = match lower_ext.as_str() {
            "rs" => t
                .strip_prefix("pub use ")
                .or_else(|| t.strip_prefix("pub(crate) use "))
                .or_else(|| t.strip_prefix("pub(super) use "))
                .or_else(|| t.strip_prefix("use "))
                .or_else(|| t.strip_prefix("pub mod "))
                .or_else(|| t.strip_prefix("pub(crate) mod "))
                .or_else(|| t.strip_prefix("mod "))
                .and_then(|rest| {
                    let clean = rest.trim_end_matches(';').trim();
                    (!clean.ends_with('{') && !clean.is_empty()).then_some(clean)
                }),
            "py" | "pyi" | "pyw" => {
                if t.starts_with("import ") || t.starts_with("from ") {
                    Some(t)
                } else {
                    None
                }
            }
            "js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx" => {
                if (t.starts_with("import ") || t.starts_with("export "))
                    && let Some(pos) = t.find(" from ")
                {
                    Some(t[pos + 6..].trim_end_matches(';').trim())
                } else {
                    None
                }
            }
            "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "ipp" | "tpp" | "ino" => {
                t.strip_prefix("#include").map(str::trim)
            }
            "go" | "java" | "cs" | "csx" => t
                .strip_prefix("import ")
                .or_else(|| t.strip_prefix("using "))
                .map(|r| r.trim_end_matches(';').trim()),
            _ => None,
        };
        if let Some(imp) = target {
            let entry = imp.to_string();
            if !entry.is_empty() && !out.contains(&entry) {
                out.push(entry);
                if out.len() >= 15 {
                    break;
                }
            }
        }
    }
    out
}

/// Fast on-demand scan for project files that import/reference `target_path` by
/// its module/file stem (`use ...::stem`, `mod stem`, `import ... stem`, `#include "...stem..."`).
fn find_importers(root: & std::path::Path, target_path: &std::path::Path) -> Vec<String> {
    let raw_stem = target_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let module_name = if matches!(raw_stem, "mod" | "index" | "__init__" | "lib") {
        target_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("")
    } else {
        raw_stem
    };
    if module_name.len() < 2 || matches!(module_name, "main" | "lib" | "src" | "tests" | "test") {
        return Vec::new();
    }

    let mut importers = Vec::new();
    let mut scanned_files = 0usize;
    let host_owned = root.join(".sqwai");

    for entry in ignore::WalkBuilder::new(root)
        .hidden(false)
        .max_filesize(Some(512 * 1024))
        .build()
        .flatten()
    {
        let p = entry.path();
        if !p.is_file() || p == target_path || p.starts_with(&host_owned) {
            continue;
        }
        let rel = p.strip_prefix(root).unwrap_or(p);
        if rel.components().any(|c| {
            matches!(
                c.as_os_str().to_str(),
                Some(".git" | ".sqwai" | "target" | "node_modules" | "dist" | "build")
            )
        }) {
            continue;
        }
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if Lang::from_extension(ext).is_none() {
            continue;
        }
        scanned_files += 1;
        if scanned_files > 500 {
            break;
        }
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        if bytes.iter().take(1024).any(|&b| b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        if !text.contains(module_name) {
            continue;
        }
        for (idx, line) in text.lines().enumerate() {
            let t = line.trim();
            let is_import_or_use = t.starts_with("use ")
                || t.starts_with("pub use ")
                || t.starts_with("pub(crate) use ")
                || t.starts_with("pub(super) use ")
                || t.starts_with("mod ")
                || t.starts_with("pub mod ")
                || t.starts_with("pub(crate) mod ")
                || t.starts_with("import ")
                || t.starts_with("from ")
                || t.starts_with("export ")
                || t.starts_with("#include")
                || t.contains(&format!("{module_name}::"));
            if is_import_or_use && contains_ident(t, module_name) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                importers.push(format!("{rel_str}:{}", idx + 1));
                break;
            }
        }
        if importers.len() >= 20 {
            break;
        }
    }
    importers.sort();
    importers
}
