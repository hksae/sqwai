//! Tree-sitter language registry: one grammar table for every consumer.
//!
//! `TsLang` maps file extensions and language names to the eleven loaded
//! grammars, with the comment-node kinds the pattern matcher needs. It backs
//! the `ast_grep` matching engine and the `outline` tool; the project graph
//! index and its per-language walkers that used to live here were retired
//! with the graph store.

use tree_sitter::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsLang {
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

impl TsLang {
    /// Extension table shared by every consumer (pattern engine, outline).
    pub fn from_extension(ext: &str) -> Option<Self> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "rs" => TsLang::Rust,
            "py" => TsLang::Python,
            "js" | "mjs" | "cjs" | "jsx" => TsLang::JavaScript,
            "ts" | "mts" | "cts" => TsLang::TypeScript,
            "tsx" => TsLang::Tsx,
            "go" => TsLang::Go,
            "sh" | "bash" => TsLang::Bash,
            "c" | "h" => TsLang::C,
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => TsLang::Cpp,
            "cs" => TsLang::CSharp,
            "java" => TsLang::Java,
            _ => return None,
        })
    }

    /// Language-name table shared by every consumer (pattern engine,
    /// outline parser tags).
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "rust" => TsLang::Rust,
            "python" => TsLang::Python,
            "javascript" => TsLang::JavaScript,
            "typescript" => TsLang::TypeScript,
            "tsx" => TsLang::Tsx,
            "go" => TsLang::Go,
            "bash" => TsLang::Bash,
            "c" => TsLang::C,
            "cpp" | "c++" => TsLang::Cpp,
            "csharp" | "c#" | "cs" => TsLang::CSharp,
            "java" => TsLang::Java,
            _ => return None,
        })
    }

    pub(crate) fn grammar(&self) -> Language {
        match self {
            TsLang::Rust => tree_sitter_rust::LANGUAGE.into(),
            TsLang::Python => tree_sitter_python::LANGUAGE.into(),
            TsLang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            TsLang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            TsLang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            TsLang::Go => tree_sitter_go::LANGUAGE.into(),
            TsLang::Bash => tree_sitter_bash::LANGUAGE.into(),
            TsLang::C => tree_sitter_c::LANGUAGE.into(),
            TsLang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            TsLang::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            TsLang::Java => tree_sitter_java::LANGUAGE.into(),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            TsLang::Rust => "rust",
            TsLang::Python => "python",
            TsLang::JavaScript => "javascript",
            TsLang::TypeScript => "typescript",
            TsLang::Tsx => "tsx",
            TsLang::Go => "go",
            TsLang::Bash => "bash",
            TsLang::C => "c",
            TsLang::Cpp => "cpp",
            TsLang::CSharp => "csharp",
            TsLang::Java => "java",
        }
    }

    /// Comment node kinds for pattern matching (comments never match code).
    pub fn is_comment(&self, kind: &str) -> bool {
        match self {
            TsLang::Rust => matches!(kind, "line_comment" | "block_comment"),
            _ => kind == "comment" || kind == "line_comment" || kind == "block_comment",
        }
    }
}
