//! Go extractor on tree-sitter: imports, types (struct, interface, other), functions,
//! methods (`Recv::Name`), tests (`Test*` in `_test.go`) and the calls each function
//! makes. Calls come from the syntax tree, so text in strings and comments is never a
//! call, and braces inside strings never cut a function short.

use crate::model::{EdgeKind, RawReference, Symbol, SymbolKind};
use crate::parser::{
    LanguageExtractor, ParsedFile, compact_signature, file_symbol_id, make_file_symbol,
    make_symbol_id,
};
use crate::tokens::count_tokens;
use anyhow::{Context, Result};
use std::collections::HashSet;
use tree_sitter::{Node, Parser};

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
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .context("failed to load tree-sitter-go grammar")?;
        let tree = parser
            .parse(source, None)
            .context("tree-sitter-go returned None")?;
        let mut out = ParsedFile::default();
        let file_id = file_symbol_id(file_path);
        out.symbols.push(make_file_symbol(file_path, source));
        let bytes = source.as_bytes();
        let root = tree.root_node();
        let mut cursor = root.walk();
        for decl in root.named_children(&mut cursor) {
            match decl.kind() {
                "import_declaration" => imports(decl, bytes, &file_id, &mut out),
                "type_declaration" => types(decl, bytes, file_path, &file_id, &mut out),
                "function_declaration" | "method_declaration" => {
                    function(decl, bytes, file_path, &file_id, &mut out)
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

fn text<'a>(node: Node<'a>, bytes: &'a [u8]) -> &'a str {
    node.utf8_text(bytes).unwrap_or("")
}

fn visibility(name: &str) -> String {
    if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
        "pub"
    } else {
        "private"
    }
    .to_string()
}

/// The declaration's text up to `stop` (its body), on one line.
fn header(node: Node, stop: Option<Node>, bytes: &[u8]) -> String {
    let end = stop.map_or(node.end_byte(), |b| b.start_byte());
    let raw = std::str::from_utf8(&bytes[node.start_byte()..end]).unwrap_or("");
    compact_signature(raw.trim(), 140)
}

/// First line of the `//` comment block directly above `node`.
fn doc_comment(node: Node, bytes: &[u8]) -> String {
    let mut first = None;
    let mut expect_row = node.start_position().row;
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        if p.kind() != "comment" || p.end_position().row + 1 != expect_row {
            break;
        }
        first = Some(p);
        expect_row = p.start_position().row;
        prev = p.prev_sibling();
    }
    first
        .map(|c| text(c, bytes).trim_start_matches('/').trim())
        .filter(|d| !d.is_empty() && !d.starts_with('*'))
        .map(|d| compact_signature(d, 110))
        .unwrap_or_default()
}

fn push_ref(out: &mut ParsedFile, source: &str, target: String, kind: EdgeKind, line: usize) {
    out.refs.push(RawReference {
        source_symbol_id: source.to_string(),
        target_name: target,
        kind,
        line,
    });
}

/// `import "a/b"` or an import block: the package's last path segment (`b`).
fn imports(node: Node, bytes: &[u8], file_id: &str, out: &mut ParsedFile) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "import_spec" {
            if let Some(path) = n.child_by_field_name("path") {
                let p = text(path, bytes).trim_matches(|c| c == '"' || c == '`');
                if let Some(last) = p.rsplit('/').next().filter(|l| !l.is_empty()) {
                    let line = n.start_position().row + 1;
                    push_ref(out, file_id, last.to_string(), EdgeKind::Imports, line);
                }
            }
            continue;
        }
        let mut cursor = n.walk();
        let children: Vec<Node> = n.named_children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
}

/// `type T struct {...}`, `type I interface {...}`, `type A = B`, and grouped `type (...)`.
fn types(decl: Node, bytes: &[u8], file_path: &str, file_id: &str, out: &mut ParsedFile) {
    let mut cursor = decl.walk();
    let specs: Vec<Node> = decl
        .named_children(&mut cursor)
        .filter(|s| matches!(s.kind(), "type_spec" | "type_alias"))
        .collect();
    let grouped = specs.len() > 1
        || text(decl, bytes)
            .trim_start()
            .strip_prefix("type")
            .is_some_and(|r| r.trim_start().starts_with('('));
    for spec in specs {
        let Some(name_node) = spec.child_by_field_name("name") else {
            continue;
        };
        let name = text(name_node, bytes).to_string();
        let ty = spec.child_by_field_name("type");
        let kind = match ty.map(|t| t.kind()) {
            Some("struct_type") if spec.kind() == "type_spec" => SymbolKind::Struct,
            Some("interface_type") if spec.kind() == "type_spec" => SymbolKind::Trait,
            _ => SymbolKind::TypeAlias,
        };
        // A single declaration starts at `type`; one of a group at its own name
        let node = if grouped { spec } else { decl };
        let body = ty.filter(|t| matches!(t.kind(), "struct_type" | "interface_type"));
        let stop = body.and_then(|b| {
            let mut c = b.walk();
            b.children(&mut c)
                .find(|n| n.kind().ends_with("_list") || n.kind() == "{")
        });
        let line = node.start_position().row + 1;
        out.symbols.push(Symbol {
            id: make_symbol_id(file_path, &name, line),
            file_path: file_path.to_string(),
            name: name.clone(),
            qualified_name: name.clone(),
            kind,
            signature: header(node, stop, bytes),
            start_line: line,
            end_line: node.end_position().row + 1,
            token_count: count_tokens(text(node, bytes)),
            visibility: visibility(&name),
            doc_summary: doc_comment(node, bytes),
            parent_id: Some(file_id.to_string()),
        });
    }
}

/// The receiver's type name: `(s *Server)` -> `Server`, `(s Stack[T])` -> `Stack`.
fn receiver_type(decl: Node, bytes: &[u8]) -> Option<String> {
    let recv = decl.child_by_field_name("receiver")?;
    let mut cursor = recv.walk();
    let param = recv
        .named_children(&mut cursor)
        .find(|n| n.kind() == "parameter_declaration")?;
    let mut ty = param.child_by_field_name("type")?;
    loop {
        match ty.kind() {
            "pointer_type" => ty = ty.named_child(0)?,
            "generic_type" => ty = ty.child_by_field_name("type")?,
            "type_identifier" => return Some(text(ty, bytes).to_string()),
            _ => return None,
        }
    }
}

fn function(decl: Node, bytes: &[u8], file_path: &str, file_id: &str, out: &mut ParsedFile) {
    let Some(name_node) = decl.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, bytes).to_string();
    let recv = if decl.kind() == "method_declaration" {
        receiver_type(decl, bytes)
    } else {
        None
    };
    let is_test = name.starts_with("Test") && file_path.ends_with("_test.go");
    let (kind, qname) = if is_test {
        (SymbolKind::Test, name.clone())
    } else if let Some(r) = &recv {
        (SymbolKind::Method, format!("{r}::{name}"))
    } else {
        (SymbolKind::Function, name.clone())
    };
    let line = decl.start_position().row + 1;
    let sym_id = make_symbol_id(file_path, &qname, line);
    let body = decl.child_by_field_name("body");
    out.symbols.push(Symbol {
        id: sym_id.clone(),
        file_path: file_path.to_string(),
        name: name.clone(),
        qualified_name: qname,
        kind,
        signature: header(decl, body, bytes),
        start_line: line,
        end_line: decl.end_position().row + 1,
        token_count: count_tokens(text(decl, bytes)),
        visibility: visibility(&name),
        doc_summary: doc_comment(decl, bytes),
        parent_id: Some(file_id.to_string()),
    });
    if let Some(r) = recv {
        push_ref(out, &sym_id, r, EdgeKind::UsesType, line);
    }
    let kind = if is_test {
        EdgeKind::Tests
    } else {
        EdgeKind::Calls
    };
    if let Some(body) = body {
        let mut seen = HashSet::new();
        calls(body, bytes, &name, &sym_id, kind, &mut seen, out);
    }
}

/// `f(...)` -> `f`, `pkg.F(...)` / `x.M(...)` -> `F` / `M` (bare name, as before).
fn calls(
    node: Node,
    bytes: &[u8],
    self_name: &str,
    source_id: &str,
    kind: EdgeKind,
    seen: &mut HashSet<String>,
    out: &mut ParsedFile,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "call_expression"
            && let Some(f) = child.child_by_field_name("function")
        {
            let name = match f.kind() {
                "identifier" => text(f, bytes),
                "selector_expression" => f
                    .child_by_field_name("field")
                    .map_or("", |n| text(n, bytes)),
                _ => "",
            };
            if name.len() >= 2
                && name != self_name
                && !is_go_builtin(name)
                && seen.insert(name.to_string())
            {
                push_ref(
                    out,
                    source_id,
                    name.to_string(),
                    kind,
                    child.start_position().row + 1,
                );
            }
        }
        calls(child, bytes, self_name, source_id, kind, seen, out);
    }
}

fn is_go_builtin(name: &str) -> bool {
    matches!(
        name,
        "make"
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

#[cfg(test)]
mod tests {
    use super::GoExtractor;
    use crate::parser::LanguageExtractor;

    const SRC: &str = r#"package server

import (
	"fmt"
	cl "example.com/app/cluster"
)

import "strings"

// Server serves requests.
// Second line.
type Server struct {
	name string
}

type (
	Handler interface {
		Serve() error
	}
	ID = string
)

// Start boots the cluster.
func (s *Server) Start(
	port int,
) error {
	msg := "bootCluster(1) { in a string"
	// stopCluster() in a comment
	cl.bootCluster(port)
	return fmt.Errorf("%s %s", msg, strings.ToUpper(s.name))
}

func (st Stack[T]) Push(v T) { st.grow() }

func helper() int { return compute(2) }
"#;

    #[test]
    fn golden_symbols_and_references() {
        let parsed = GoExtractor.parse("server/server.go", SRC).unwrap();
        let syms: Vec<(&str, &str, usize, usize)> = parsed
            .symbols
            .iter()
            .skip(1)
            .map(|s| {
                (
                    s.kind.as_str(),
                    s.qualified_name.as_str(),
                    s.start_line,
                    s.end_line,
                )
            })
            .collect();
        assert_eq!(
            syms,
            vec![
                ("struct", "Server", 12, 14),
                ("trait", "Handler", 17, 19),
                ("type", "ID", 20, 20),
                ("method", "Server::Start", 24, 31),
                ("method", "Stack::Push", 33, 33),
                ("fn", "helper", 35, 35),
            ]
        );
        assert_eq!(parsed.symbols[1].doc_summary, "Server serves requests.");
        assert_eq!(parsed.symbols[1].signature, "type Server struct");
        assert_eq!(parsed.symbols[4].doc_summary, "Start boots the cluster.");
        assert_eq!(
            parsed.symbols[4].signature,
            "func (s *Server) Start( port int, ) error"
        );

        let refs: Vec<(String, &str, &str)> = parsed
            .refs
            .iter()
            .map(|r| {
                (
                    r.source_symbol_id
                        .split("::")
                        .last()
                        .unwrap_or("")
                        .to_string(),
                    r.kind.as_str(),
                    r.target_name.as_str(),
                )
            })
            .collect();
        let has = |src: &str, kind: &str, target: &str| {
            refs.iter()
                .any(|(s, k, t)| s.starts_with(src) && *k == kind && *t == target)
        };
        for pkg in ["fmt", "cluster", "strings"] {
            assert!(has("__file__", "IMPORTS", pkg), "{pkg}: {refs:?}");
        }
        assert!(has("Start#", "USES_TYPE", "Server"), "{refs:?}");
        assert!(has("Start#", "CALLS", "bootCluster"), "{refs:?}");
        assert!(has("Start#", "CALLS", "ToUpper"), "{refs:?}");
        assert!(has("Push#", "USES_TYPE", "Stack"), "{refs:?}");
        assert!(has("Push#", "CALLS", "grow"), "{refs:?}");
        assert!(has("helper#", "CALLS", "compute"), "{refs:?}");
        assert!(!has("Start#", "CALLS", "stopCluster"), "{refs:?}");
        assert!(!has("Start#", "CALLS", "Errorf"), "{refs:?}");
    }
}
