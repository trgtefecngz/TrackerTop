use crate::model::{EdgeKind, RawReference, Symbol, SymbolKind};
use crate::parser::{
    LanguageExtractor, ParsedFile, compact_signature, file_symbol_id, make_file_symbol,
    make_symbol_id,
};
use crate::tokens::count_tokens;
use anyhow::{Context, Result};
use std::collections::HashSet;
use tree_sitter::{Node, Parser};

pub struct HtmlExtractor;

impl LanguageExtractor for HtmlExtractor {
    fn id(&self) -> &'static str {
        "html"
    }

    fn display_name(&self) -> &'static str {
        "HTML"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["html", "htm"]
    }

    fn is_syntax_tree(&self) -> bool {
        true
    }

    fn color_hex(&self) -> &'static str {
        "#38bdf8"
    }

    fn parse(&self, file_path: &str, source: &str) -> Result<ParsedFile> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_html::LANGUAGE.into())
            .context("failed to load tree-sitter-html grammar")?;
        let tree = parser
            .parse(source, None)
            .context("tree-sitter-html returned None")?;

        let mut out = ParsedFile::default();
        let file_id = file_symbol_id(file_path);
        out.symbols.push(make_file_symbol(file_path, source));

        let bytes = source.as_bytes();
        walk_html(tree.root_node(), file_path, bytes, &file_id, &mut out);
        Ok(out)
    }
}

fn text<'a>(n: Node<'a>, bytes: &'a [u8]) -> &'a str {
    n.utf8_text(bytes).unwrap_or("")
}

fn walk_html(node: Node, file_path: &str, bytes: &[u8], file_id: &str, out: &mut ParsedFile) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let start_line = child.start_position().row + 1;
        let end_line = child.end_position().row + 1;

        match child.kind() {
            "script_element" => {
                let attrs = extract_start_tag_attrs(child, bytes);
                if let Some(src) = attrs.get("src") {
                    out.refs.push(RawReference {
                        source_symbol_id: file_id.to_string(),
                        target_name: normalize_asset_ref(src),
                        kind: EdgeKind::Imports,
                        line: start_line,
                    });
                } else {
                    // Inline <script>: extract JS function declarations and fetch/wgsl refs
                    let raw = text(child, bytes);
                    let sym_id = make_symbol_id(file_path, "inline_script", start_line);
                    out.symbols.push(Symbol {
                        id: sym_id.clone(),
                        file_path: file_path.to_string(),
                        name: format!("script@{start_line}"),
                        qualified_name: format!("script@{start_line}"),
                        kind: SymbolKind::Script,
                        signature: format!("<script> (lines {start_line}-{end_line})"),
                        start_line,
                        end_line,
                        token_count: count_tokens(raw),
                        visibility: "inline".to_string(),
                        doc_summary: String::new(),
                        parent_id: Some(file_id.to_string()),
                    });
                    extract_inline_js_symbols(raw, start_line, file_path, &sym_id, out);
                }
            }
            "style_element" => {
                let raw = text(child, bytes);
                let sym_id = make_symbol_id(file_path, "inline_style", start_line);
                out.symbols.push(Symbol {
                    id: sym_id,
                    file_path: file_path.to_string(),
                    name: format!("style@{start_line}"),
                    qualified_name: format!("style@{start_line}"),
                    kind: SymbolKind::Style,
                    signature: format!("<style> (lines {start_line}-{end_line})"),
                    start_line,
                    end_line,
                    token_count: count_tokens(raw),
                    visibility: "inline".to_string(),
                    doc_summary: String::new(),
                    parent_id: Some(file_id.to_string()),
                });
            }
            "element" => {
                let tag = extract_tag_name(child, bytes);
                let attrs = extract_start_tag_attrs(child, bytes);

                // External links & assets
                for attr_key in ["href", "src", "action"] {
                    if let Some(val) = attrs.get(attr_key)
                        && is_local_ref(val)
                    {
                        out.refs.push(RawReference {
                            source_symbol_id: file_id.to_string(),
                            target_name: normalize_asset_ref(val),
                            kind: EdgeKind::Imports,
                            line: start_line,
                        });
                    }
                }

                // Meaningful structural elements: elements with `id`, `<template>`, `<canvas>`, custom elements (`<my-foo>`)
                let is_custom = tag.contains('-');
                let id_attr = attrs.get("id");
                if id_attr.is_some() || tag == "template" || tag == "canvas" || is_custom {
                    let name = if let Some(idv) = id_attr {
                        format!("#{idv}")
                    } else {
                        format!("<{tag}>")
                    };
                    let sym_id = make_symbol_id(file_path, &name, start_line);
                    let class_hint = attrs
                        .get("class")
                        .map(|c| format!(" class=\"{c}\""))
                        .unwrap_or_default();
                    let id_hint = id_attr.map(|i| format!(" id=\"{i}\"")).unwrap_or_default();
                    let sig = compact_signature(&format!("<{tag}{id_hint}{class_hint}>"), 120);
                    let raw = text(child, bytes);
                    out.symbols.push(Symbol {
                        id: sym_id,
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: format!("{tag}{name}"),
                        kind: SymbolKind::Element,
                        signature: sig,
                        start_line,
                        end_line,
                        token_count: count_tokens(raw),
                        visibility: tag.clone(),
                        doc_summary: String::new(),
                        parent_id: Some(file_id.to_string()),
                    });
                }

                // Inline event handlers (onclick="foo()")
                for (k, v) in &attrs {
                    if k.starts_with("on")
                        && let Some(fn_name) = v.split('(').next()
                    {
                        let clean = fn_name.trim();
                        if !clean.is_empty()
                            && clean.chars().all(|c| c.is_alphanumeric() || c == '_')
                        {
                            out.refs.push(RawReference {
                                source_symbol_id: file_id.to_string(),
                                target_name: clean.to_string(),
                                kind: EdgeKind::Calls,
                                line: start_line,
                            });
                        }
                    }
                }

                walk_html(child, file_path, bytes, file_id, out);
            }
            _ => {
                walk_html(child, file_path, bytes, file_id, out);
            }
        }
    }
}

fn extract_tag_name(elem: Node, bytes: &[u8]) -> String {
    let mut cursor = elem.walk();
    for ch in elem.children(&mut cursor) {
        if ch.kind() == "start_tag" || ch.kind() == "self_closing_tag" {
            let mut c2 = ch.walk();
            for sub in ch.children(&mut c2) {
                if sub.kind() == "tag_name" {
                    return text(sub, bytes).to_ascii_lowercase();
                }
            }
        }
    }
    String::new()
}

fn extract_start_tag_attrs(elem: Node, bytes: &[u8]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let mut cursor = elem.walk();
    for ch in elem.children(&mut cursor) {
        if ch.kind() == "start_tag" || ch.kind() == "self_closing_tag" {
            let mut c2 = ch.walk();
            for attr in ch.children(&mut c2) {
                if attr.kind() == "attribute" {
                    let mut name = String::new();
                    let mut val = String::new();
                    let mut c3 = attr.walk();
                    for part in attr.children(&mut c3) {
                        match part.kind() {
                            "attribute_name" => name = text(part, bytes).to_ascii_lowercase(),
                            "quoted_attribute_value" | "attribute_value" => {
                                val = text(part, bytes)
                                    .trim_matches('"')
                                    .trim_matches('\'')
                                    .to_string();
                            }
                            _ => {}
                        }
                    }
                    if !name.is_empty() {
                        map.insert(name, val);
                    }
                }
            }
        }
    }
    map
}

fn is_local_ref(val: &str) -> bool {
    let v = val.trim();
    !v.is_empty()
        && !v.starts_with("http://")
        && !v.starts_with("https://")
        && !v.starts_with("//")
        && !v.starts_with('#')
        && !v.starts_with("mailto:")
        && !v.starts_with("data:")
        && !v.starts_with("javascript:")
}

fn normalize_asset_ref(val: &str) -> String {
    val.split('?')
        .next()
        .unwrap_or(val)
        .split('#')
        .next()
        .unwrap_or(val)
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

/// Lightweight inline JS extractor so `<script>` blocks inside HTML apps still expose functions & shader/asset fetches.
fn extract_inline_js_symbols(
    raw: &str,
    base_line: usize,
    file_path: &str,
    script_id: &str,
    out: &mut ParsedFile,
) {
    let mut seen_refs = HashSet::new();
    for (idx, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        let line_no = base_line + idx;

        // `function foo(...)` or `async function foo(...)`
        let fn_rest = trimmed
            .strip_prefix("async function ")
            .or_else(|| trimmed.strip_prefix("function "));
        if let Some(rest) = fn_rest
            && let Some(paren) = rest.find('(')
        {
            let name = rest[..paren].trim();
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                let sym_id = make_symbol_id(file_path, name, line_no);
                out.symbols.push(Symbol {
                    id: sym_id,
                    file_path: file_path.to_string(),
                    name: name.to_string(),
                    qualified_name: name.to_string(),
                    kind: SymbolKind::Function,
                    signature: compact_signature(trimmed.trim_end_matches('{'), 120),
                    start_line: line_no,
                    end_line: line_no,
                    token_count: count_tokens(trimmed),
                    visibility: "js".to_string(),
                    doc_summary: String::new(),
                    parent_id: Some(script_id.to_string()),
                });
            }
        }

        // Detect references to `.wgsl` / `.js` / `.html` / `.css` files in fetch/import strings
        for quote in ['"', '\''] {
            let parts: Vec<&str> = trimmed.split(quote).collect();
            for chunk in parts.iter().skip(1).step_by(2) {
                if (chunk.ends_with(".wgsl")
                    || chunk.ends_with(".js")
                    || chunk.ends_with(".css")
                    || chunk.ends_with(".html"))
                    && is_local_ref(chunk)
                {
                    let norm = normalize_asset_ref(chunk);
                    if seen_refs.insert(norm.clone()) {
                        out.refs.push(RawReference {
                            source_symbol_id: script_id.to_string(),
                            target_name: norm,
                            kind: EdgeKind::Imports,
                            line: line_no,
                        });
                    }
                }
            }
        }
    }
}
