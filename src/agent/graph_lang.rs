//! Tree-sitter language registry: the grammar table behind the `outline`
//! tool. The project graph index, its per-language walkers and the `ast_grep`
//! pattern engine that also lived on this enum were all retired — the graph
//! with the store, the pattern engine when its tool left the registry.

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
    /// File extension to grammar, the only mapping `outline` needs.
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
}
