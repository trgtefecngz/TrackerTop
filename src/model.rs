use serde::{Deserialize, Serialize};

/// Metadata describing any language registered in `LanguageRegistry`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageInfo {
    pub id: String,
    pub display_name: String,
    pub extensions: Vec<String>,
    pub color_hex: String,
}

/// Structural symbol categories extracted deterministically from source code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    File,
    Module,
    Struct,
    Enum,
    Trait,
    Impl,
    Function,
    Method,
    Test,
    Const,
    TypeAlias,
    Macro,
    // Shader / GPU
    ShaderEntry,
    Binding,
    // Web / Markup
    Element,
    Script,
    Style,
    // Scripting / OOP
    Class,
    Filter,
    // Docs / Config
    Section,
    Dependency,
}

impl SymbolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Module => "mod",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Function => "fn",
            Self::Method => "method",
            Self::Test => "test",
            Self::Const => "const",
            Self::TypeAlias => "type",
            Self::Macro => "macro",
            Self::ShaderEntry => "shader",
            Self::Binding => "binding",
            Self::Element => "elem",
            Self::Script => "script",
            Self::Style => "style",
            Self::Class => "class",
            Self::Filter => "filter",
            Self::Section => "section",
            Self::Dependency => "dep",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "file" => Self::File,
            "mod" => Self::Module,
            "struct" => Self::Struct,
            "enum" => Self::Enum,
            "trait" => Self::Trait,
            "impl" => Self::Impl,
            "fn" => Self::Function,
            "method" => Self::Method,
            "test" => Self::Test,
            "const" => Self::Const,
            "type" => Self::TypeAlias,
            "macro" => Self::Macro,
            "shader" => Self::ShaderEntry,
            "binding" => Self::Binding,
            "elem" => Self::Element,
            "script" => Self::Script,
            "style" => Self::Style,
            "class" => Self::Class,
            "filter" => Self::Filter,
            "section" => Self::Section,
            "dep" => Self::Dependency,
            _ => Self::Function,
        }
    }
}

/// Directed relationship kinds in the codebase knowledge graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeKind {
    Contains,
    Imports,
    Calls,
    Implements,
    UsesType,
    Tests,
    References,
    CoChanges,
}

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Contains => "CONTAINS",
            Self::Imports => "IMPORTS",
            Self::Calls => "CALLS",
            Self::Implements => "IMPLEMENTS",
            Self::UsesType => "USES_TYPE",
            Self::Tests => "TESTS",
            Self::References => "REFERENCES",
            Self::CoChanges => "CO_CHANGES",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "CONTAINS" => Self::Contains,
            "IMPORTS" => Self::Imports,
            "CALLS" => Self::Calls,
            "IMPLEMENTS" => Self::Implements,
            "USES_TYPE" => Self::UsesType,
            "TESTS" => Self::Tests,
            "CO_CHANGES" => Self::CoChanges,
            _ => Self::References,
        }
    }
}

/// Indexed file metadata. `language` holds the open `LanguageExtractor::id()` string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub path: String, // forward-slash normalized path relative to repo root
    pub language: String,
    pub hash: String,
    pub mtime_secs: i64,
    pub size_bytes: u64,
    pub line_count: usize,
    pub token_count: usize,
}

/// Extracted code/doc symbol stored in the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub id: String,        // unique stable id: "<file>::<qualified_name>#<start_line>"
    pub file_path: String, // normalized relative path
    pub name: String,
    pub qualified_name: String,
    pub kind: SymbolKind,
    pub signature: String,
    pub start_line: usize, // 1-based inclusive
    pub end_line: usize,   // 1-based inclusive
    pub token_count: usize,
    pub visibility: String,
    pub doc_summary: String,
    pub parent_id: Option<String>,
}

/// Unresolved reference produced during single-file parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawReference {
    pub source_symbol_id: String,
    pub target_name: String,
    pub kind: EdgeKind,
    pub line: usize,
}

/// Resolved directed edge stored in the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub source_id: String,
    pub target_id: String,
    pub kind: EdgeKind,
    pub weight: f32,
}

/// Telemetry record for every AI/CLI query measuring real token savings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryEntry {
    pub id: i64,
    pub timestamp: String,
    pub tool: String,
    pub query: String,
    pub tokens_sent: usize,
    pub baseline_tokens: usize,
    pub tokens_saved: usize,
    pub files_avoided: usize,
    pub latency_ms: u64,
}

/// Aggregated token-savings statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenStatsSummary {
    pub total_queries: usize,
    pub total_tokens_sent: usize,
    pub total_baseline_tokens: usize,
    pub total_tokens_saved: usize,
    pub savings_percent: f64,
    pub est_usd_saved_sonnet: f64,
    pub est_usd_saved_opus: f64,
    pub by_tool: Vec<ToolSavingsBreakdown>,
    pub recent: Vec<TelemetryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSavingsBreakdown {
    pub tool: String,
    pub queries: usize,
    pub tokens_sent: usize,
    pub baseline_tokens: usize,
    pub tokens_saved: usize,
    pub savings_percent: f64,
}

/// Persistent AI/developer architectural memory note stored per repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectNote {
    pub id: i64,
    pub target: String, // "project", file path, or symbol name
    pub content: String,
    pub created_at: String,
}
