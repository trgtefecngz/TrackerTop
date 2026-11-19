use crate::model::{EdgeKind, RawReference, Symbol, SymbolKind};
use crate::parser::{
    LanguageExtractor, ParsedFile, compact_signature, file_symbol_id, make_file_symbol,
    make_symbol_id,
};
use crate::tokens::count_tokens;
use anyhow::{Context, Result};
use std::collections::HashSet;
use tree_sitter::{Node, Parser};

pub struct RustExtractor;

impl LanguageExtractor for RustExtractor {
    fn id(&self) -> &'static str {
        "rust"
    }

    fn display_name(&self) -> &'static str {
        "Rust"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["rs"]
    }

    fn color_hex(&self) -> &'static str {
        "#fb923c"
    }

    fn parse(&self, file_path: &str, source: &str) -> Result<ParsedFile> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .context("failed to load tree-sitter-rust grammar")?;
        let tree = parser
            .parse(source, None)
            .context("tree-sitter-rust returned None")?;

        let mut out = ParsedFile::default();
        let file_id = file_symbol_id(file_path);
        out.symbols.push(make_file_symbol(file_path, source));

        let root = tree.root_node();
        let bytes = source.as_bytes();

        walk_items(root, file_path, bytes, &file_id, None, &mut out);
        Ok(out)
    }
}

fn node_text<'a>(node: Node<'a>, bytes: &'a [u8]) -> &'a str {
    node.utf8_text(bytes).unwrap_or("")
}

fn find_visibility(node: Node, bytes: &[u8]) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return node_text(child, bytes).trim().to_string();
        }
    }
    "private".to_string()
}

fn extract_preceding_docs_and_test_attr(node: Node, bytes: &[u8]) -> (String, bool) {
    let mut docs = Vec::new();
    let mut is_test = false;
    let mut prev = node.prev_sibling();
    while let Some(sib) = prev {
        match sib.kind() {
            "line_comment" => {
                let txt = node_text(sib, bytes).trim();
                if let Some(rest) = txt.strip_prefix("///") {
                    docs.push(rest.trim().to_string());
                } else {
                    break;
                }
            }
            "attribute_item" => {
                let txt = node_text(sib, bytes);
                if txt.contains("test") {
                    is_test = true;
                }
            }
            _ => break,
        }
        prev = sib.prev_sibling();
    }
    docs.reverse();
    let summary = docs.into_iter().next().unwrap_or_default();
    (compact_signature(&summary, 110), is_test)
}

fn extract_signature_before_body(node: Node, bytes: &[u8]) -> String {
    let full = node_text(node, bytes);
    if let Some(body) = node.child_by_field_name("body") {
        let rel = body.start_byte().saturating_sub(node.start_byte());
        if rel <= full.len() {
            return compact_signature(&full[..rel], 140);
        }
    }
    if let Some(idx) = full.find('{') {
        compact_signature(&full[..idx], 140)
    } else {
        compact_signature(full, 140)
    }
}

fn walk_items(
    parent: Node,
    file_path: &str,
    bytes: &[u8],
    parent_id: &str,
    impl_type: Option<&str>,
    out: &mut ParsedFile,
) {
    let mut cursor = parent.walk();
    for node in parent.children(&mut cursor) {
        let start_line = node.start_position().row + 1;
        let end_line = node.end_position().row + 1;

        match node.kind() {
            "use_declaration" => {
                let txt = node_text(node, bytes);
                for target in parse_rust_use_targets(txt) {
                    out.refs.push(RawReference {
                        source_symbol_id: parent_id.to_string(),
                        target_name: target,
                        kind: EdgeKind::Imports,
                        line: start_line,
                    });
                }
            }
            "mod_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = node_text(name_node, bytes).to_string();
                    let qname = name.clone();
                    let sym_id = make_symbol_id(file_path, &qname, start_line);
                    let (doc_summary, _) = extract_preceding_docs_and_test_attr(node, bytes);
                    let sig = extract_signature_before_body(node, bytes);
                    let src = node_text(node, bytes);

                    out.symbols.push(Symbol {
                        id: sym_id.clone(),
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: qname,
                        kind: SymbolKind::Module,
                        signature: sig,
                        start_line,
                        end_line,
                        token_count: count_tokens(src),
                        visibility: find_visibility(node, bytes),
                        doc_summary,
                        parent_id: Some(parent_id.to_string()),
                    });

                    if let Some(body) = node.child_by_field_name("body") {
                        walk_items(body, file_path, bytes, &sym_id, None, out);
                    } else {
                        // External module declaration `mod foo;` -> import edge
                        out.refs.push(RawReference {
                            source_symbol_id: parent_id.to_string(),
                            target_name: format!("mod::{name}"),
                            kind: EdgeKind::Imports,
                            line: start_line,
                        });
                    }
                }
            }
            "struct_item" | "enum_item" | "trait_item" | "type_item" | "const_item"
            | "static_item" | "macro_definition" => {
                let kind = match node.kind() {
                    "struct_item" => SymbolKind::Struct,
                    "enum_item" => SymbolKind::Enum,
                    "trait_item" => SymbolKind::Trait,
                    "type_item" => SymbolKind::TypeAlias,
                    "const_item" | "static_item" => SymbolKind::Const,
                    "macro_definition" => SymbolKind::Macro,
                    _ => SymbolKind::Struct,
                };
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = node_text(name_node, bytes).to_string();
                    let sym_id = make_symbol_id(file_path, &name, start_line);
                    let (doc_summary, _) = extract_preceding_docs_and_test_attr(node, bytes);
                    let sig = extract_signature_before_body(node, bytes);
                    let src = node_text(node, bytes);

                    out.symbols.push(Symbol {
                        id: sym_id.clone(),
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: name,
                        kind,
                        signature: sig,
                        start_line,
                        end_line,
                        token_count: count_tokens(src),
                        visibility: find_visibility(node, bytes),
                        doc_summary,
                        parent_id: Some(parent_id.to_string()),
                    });

                    // Collect referenced types inside struct/enum/trait
                    let mut seen_types = HashSet::new();
                    collect_type_refs(node, bytes, &sym_id, &mut seen_types, out);

                    if node.kind() == "trait_item" {
                        if let Some(body) = node.child_by_field_name("body") {
                            walk_items(body, file_path, bytes, &sym_id, Some(&sym_id), out);
                        }
                    }
                }
            }
            "impl_item" => {
                let target_type = node
                    .child_by_field_name("type")
                    .map(|n| clean_rust_type_name(node_text(n, bytes)))
                    .unwrap_or_else(|| "Unknown".to_string());
                let trait_name = node
                    .child_by_field_name("trait")
                    .map(|n| clean_rust_type_name(node_text(n, bytes)));

                let qname = if let Some(ref tr) = trait_name {
                    format!("impl {tr} for {target_type}")
                } else {
                    format!("impl {target_type}")
                };
                let sym_id = make_symbol_id(file_path, &qname, start_line);
                let sig = extract_signature_before_body(node, bytes);
                let src = node_text(node, bytes);

                out.symbols.push(Symbol {
                    id: sym_id.clone(),
                    file_path: file_path.to_string(),
                    name: qname.clone(),
                    qualified_name: qname,
                    kind: SymbolKind::Impl,
                    signature: sig,
                    start_line,
                    end_line,
                    token_count: count_tokens(src),
                    visibility: "impl".to_string(),
                    doc_summary: String::new(),
                    parent_id: Some(parent_id.to_string()),
                });

                if let Some(tr) = trait_name {
                    out.refs.push(RawReference {
                        source_symbol_id: sym_id.clone(),
                        target_name: tr,
                        kind: EdgeKind::Implements,
                        line: start_line,
                    });
                }
                out.refs.push(RawReference {
                    source_symbol_id: sym_id.clone(),
                    target_name: target_type.clone(),
                    kind: EdgeKind::UsesType,
                    line: start_line,
                });

                if let Some(body) = node.child_by_field_name("body") {
                    walk_items(body, file_path, bytes, &sym_id, Some(&target_type), out);
                }
            }
            "function_item" | "function_signature_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = node_text(name_node, bytes).to_string();
                    let (doc_summary, is_test) = extract_preceding_docs_and_test_attr(node, bytes);
                    let (kind, qname) = if is_test {
                        (SymbolKind::Test, name.clone())
                    } else if let Some(ty) = impl_type {
                        if ty.contains("::") {
                            (SymbolKind::Method, name.clone())
                        } else {
                            (SymbolKind::Method, format!("{ty}::{name}"))
                        }
                    } else {
                        (SymbolKind::Function, name.clone())
                    };
                    let sym_id = make_symbol_id(file_path, &qname, start_line);
                    let sig = extract_signature_before_body(node, bytes);
                    let src = node_text(node, bytes);

                    out.symbols.push(Symbol {
                        id: sym_id.clone(),
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: qname,
                        kind,
                        signature: sig,
                        start_line,
                        end_line,
                        token_count: count_tokens(src),
                        visibility: find_visibility(node, bytes),
                        doc_summary,
                        parent_id: Some(parent_id.to_string()),
                    });

                    let mut seen_calls = HashSet::new();
                    let mut seen_types = HashSet::new();
                    let call_kind = if is_test {
                        EdgeKind::Tests
                    } else {
                        EdgeKind::Calls
                    };
                    collect_calls(node, bytes, &sym_id, call_kind, &mut seen_calls, out);
                    collect_type_refs(node, bytes, &sym_id, &mut seen_types, out);
                }
            }
            _ => {}
        }
    }
}

fn collect_calls(
    node: Node,
    bytes: &[u8],
    source_id: &str,
    edge_kind: EdgeKind,
    seen: &mut HashSet<String>,
    out: &mut ParsedFile,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "call_expression" => {
                if let Some(func) = child.child_by_field_name("function") {
                    let raw = node_text(func, bytes).trim();
                    let target = extract_callable_name(raw);
                    if is_interesting_call(&target) && seen.insert(target.clone()) {
                        out.refs.push(RawReference {
                            source_symbol_id: source_id.to_string(),
                            target_name: target,
                            kind: edge_kind,
                            line: child.start_position().row + 1,
                        });
                    }
                }
            }
            "macro_invocation" => {
                if let Some(mac) = child.child_by_field_name("macro") {
                    let name = node_text(mac, bytes).trim();
                    if !is_std_macro(name) && seen.insert(name.to_string()) {
                        out.refs.push(RawReference {
                            source_symbol_id: source_id.to_string(),
                            target_name: name.to_string(),
                            kind: edge_kind,
                            line: child.start_position().row + 1,
                        });
                    }
                }
            }
            _ => {}
        }
        collect_calls(child, bytes, source_id, edge_kind, seen, out);
    }
}

fn collect_type_refs(
    node: Node,
    bytes: &[u8],
    source_id: &str,
    seen: &mut HashSet<String>,
    out: &mut ParsedFile,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "type_identifier" {
            let name = node_text(child, bytes).trim();
            if !is_builtin_rust_type(name) && seen.insert(name.to_string()) {
                out.refs.push(RawReference {
                    source_symbol_id: source_id.to_string(),
                    target_name: name.to_string(),
                    kind: EdgeKind::UsesType,
                    line: child.start_position().row + 1,
                });
            }
        }
        // Don't recurse into nested functions inside a struct/impl when scanning header types
        if child.kind() != "block" && child.kind() != "declaration_list" {
            collect_type_refs(child, bytes, source_id, seen, out);
        }
    }
}

fn clean_rust_type_name(raw: &str) -> String {
    let base = raw.split('<').next().unwrap_or(raw).trim();
    base.rsplit("::").next().unwrap_or(base).trim().to_string()
}

fn extract_callable_name(raw: &str) -> String {
    // Handle `foo.bar`, `Type::method`, `a::b::func`
    if let Some(dot) = raw.rfind('.') {
        return raw[dot + 1..]
            .split("::<")
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
    }
    let clean = raw.split("::<").next().unwrap_or(raw).trim();
    if let Some(parts) = clean.rsplit_once("::") {
        let left = parts.0.rsplit("::").next().unwrap_or(parts.0);
        if left.chars().next().is_some_and(|c| c.is_uppercase()) {
            return format!("{left}::{}", parts.1);
        }
        return parts.1.to_string();
    }
    clean.to_string()
}

fn parse_rust_use_targets(use_stmt: &str) -> Vec<String> {
    let trimmed = use_stmt
        .trim()
        .trim_start_matches("pub")
        .trim()
        .trim_start_matches("(crate)")
        .trim()
        .trim_start_matches("use")
        .trim()
        .trim_end_matches(';')
        .trim();

    let mut targets = Vec::new();
    if let Some((prefix, group)) = trimmed.split_once('{') {
        let prefix = prefix.trim().trim_end_matches("::");
        let inside = group.trim_end_matches('}');
        for item in inside.split(',') {
            let clean = item.split(" as ").next().unwrap_or(item).trim();
            if !clean.is_empty() && clean != "*" {
                targets.push(format!("{prefix}::{clean}"));
            }
        }
    } else if !trimmed.is_empty() {
        let clean = trimmed.split(" as ").next().unwrap_or(trimmed).trim();
        targets.push(clean.to_string());
    }
    targets
}

fn is_interesting_call(name: &str) -> bool {
    if name.len() < 2 {
        return false;
    }
    !matches!(
        name,
        "clone"
            | "to_string"
            | "to_owned"
            | "into"
            | "from"
            | "unwrap"
            | "unwrap_or"
            | "unwrap_or_default"
            | "unwrap_or_else"
            | "expect"
            | "ok"
            | "err"
            | "is_some"
            | "is_none"
            | "is_ok"
            | "is_err"
            | "is_empty"
            | "len"
            | "push"
            | "pop"
            | "iter"
            | "into_iter"
            | "map"
            | "filter"
            | "collect"
            | "insert"
            | "get"
            | "contains"
            | "trim"
            | "as_str"
            | "as_bytes"
            | "display"
            | "join"
            | "split"
            | "lines"
            | "chars"
            | "next"
            | "take"
            | "sort"
            | "sort_by"
            | "extend"
            | "entry"
            | "or_default"
            | "or_insert"
            | "or_insert_with"
            | "saturating_sub"
            | "max"
            | "min"
            | "Ok"
            | "Err"
            | "Some"
            | "None"
            | "Vec::new"
            | "HashMap::new"
            | "HashSet::new"
            | "String::new"
            | "String::from"
            | "Path::new"
            | "PathBuf::from"
    )
}

fn is_std_macro(name: &str) -> bool {
    matches!(
        name,
        "println"
            | "eprintln"
            | "print"
            | "eprint"
            | "format"
            | "vec"
            | "assert"
            | "assert_eq"
            | "assert_ne"
            | "matches"
            | "write"
            | "writeln"
            | "panic"
            | "todo"
            | "unimplemented"
            | "unreachable"
            | "dbg"
            | "env"
            | "include_str"
            | "include_bytes"
            | "cfg"
    )
}

fn is_builtin_rust_type(name: &str) -> bool {
    matches!(
        name,
        "Self"
            | "String"
            | "Vec"
            | "Option"
            | "Result"
            | "Box"
            | "Arc"
            | "Rc"
            | "HashMap"
            | "HashSet"
            | "BTreeMap"
            | "BTreeSet"
            | "Path"
            | "PathBuf"
            | "OsStr"
            | "OsString"
            | "Cow"
            | "Pin"
            | "PhantomData"
            | "Send"
            | "Sync"
            | "Clone"
            | "Copy"
            | "Debug"
            | "Display"
            | "Default"
            | "Serialize"
            | "Deserialize"
            | "PartialEq"
            | "Eq"
            | "Hash"
            | "Ord"
            | "PartialOrd"
    )
}
