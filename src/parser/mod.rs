pub mod docs;
pub mod html;
pub mod powershell;
pub mod rust_lang;
pub mod wgsl;

use crate::model::{Language, RawReference, Symbol, SymbolKind};
use crate::tokens::count_tokens;
use anyhow::Result;

/// Output of parsing a single source file.
#[derive(Debug, Default)]
pub struct ParsedFile {
    pub symbols: Vec<Symbol>,
    pub refs: Vec<RawReference>,
}

/// Extensible trait for deterministic language-specific AST/symbol extractors.
pub trait LanguageExtractor: Send + Sync {
    fn language(&self) -> Language;
    fn parse(&self, file_path: &str, source: &str) -> Result<ParsedFile>;
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
        let mut s: String = collapsed.chars().take(max_chars.saturating_sub(3)).collect();
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

/// Dispatches parsing to the appropriate language extractor.
pub fn parse_file(lang: Language, file_path: &str, source: &str) -> Result<ParsedFile> {
    match lang {
        Language::Rust => rust_lang::RustExtractor.parse(file_path, source),
        Language::Html => html::HtmlExtractor.parse(file_path, source),
        Language::Wgsl => wgsl::WgslExtractor.parse(file_path, source),
        Language::PowerShell => powershell::PowerShellExtractor.parse(file_path, source),
        Language::Markdown => docs::MarkdownExtractor.parse(file_path, source),
        Language::Toml => docs::TomlExtractor.parse(file_path, source),
    }
}
