pub mod docs;
pub mod golang;
pub mod html;
pub mod powershell;
pub mod python;
pub mod rust_lang;
pub mod typescript;
pub mod wgsl;

use crate::model::{LanguageInfo, RawReference, Symbol, SymbolKind};
use crate::tokens::count_tokens;
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

/// Output of parsing a single source file.
#[derive(Debug, Default)]
pub struct ParsedFile {
    pub symbols: Vec<Symbol>,
    pub refs: Vec<RawReference>,
}

/// Open, extensible trait for deterministic language extractors.
///
/// Implement this trait and register it in `LanguageRegistry` to add support for any language
/// without modifying the database, graph engine, MCP server, HTML report, or desktop UI.
pub trait LanguageExtractor: Send + Sync {
    /// Stable lowercase identifier (e.g. `"rust"`, `"wgsl"`, `"python"`).
    fn id(&self) -> &'static str;
    /// Human-readable label shown in reports & UI (e.g. `"Rust"`, `"WGSL"`).
    fn display_name(&self) -> &'static str;
    /// Lowercase file extensions without leading dot (e.g. `&["rs"]`, `&["ps1", "psm1"]`).
    fn extensions(&self) -> &'static [&'static str];
    /// Hex color for graphs and badges (e.g. `"#fb923c"`).
    fn color_hex(&self) -> &'static str;
    /// Deterministically extract symbols and references from a single file.
    fn parse(&self, file_path: &str, source: &str) -> Result<ParsedFile>;
    /// Whether symbol spans come from a real parser (tree-sitter) rather than line
    /// heuristics. Tools tell the agent when spans may be approximate.
    fn is_syntax_tree(&self) -> bool {
        false
    }
}

impl LanguageRegistry {
    /// Short extractor-quality label for tool output, e.g. `tree-sitter` or
    /// `heuristic extractor: spans may be approximate`.
    pub fn extractor_quality(&self, id: &str) -> &'static str {
        match self.extractor_by_id(id) {
            Some(e) if e.is_syntax_tree() => "tree-sitter",
            Some(_) => "heuristic extractor: spans may be approximate, verify before editing",
            None => "no extractor",
        }
    }
}

/// Runtime registry of pluggable `LanguageExtractor` implementations.
pub struct LanguageRegistry {
    extractors: Vec<Arc<dyn LanguageExtractor>>,
    by_id: HashMap<String, usize>,
    by_ext: HashMap<String, usize>,
}

impl LanguageRegistry {
    pub fn new() -> Self {
        Self {
            extractors: Vec::new(),
            by_id: HashMap::new(),
            by_ext: HashMap::new(),
        }
    }

    /// Registers a new language extractor (can also override an existing extension).
    pub fn register(&mut self, extractor: Arc<dyn LanguageExtractor>) {
        let idx = self.extractors.len();
        self.by_id.insert(extractor.id().to_ascii_lowercase(), idx);
        for ext in extractor.extensions() {
            self.by_ext.insert(ext.to_ascii_lowercase(), idx);
        }
        self.extractors.push(extractor);
    }

    /// Default registry populated with TrackerTop's built-in extractors.
    /// Contributors adding a new language simply add one `reg.register(...)` line here!
    pub fn default_builtin() -> Self {
        let mut reg = Self::new();
        reg.register(Arc::new(rust_lang::RustExtractor));
        reg.register(Arc::new(typescript::TypeScriptExtractor));
        reg.register(Arc::new(python::PythonExtractor));
        reg.register(Arc::new(golang::GoExtractor));
        reg.register(Arc::new(html::HtmlExtractor));
        reg.register(Arc::new(wgsl::WgslExtractor));
        reg.register(Arc::new(powershell::PowerShellExtractor));
        reg.register(Arc::new(docs::MarkdownExtractor));
        reg.register(Arc::new(docs::TomlExtractor));
        reg
    }

    pub fn extractor_for_path(&self, path: &Path) -> Option<Arc<dyn LanguageExtractor>> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        let &idx = self.by_ext.get(&ext)?;
        Some(Arc::clone(&self.extractors[idx]))
    }

    pub fn extractor_by_id(&self, id: &str) -> Option<Arc<dyn LanguageExtractor>> {
        let &idx = self.by_id.get(&id.to_ascii_lowercase())?;
        Some(Arc::clone(&self.extractors[idx]))
    }

    pub fn display_name_for(&self, id: &str) -> String {
        self.extractor_by_id(id)
            .map(|e| e.display_name().to_string())
            .unwrap_or_else(|| id.to_string())
    }

    pub fn color_for(&self, id: &str) -> &'static str {
        self.extractor_by_id(id)
            .map(|e| e.color_hex())
            .unwrap_or("#94a3b8")
    }

    pub fn languages(&self) -> Vec<LanguageInfo> {
        self.extractors
            .iter()
            .map(|e| LanguageInfo {
                id: e.id().to_string(),
                display_name: e.display_name().to_string(),
                extensions: e.extensions().iter().map(|s| s.to_string()).collect(),
                color_hex: e.color_hex().to_string(),
            })
            .collect()
    }
}

impl Default for LanguageRegistry {
    fn default() -> Self {
        Self::default_builtin()
    }
}

static DEFAULT_REGISTRY: OnceLock<LanguageRegistry> = OnceLock::new();

pub fn default_registry() -> &'static LanguageRegistry {
    DEFAULT_REGISTRY.get_or_init(LanguageRegistry::default_builtin)
}

/// Creates the root File symbol for a parsed file so file-level edges work uniformly.
pub fn make_file_symbol(file_path: &str, source: &str) -> Symbol {
    let line_count = source.lines().count().max(1);
    Symbol {
        id: file_symbol_id(file_path),
        file_path: file_path.to_string(),
        name: file_path
            .rsplit('/')
            .next()
            .unwrap_or(file_path)
            .to_string(),
        qualified_name: file_path.to_string(),
        kind: SymbolKind::File,
        signature: format!("file {file_path} ({line_count}L)"),
        start_line: 1,
        end_line: line_count,
        token_count: count_tokens(source),
        visibility: "file".to_string(),
        doc_summary: String::new(),
        parent_id: None,
    }
}

pub fn file_symbol_id(file_path: &str) -> String {
    format!("{file_path}::__file__")
}

pub fn make_symbol_id(file_path: &str, qualified_name: &str, start_line: usize) -> String {
    format!("{file_path}::{qualified_name}#{start_line}")
}

/// Compactly cleans a multi-line signature into a single concise line for token efficiency.
pub fn compact_signature(raw: &str, max_chars: usize) -> String {
    let joined: String = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let collapsed = collapse_spaces(&joined);
    if collapsed.chars().count() <= max_chars {
        collapsed
    } else {
        let mut s: String = collapsed
            .chars()
            .take(max_chars.saturating_sub(3))
            .collect();
        s.push_str("...");
        s
    }
}

pub fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// Error recovery for line-oriented languages (Python, Go). When a file has a syntax error
/// (an unclosed bracket while it is being edited), tree-sitter can fold everything after it
/// into one `ERROR` node, and every later definition would vanish from the index. The
/// source is then cut at top-level definition lines (`starts_definition`, at column 0) and
/// each piece is parsed on its own. A piece is returned padded with blank lines, so its line
/// numbers stay those of the whole file. Lines for which
/// `leads` holds (decorators, doc comments) stay attached to the definition below them.
pub fn recovery_segments(
    source: &str,
    starts_definition: fn(&str) -> bool,
    leads: fn(&str) -> bool,
) -> Vec<String> {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let mut cuts: Vec<usize> = vec![0];
    for (i, line) in lines.iter().enumerate() {
        if i == 0 || !starts_definition(line) {
            continue;
        }
        // Move the cut above the decorators / doc comments of this definition
        let mut at = i;
        while at > 0 && leads(lines[at - 1]) {
            at -= 1;
        }
        if at > *cuts.last().unwrap_or(&0) {
            cuts.push(at);
        }
    }
    cuts.push(lines.len());
    cuts.windows(2)
        .map(|w| {
            // One newline per skipped line keeps row numbers those of the whole file
            "\n".repeat(w[0]) + &lines[w[0]..w[1]].concat()
        })
        .collect()
}
