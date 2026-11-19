use crate::model::{EdgeKind, RawReference, Symbol, SymbolKind};
use crate::parser::{
    LanguageExtractor, ParsedFile, compact_signature, file_symbol_id, make_file_symbol,
    make_symbol_id,
};
use crate::tokens::count_tokens;
use anyhow::Result;
use std::collections::HashSet;

pub struct GoExtractor;

impl LanguageExtractor for GoExtractor {
    fn id(&self) -> &'static str {
        "go"
    }

    fn display_name(&self) -> &'static str {
        "Go"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["go"]
    }

    fn color_hex(&self) -> &'static str {
        "#06b6d4"
    }

    fn parse(&self, file_path: &str, source: &str) -> Result<ParsedFile> {
        let mut out = ParsedFile::default();
        let file_id = file_symbol_id(file_path);
        out.symbols.push(make_file_symbol(file_path, source));

        let lines: Vec<&str> = source.lines().collect();
        let mut in_import_block = false;
        let mut pending_doc = String::new();
        let mut i = 0usize;

        while i < lines.len() {
            let trimmed = lines[i].trim();
            let line_no = i + 1;

            if in_import_block {
                if trimmed.starts_with(')') {
                    in_import_block = false;
                } else if let Some(pkg) = extract_go_quoted(trimmed) {
                    if let Some(last) = pkg.rsplit('/').next() {
                        out.refs.push(RawReference {
                            source_symbol_id: file_id.clone(),
                            target_name: last.to_string(),
                            kind: EdgeKind::Imports,
                            line: line_no,
                        });
                    }
                }
                i += 1;
                continue;
            }

            if trimmed.starts_with("//") {
                let doc = trimmed.trim_start_matches('/').trim();
                if !doc.is_empty() && pending_doc.is_empty() {
                    pending_doc = compact_signature(doc, 110);
                }
                i += 1;
                continue;
            }
            if trimmed.is_empty() {
                pending_doc.clear();
                i += 1;
                continue;
            }

            if trimmed.starts_with("import (") {
                in_import_block = true;
                i += 1;
                continue;
            }
            if trimmed.starts_with("import ") {
                if let Some(pkg) = extract_go_quoted(trimmed) {
                    if let Some(last) = pkg.rsplit('/').next() {
                        out.refs.push(RawReference {
                            source_symbol_id: file_id.clone(),
                            target_name: last.to_string(),
                            kind: EdgeKind::Imports,
                            line: line_no,
                        });
                    }
                }
                i += 1;
                continue;
            }

            // `type Name struct { ... }` / `type Name interface { ... }` / `type Name ...`
            if let Some(rest) = trimmed.strip_prefix("type ") {
                let name = read_go_ident(rest);
                if !name.is_empty() {
                    let after_name = rest[name.len()..].trim_start();
                    let kind = if after_name.starts_with("struct") {
                        SymbolKind::Struct
                    } else if after_name.starts_with("interface") {
                        SymbolKind::Trait
                    } else {
                        SymbolKind::TypeAlias
                    };
                    let end_idx = if trimmed.contains('{') {
                        find_go_brace_end(&lines, i)
                    } else {
                        i
                    };
                    let block = lines[i..=end_idx].join("\n");
                    let vis = if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                        "pub"
                    } else {
                        "private"
                    };
                    let sym_id = make_symbol_id(file_path, &name, line_no);
                    out.symbols.push(Symbol {
                        id: sym_id,
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: name,
                        kind,
                        signature: compact_signature(trimmed.split('{').next().unwrap_or(trimmed), 140),
                        start_line: line_no,
                        end_line: end_idx + 1,
                        token_count: count_tokens(&block),
                        visibility: vis.to_string(),
                        doc_summary: std::mem::take(&mut pending_doc),
                        parent_id: Some(file_id.clone()),
                    });
                    i = end_idx + 1;
                    continue;
                }
            }

            // `func (r *Recv) Name(...)` or `func Name(...)`
            if let Some(rest) = trimmed.strip_prefix("func ") {
                let (recv, after_recv) = if rest.starts_with('(') {
                    if let Some(close) = rest.find(')') {
                        let inside = &rest[1..close];
                        let recv_type = inside
                            .split_whitespace()
                            .last()
                            .unwrap_or("")
                            .trim_start_matches('*')
                            .to_string();
                        (Some(recv_type), rest[close + 1..].trim_start())
                    } else {
                        (None, rest)
                    }
                } else {
                    (None, rest)
                };

                let name = read_go_ident(after_recv);
                if !name.is_empty() {
                    let end_idx = find_go_brace_end(&lines, i);
                    let block = lines[i..=end_idx].join("\n");
                    let is_test = name.starts_with("Test") && file_path.ends_with("_test.go");
                    let (kind, qname) = if is_test {
                        (SymbolKind::Test, name.clone())
                    } else if let Some(ref r) = recv {
                        (SymbolKind::Method, format!("{r}::{name}"))
                    } else {
                        (SymbolKind::Function, name.clone())
                    };
                    let vis = if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                        "pub"
                    } else {
                        "private"
                    };
                    let sym_id = make_symbol_id(file_path, &qname, line_no);

                    out.symbols.push(Symbol {
                        id: sym_id.clone(),
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: qname,
                        kind,
                        signature: compact_signature(trimmed.split('{').next().unwrap_or(trimmed), 140),
                        start_line: line_no,
                        end_line: end_idx + 1,
                        token_count: count_tokens(&block),
                        visibility: vis.to_string(),
                        doc_summary: std::mem::take(&mut pending_doc),
                        parent_id: Some(file_id.clone()),
                    });

                    if let Some(r) = recv {
                        if !r.is_empty() {
                            out.refs.push(RawReference {
                                source_symbol_id: sym_id.clone(),
                                target_name: r,
                                kind: EdgeKind::UsesType,
                                line: line_no,
                            });
                        }
                    }

                    let edge_kind = if is_test { EdgeKind::Tests } else { EdgeKind::Calls };
                    collect_go_calls(&block, &name, &sym_id, line_no, edge_kind, &mut out);
                    i = end_idx + 1;
                    continue;
                }
            }

            pending_doc.clear();
            i += 1;
        }

        Ok(out)
    }
}

fn read_go_ident(s: &str) -> String {
    s.trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

fn extract_go_quoted(s: &str) -> Option<String> {
    let start = s.find('"')?;
    let rest = &s[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn find_go_brace_end(lines: &[&str], start: usize) -> usize {
    let mut depth = 0i32;
    let mut seen = false;
    for (idx, line) in lines.iter().enumerate().skip(start) {
        for ch in line.chars() {
            if ch == '{' {
                depth += 1;
                seen = true;
            } else if ch == '}' {
                depth -= 1;
                if seen && depth <= 0 {
                    return idx;
                }
            }
        }
    }
    start
}

fn collect_go_calls(
    block: &str,
    self_name: &str,
    source_id: &str,
    base_line: usize,
    edge_kind: EdgeKind,
    out: &mut ParsedFile,
) {
    let mut seen = HashSet::new();
    let chars: Vec<char> = block.chars().collect();
    let mut i = 0usize;
    let mut line_off = 0usize;
    while i < chars.len() {
        if chars[i] == '\n' {
            line_off += 1;
            i += 1;
            continue;
        }
        if chars[i].is_ascii_alphabetic() || chars[i] == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let ident: String = chars[start..i].iter().collect();
            if i < chars.len()
                && chars[i] == '('
                && ident.len() >= 2
                && ident != self_name
                && !is_go_builtin(&ident)
                && seen.insert(ident.clone())
            {
                out.refs.push(RawReference {
                    source_symbol_id: source_id.to_string(),
                    target_name: ident,
                    kind: edge_kind,
                    line: base_line + line_off,
                });
            }
        } else {
            i += 1;
        }
    }
}

fn is_go_builtin(name: &str) -> bool {
    matches!(
        name,
        "if" | "for"
            | "switch"
            | "select"
            | "func"
            | "return"
            | "make"
            | "new"
            | "len"
            | "cap"
            | "append"
            | "copy"
            | "delete"
            | "close"
            | "panic"
            | "recover"
            | "Println"
            | "Printf"
            | "Sprintf"
            | "Errorf"
            | "Error"
            | "Fatal"
            | "Fatalf"
    )
}
