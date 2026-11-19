use crate::model::{EdgeKind, RawReference, Symbol, SymbolKind};
use crate::parser::{
    LanguageExtractor, ParsedFile, compact_signature, file_symbol_id, make_file_symbol,
    make_symbol_id,
};
use crate::tokens::count_tokens;
use anyhow::Result;
use std::collections::HashSet;

pub struct MarkdownExtractor;

impl LanguageExtractor for MarkdownExtractor {
    fn id(&self) -> &'static str {
        "markdown"
    }

    fn display_name(&self) -> &'static str {
        "Markdown"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["md", "markdown"]
    }

    fn color_hex(&self) -> &'static str {
        "#34d399"
    }

    fn parse(&self, file_path: &str, source: &str) -> Result<ParsedFile> {
        let mut out = ParsedFile::default();
        let file_id = file_symbol_id(file_path);
        out.symbols.push(make_file_symbol(file_path, source));

        let lines: Vec<&str> = source.lines().collect();
        let mut in_code_fence = false;
        let mut seen_refs = HashSet::new();

        for (idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("```") {
                in_code_fence = !in_code_fence;
                continue;
            }
            if in_code_fence {
                continue;
            }
            let line_no = idx + 1;

            if trimmed.starts_with('#') {
                let hashes = trimmed.chars().take_while(|c| *c == '#').count();
                if (1..=3).contains(&hashes) {
                    let title = trimmed[hashes..].trim();
                    if !title.is_empty() {
                        let sym_id = make_symbol_id(file_path, title, line_no);
                        out.symbols.push(Symbol {
                            id: sym_id,
                            file_path: file_path.to_string(),
                            name: title.to_string(),
                            qualified_name: title.to_string(),
                            kind: SymbolKind::Section,
                            signature: compact_signature(trimmed, 120),
                            start_line: line_no,
                            end_line: line_no,
                            token_count: count_tokens(trimmed),
                            visibility: format!("h{hashes}"),
                            doc_summary: String::new(),
                            parent_id: Some(file_id.clone()),
                        });
                    }
                }
            }

            // Link backtick-quoted symbol/file references like `src/graph.rs` or `Database`
            let parts: Vec<&str> = trimmed.split('`').collect();
            for code_span in parts.iter().skip(1).step_by(2) {
                let clean = code_span.trim();
                if clean.len() >= 3
                    && clean.len() <= 64
                    && !clean.contains(' ')
                    && seen_refs.insert(clean.to_string())
                {
                    out.refs.push(RawReference {
                        source_symbol_id: file_id.clone(),
                        target_name: clean.to_string(),
                        kind: EdgeKind::References,
                        line: line_no,
                    });
                }
            }
        }
        Ok(out)
    }
}

pub struct TomlExtractor;

impl LanguageExtractor for TomlExtractor {
    fn id(&self) -> &'static str {
        "toml"
    }

    fn display_name(&self) -> &'static str {
        "TOML"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["toml"]
    }

    fn color_hex(&self) -> &'static str {
        "#94a3b8"
    }

    fn parse(&self, file_path: &str, source: &str) -> Result<ParsedFile> {
        let mut out = ParsedFile::default();
        let file_id = file_symbol_id(file_path);
        out.symbols.push(make_file_symbol(file_path, source));

        let mut in_deps = false;
        for (idx, line) in source.lines().enumerate() {
            let trimmed = line.trim();
            let line_no = idx + 1;
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                in_deps = trimmed.contains("dependencies");
                let sec = trimmed.trim_matches(|c| c == '[' || c == ']');
                let sym_id = make_symbol_id(file_path, sec, line_no);
                out.symbols.push(Symbol {
                    id: sym_id,
                    file_path: file_path.to_string(),
                    name: sec.to_string(),
                    qualified_name: sec.to_string(),
                    kind: SymbolKind::Section,
                    signature: trimmed.to_string(),
                    start_line: line_no,
                    end_line: line_no,
                    token_count: count_tokens(trimmed),
                    visibility: "toml".to_string(),
                    doc_summary: String::new(),
                    parent_id: Some(file_id.clone()),
                });
            } else if in_deps && !trimmed.is_empty() && !trimmed.starts_with('#') {
                if let Some((dep_name, _)) = trimmed.split_once('=') {
                    let dep = dep_name.trim();
                    if !dep.is_empty() {
                        let sym_id = make_symbol_id(file_path, dep, line_no);
                        out.symbols.push(Symbol {
                            id: sym_id,
                            file_path: file_path.to_string(),
                            name: dep.to_string(),
                            qualified_name: format!("dep::{dep}"),
                            kind: SymbolKind::Dependency,
                            signature: compact_signature(trimmed, 100),
                            start_line: line_no,
                            end_line: line_no,
                            token_count: count_tokens(trimmed),
                            visibility: "dep".to_string(),
                            doc_summary: String::new(),
                            parent_id: Some(file_id.clone()),
                        });
                    }
                }
            }
        }
        Ok(out)
    }
}
