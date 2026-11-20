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
    /// Who made the call: `mcp:<client name>`, `cli`, `ui`, `test`, a custom tag from
    /// `TRACKERTOP_TELEMETRY_SOURCE`, or `legacy` (recorded before v0.7, source unknown).
    #[serde(default)]
    pub source: String,
}

/// Aggregated token-savings statistics.
///
/// Savings are a range: `total_tokens_saved` is the upper bound (the agent would have read
/// every touched file whole, and repo_map up to 15 files), `conservative_tokens_saved` the
/// lower estimate (targeted reads of at most 1,500 tokens from at most 5 touched files,
/// never less than what TrackerTop sent).
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
    /// Which calls the totals cover, e.g. `AI agents (MCP)` or `all sources`.
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub total_conservative_baseline: usize,
    #[serde(default)]
    pub conservative_tokens_saved: usize,
    #[serde(default)]
    pub conservative_savings_percent: f64,
    /// Every source, regardless of `scope`, so test/CLI calls are visible but separate.
    #[serde(default)]
    pub by_source: Vec<SourceBreakdown>,
}

impl TokenStatsSummary {
    /// Plain-text summary: savings as a range, calls per source, and the latest requests
    /// (tool + query) so a human can see what the agent asked for.
    pub fn summary_text(&self, recent: usize) -> String {
        let mut out = format!(
            "# TRACKERTOP TOKEN SAVINGS ({})\nQueries: {}\nTokens sent: {}\n\
             Saved: {} to {} tok ({:.0}% to {:.0}%): conservative estimate to upper bound\n\
             Est. USD saved (upper bound): ${:.4} (Sonnet) / ${:.4} (Opus)\n\
             Conservative: targeted reads of at most 1,500 tok from at most 5 touched files. \
             Upper bound: every touched file read whole (repo_map: up to 15 files).\n",
            self.scope,
            self.total_queries,
            self.total_tokens_sent,
            self.conservative_tokens_saved,
            self.total_tokens_saved,
            self.conservative_savings_percent,
            self.savings_percent,
            self.est_usd_saved_sonnet,
            self.est_usd_saved_opus
        );
        if !self.by_source.is_empty() {
            out.push_str("\n## Calls by source (all rows)\n");
            for s in &self.by_source {
                out.push_str(&format!(
                    "- {}: {} calls, sent {} tok, saved {} to {} tok\n",
                    s.source,
                    s.queries,
                    s.tokens_sent,
                    s.tokens_saved_conservative,
                    s.tokens_saved_upper
                ));
            }
            out.push_str(
                "(`mcp:*` = AI agents; `cli`, `ui`, `test`, `benchmark` and `legacy` \
                 (before v0.7, source unknown) are not counted in the totals above)\n",
            );
        }
        if !self.recent.is_empty() && recent > 0 {
            out.push_str("\n## What the agent asked for (latest first)\n");
            for e in self.recent.iter().take(recent) {
                out.push_str(&format!(
                    "- {} `{}` `{}` ({} tok sent)\n",
                    e.timestamp.get(..19).unwrap_or(&e.timestamp),
                    e.tool,
                    e.query.chars().take(80).collect::<String>(),
                    e.tokens_sent
                ));
            }
        }
        out
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceBreakdown {
    pub source: String,
    pub queries: usize,
    pub tokens_sent: usize,
    pub tokens_saved_upper: usize,
    pub tokens_saved_conservative: usize,
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
    /// `note`, `decision` or `dismissed` (a review lead that was checked and rejected).
    pub kind: String,
    /// Why: required for decisions and dismissed leads, empty for plain notes.
    pub reason: String,
}

impl ProjectNote {
    /// One line as surfaced next to code: `Decision #3 [target]: text (why: reason)`.
    pub fn display_line(&self) -> String {
        let label = match self.kind.as_str() {
            "decision" => "Decision",
            "dismissed" => "Dismissed lead",
            _ => "Note",
        };
        if self.reason.is_empty() {
            format!("{label} #{} [{}]: {}", self.id, self.target, self.content)
        } else {
            format!(
                "{label} #{} [{}]: {} (why: {})",
                self.id, self.target, self.content, self.reason
            )
        }
    }
}
