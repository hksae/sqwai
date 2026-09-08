//! `outline`: structural file outline with signatures and line numbers.
//!
//! Provides a compact summary of code structure (functions, methods, classes,
//! structs, enums, interfaces, modules) with line numbers to save tokens.
//! Uses tree-sitter AST for 11 languages (Rust, Python, JS, TS, TSX, Go, Bash,
//! C, C++, C#, Java) and an indentation/keyword fallback for all other files.

use super::astgrep::Lang;
use super::{Outcome, ToolCtx};
use serde_json::Value;
use tree_sitter::Node;

const MAX_FILE_BYTES: u64 = 512_000;
const MAX_SIG_LEN: usize = 120;

#[derive(Debug, Clone)]
pub(crate) struct OutlineItem {
    pub(crate) line: usize,
    pub(crate) depth: usize,
    pub(crate) signature: String,
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
        return Outcome::err("file too large for outline (max 512KB)");
    }

    let bytes = match std::fs::read(&full_path) {
        Ok(b) => b,
        Err(e) => return Outcome::err(format!("cannot read {path_arg}: {e}")),
    };

    let src = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => return Outcome::err("file is binary or not valid UTF-8"),
    };

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
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&lang.ts()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(src, None) else {
        return Vec::new();
    };

    let mut items = Vec::new();
    walk_node(tree.root_node(), src, lang, 1, max_depth, &mut items);
    items
}

fn walk_node(
    node: Node,
    src: &str,
    lang: Lang,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<OutlineItem>,
) {
    if depth > max_depth {
        return;
    }

    let kind = node.kind();

    // Transparent containers / file roots: descend without increasing depth
    if is_transparent_container(kind, lang) {
        let mut cursor = node.walk();
        for c in node.children(&mut cursor) {
            walk_node(c, src, lang, depth, max_depth, out);
        }
        return;
    }

    // Check if this node is a declaration
    if is_declaration_node(node, lang) {
        let body_opt = find_body_node(node, lang);
        let (line, sig) = extract_signature(node, src, body_opt, lang);

        if !sig.is_empty() {
            out.push(OutlineItem {
                line,
                depth,
                signature: sig,
            });

            // If it has a body and depth < max_depth, recurse into inner declarations
            if let Some(body) = body_opt
                && depth < max_depth
            {
                // C# and C++ namespaces: children stay at current depth (top-level classes)
                let next_depth = if is_namespace(kind, lang) {
                    depth
                } else {
                    depth + 1
                };
                let mut cursor = body.walk();
                for c in body.children(&mut cursor) {
                    walk_node(c, src, lang, next_depth, max_depth, out);
                }
            }
        }
        return;
    }

    // Default: visit children at same depth (e.g. wrapper expressions, blocks at root)
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        walk_node(c, src, lang, depth, max_depth, out);
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
        && kind == "export_statement")
        || (lang == Lang::Python && kind == "decorated_definition")
        || (matches!(lang, Lang::C | Lang::Cpp) && kind == "template_declaration")
        || (lang == Lang::Go && kind == "type_declaration")
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
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => matches!(
            kind,
            "function_declaration"
                | "class_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "method_definition"
                | "enum_assignment"
        ),
        Lang::Go => matches!(
            kind,
            "function_declaration" | "method_declaration" | "type_spec"
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

fn find_body_node<'a>(node: Node<'a>, _lang: Lang) -> Option<Node<'a>> {
    if let Some(b) = node.child_by_field_name("body") {
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

    let mut has_parent_prefix = false;
    // If wrapped in export_statement, template_declaration, or decorated_definition, include parent prefix
    if let Some(p) = node.parent() {
        let pk = p.kind();
        if pk == "export_statement" || pk == "template_declaration" || pk == "decorated_definition"
        {
            start = p.start_byte();
            has_parent_prefix = true;
        } else if pk == "type_declaration" && p.start_byte() < node.start_byte() {
            let prefix = &src[p.start_byte()..node.start_byte()];
            if prefix.contains("type") && !prefix.contains('(') {
                start = p.start_byte();
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
            }
            break;
        }
    }

    let line = if start < src.len() {
        src[..start].chars().filter(|&c| c == '\n').count() + 1
    } else {
        node.start_position().row + 1
    };

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
    let mut indent_stack: Vec<usize> = Vec::new();

    for (idx, line) in src.lines().enumerate() {
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
                        depth: hashes,
                        signature: clean_signature(trimmed),
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

            while let Some(&top) = indent_stack.last() {
                if indent_cols < top {
                    indent_stack.pop();
                } else {
                    break;
                }
            }
            if indent_stack.is_empty() || *indent_stack.last().unwrap() < indent_cols {
                indent_stack.push(indent_cols);
            }

            let depth = indent_stack.len();
            if depth <= max_depth {
                let sig = clean_signature(&raw_sig);
                if !sig.is_empty() {
                    items.push(OutlineItem {
                        line: line_no,
                        depth,
                        signature: sig,
                    });
                }
            }
        }
    }

    items
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
