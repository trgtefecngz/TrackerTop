//! Python extractor on tree-sitter: imports, classes (with base classes), functions,
//! methods and tests, and the calls each function makes. Calls are taken from the syntax
//! tree, so a name inside a string or comment is never a call, and a `def` inside a
//! docstring is never a symbol.

use crate::model::{EdgeKind, RawReference, Symbol, SymbolKind};
use crate::parser::{
    LanguageExtractor, ParsedFile, compact_signature, file_symbol_id, make_file_symbol,
    make_symbol_id,
};
use crate::tokens::count_tokens;
use anyhow::{Context, Result};
use std::collections::HashSet;
use tree_sitter::{Node, Parser};

pub struct PythonExtractor;

impl LanguageExtractor for PythonExtractor {
    fn id(&self) -> &'static str {
        "python"
    }

    fn display_name(&self) -> &'static str {
        "Python"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["py", "pyi"]
    }

    fn color_hex(&self) -> &'static str {
        "#facc15"
    }

    fn parse(&self, file_path: &str, source: &str) -> Result<ParsedFile> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .context("failed to load tree-sitter-python grammar")?;
        let tree = parser
            .parse(source, None)
            .context("tree-sitter-python returned None")?;
        let mut out = ParsedFile::default();
        let file_id = file_symbol_id(file_path);
        out.symbols.push(make_file_symbol(file_path, source));
        if !tree.root_node().has_error() {
            let cx = Cx {
                file_path,
                bytes: source.as_bytes(),
                file_id: &file_id,
            };
            walk(&cx, tree.root_node(), None, &mut out);
            return Ok(out);
        }
        // A syntax error can swallow the rest of the file: parse each top-level piece alone
        let starts = |l: &str| {
            ["def ", "async def ", "class ", "@"]
                .iter()
                .any(|p| l.starts_with(p))
        };
        for piece in crate::parser::recovery_segments(source, starts, |l| l.starts_with('@')) {
            let Some(tree) = parser.parse(&piece, None) else {
                continue;
            };
            let cx = Cx {
                file_path,
                bytes: piece.as_bytes(),
                file_id: &file_id,
            };
            walk(&cx, tree.root_node(), None, &mut out);
        }
        Ok(out)
    }
}

struct Cx<'a> {
    file_path: &'a str,
    bytes: &'a [u8],
    file_id: &'a str,
}

fn text<'a>(node: Node<'a>, bytes: &'a [u8]) -> &'a str {
    node.utf8_text(bytes).unwrap_or("")
}

fn visibility(name: &str) -> String {
    if name.starts_with('_') {
        "private"
    } else {
        "pub"
    }
    .to_string()
}

/// `def f(a, b) -> int` / `class C(Base)`: the header up to the body, on one line.
fn header(node: Node, bytes: &[u8]) -> String {
    let end = node
        .child_by_field_name("body")
        .map_or(node.end_byte(), |b| b.start_byte());
    let raw = std::str::from_utf8(&bytes[node.start_byte()..end]).unwrap_or("");
    compact_signature(raw.trim().trim_end_matches(':'), 140)
}

/// First line of a docstring (the body's leading string statement).
fn docstring(node: Node, bytes: &[u8]) -> String {
    let Some(body) = node.child_by_field_name("body") else {
        return String::new();
    };
    let Some(first) = body.named_child(0) else {
        return String::new();
    };
    if first.kind() != "expression_statement" {
        return String::new();
    }
    let Some(s) = first.named_child(0).filter(|s| s.kind() == "string") else {
        return String::new();
    };
    let raw = text(s, bytes)
        .trim_start_matches(['r', 'b', 'u', 'R', 'B', 'U'])
        .trim_matches(|c| c == '"' || c == '\'');
    raw.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| compact_signature(l, 110))
        .unwrap_or_default()
}

/// Module statements and class bodies. `class_ctx` is the enclosing class (name, id).
fn walk(cx: &Cx, node: Node, class_ctx: Option<(&str, &str)>, out: &mut ParsedFile) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let def = if child.kind() == "decorated_definition" {
            match child.child_by_field_name("definition") {
                Some(d) => d,
                None => continue,
            }
        } else {
            child
        };
        match def.kind() {
            "import_statement" => imports(cx, def, out),
            "import_from_statement" => from_imports(cx, def, out),
            "class_definition" => class(cx, def, out),
            "function_definition" => function(cx, def, class_ctx, out),
            // `if TYPE_CHECKING:`, `try:`, loops and `match` hold definitions too. `ERROR`
            // wraps the rest of a file after a syntax error (an unclosed bracket mid-edit):
            // its definitions are still found, as the old line-based parser found them.
            "if_statement" | "try_statement" | "with_statement" | "else_clause" | "elif_clause"
            | "except_clause" | "finally_clause" | "block" | "for_statement"
            | "while_statement" | "match_statement" | "case_clause" | "ERROR" => {
                walk(cx, def, class_ctx, out)
            }
            _ => {}
        }
    }
}

fn push_ref(out: &mut ParsedFile, source: &str, target: String, kind: EdgeKind, line: usize) {
    out.refs.push(RawReference {
        source_symbol_id: source.to_string(),
        target_name: target,
        kind,
        line,
    });
}

/// `import a.b, c as d` -> imports of `a/b.py` and `c.py`.
fn imports(cx: &Cx, node: Node, out: &mut ParsedFile) {
    let line = node.start_position().row + 1;
    let mut cursor = node.walk();
    for name in node.children_by_field_name("name", &mut cursor) {
        let dotted = if name.kind() == "aliased_import" {
            name.child_by_field_name("name")
        } else {
            Some(name)
        };
        if let Some(d) = dotted {
            let m = text(d, cx.bytes).trim().replace('.', "/");
            if !m.is_empty() {
                push_ref(out, cx.file_id, format!("{m}.py"), EdgeKind::Imports, line);
            }
        }
    }
}

/// `from a.b import X, Y as Z` -> an import of `a/b.py` and references to `X`, `Y`.
fn from_imports(cx: &Cx, node: Node, out: &mut ParsedFile) {
    let line = node.start_position().row + 1;
    if let Some(module) = node.child_by_field_name("module_name") {
        let m = text(module, cx.bytes)
            .trim()
            .trim_start_matches('.')
            .replace('.', "/");
        if !m.is_empty() {
            push_ref(out, cx.file_id, format!("{m}.py"), EdgeKind::Imports, line);
        }
    }
    let mut cursor = node.walk();
    for name in node.children_by_field_name("name", &mut cursor) {
        let item = if name.kind() == "aliased_import" {
            name.child_by_field_name("name")
        } else {
            Some(name)
        };
        if let Some(item) = item {
            let n = text(item, cx.bytes).trim();
            if !n.is_empty() {
                push_ref(out, cx.file_id, n.to_string(), EdgeKind::References, line);
            }
        }
    }
}

fn class(cx: &Cx, node: Node, out: &mut ParsedFile) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, cx.bytes).to_string();
    let line = node.start_position().row + 1;
    let sym_id = make_symbol_id(cx.file_path, &name, line);
    out.symbols.push(Symbol {
        id: sym_id.clone(),
        file_path: cx.file_path.to_string(),
        name: name.clone(),
        qualified_name: name.clone(),
        kind: SymbolKind::Class,
        signature: header(node, cx.bytes),
        start_line: line,
        end_line: node.end_position().row + 1,
        token_count: count_tokens(text(node, cx.bytes)),
        visibility: visibility(&name),
        doc_summary: docstring(node, cx.bytes),
        parent_id: Some(cx.file_id.to_string()),
    });
    if let Some(bases) = node.child_by_field_name("superclasses") {
        let mut cursor = bases.walk();
        for base in bases.named_children(&mut cursor) {
            // `Base`, `pkg.Base`; keyword arguments (`metaclass=...`) are not bases
            let b = match base.kind() {
                "identifier" => text(base, cx.bytes),
                "attribute" => base
                    .child_by_field_name("attribute")
                    .map_or("", |a| text(a, cx.bytes)),
                _ => "",
            };
            if !b.is_empty() && !matches!(b, "object" | "ABC" | "Exception") {
                push_ref(out, &sym_id, b.to_string(), EdgeKind::Implements, line);
            }
        }
    }
    if let Some(body) = node.child_by_field_name("body") {
        walk(cx, body, Some((&name, &sym_id)), out);
    }
}

fn function(cx: &Cx, node: Node, class_ctx: Option<(&str, &str)>, out: &mut ParsedFile) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, cx.bytes).to_string();
    let line = node.start_position().row + 1;
    let is_test = name.starts_with("test_");
    let (kind, qname, parent) = if is_test {
        (SymbolKind::Test, name.clone(), cx.file_id.to_string())
    } else if let Some((cname, cid)) = class_ctx {
        (
            SymbolKind::Method,
            format!("{cname}::{name}"),
            cid.to_string(),
        )
    } else {
        (SymbolKind::Function, name.clone(), cx.file_id.to_string())
    };
    let sym_id = make_symbol_id(cx.file_path, &qname, line);
    out.symbols.push(Symbol {
        id: sym_id.clone(),
        file_path: cx.file_path.to_string(),
        name: name.clone(),
        qualified_name: qname,
        kind,
        signature: header(node, cx.bytes),
        start_line: line,
        end_line: node.end_position().row + 1,
        token_count: count_tokens(text(node, cx.bytes)),
        visibility: visibility(&name),
        doc_summary: docstring(node, cx.bytes),
        parent_id: Some(parent),
    });
    let kind = if is_test {
        EdgeKind::Tests
    } else {
        EdgeKind::Calls
    };
    // Nested functions and classes are part of this function: their calls count as its own
    if let Some(body) = node.child_by_field_name("body") {
        let mut seen = HashSet::new();
        calls(cx, body, &name, &sym_id, kind, &mut seen, out);
    }
}

/// Calls in a function body: `f(...)` -> `f`, `obj.m(...)` -> `m` (the receiver's type
/// is unknown, so the bare name is resolved).
fn calls(
    cx: &Cx,
    node: Node,
    self_name: &str,
    source_id: &str,
    kind: EdgeKind,
    seen: &mut HashSet<String>,
    out: &mut ParsedFile,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "call"
            && let Some(f) = child.child_by_field_name("function")
        {
            let name = match f.kind() {
                "identifier" => text(f, cx.bytes),
                "attribute" => f
                    .child_by_field_name("attribute")
                    .map_or("", |a| text(a, cx.bytes)),
                _ => "",
            };
            if name.len() >= 2
                && name != self_name
                && !is_py_builtin(name)
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
        calls(cx, child, self_name, source_id, kind, seen, out);
    }
}

fn is_py_builtin(name: &str) -> bool {
    matches!(
        name,
        "def"
            | "class"
            | "lambda"
            | "return"
            | "if"
            | "elif"
            | "while"
            | "for"
            | "print"
            | "len"
            | "range"
            | "str"
            | "int"
            | "float"
            | "bool"
            | "list"
            | "dict"
            | "set"
            | "tuple"
            | "enumerate"
            | "zip"
            | "map"
            | "filter"
            | "sorted"
            | "min"
            | "max"
            | "sum"
            | "any"
            | "all"
            | "isinstance"
            | "hasattr"
            | "getattr"
            | "setattr"
            | "open"
            | "super"
            | "append"
            | "extend"
            | "get"
            | "items"
            | "keys"
            | "values"
            | "join"
            | "split"
            | "strip"
            | "format"
    )
}

#[cfg(test)]
mod tests {
    use super::PythonExtractor;
    use crate::model::EdgeKind;
    use crate::parser::LanguageExtractor;

    const SRC: &str = r#"import os.path, json as j
from .models import (
    User,
    Account as Acc,
)

class Service(BaseService, metaclass=Meta):
    """Handles users.

    More text."""

    def __init__(self):
        self.store = make_store()

    @property
    def name(
        self,
        prefix: str = "def fake(x):",
    ) -> str:
        # build_user(1) in a comment is not a call
        msg = "load_user(2) in a string is not a call"
        return self.store.fetch_name(prefix)

    async def refresh(self):
        def inner():
            return reload_cache()
        return inner()


def build_user(uid):
    '''Build one user.'''
    return User(uid)


def test_build_user():
    assert build_user("1") is not None
"#;

    #[test]
    fn golden_symbols_and_references() {
        let parsed = PythonExtractor.parse("svc/service.py", SRC).unwrap();
        let syms: Vec<(String, String, usize, usize)> = parsed
            .symbols
            .iter()
            .skip(1)
            .map(|s| {
                (
                    s.kind.as_str().to_string(),
                    s.qualified_name.clone(),
                    s.start_line,
                    s.end_line,
                )
            })
            .collect();
        let want: Vec<(String, String, usize, usize)> = [
            ("class", "Service", 7, 27),
            ("method", "Service::__init__", 12, 13),
            ("method", "Service::name", 16, 22),
            ("method", "Service::refresh", 24, 27),
            ("fn", "build_user", 30, 32),
            ("test", "test_build_user", 35, 36),
        ]
        .iter()
        .map(|(k, q, a, b)| (k.to_string(), q.to_string(), *a, *b))
        .collect();
        assert_eq!(syms, want);

        let svc = &parsed.symbols[1];
        assert_eq!(svc.doc_summary, "Handles users.");
        assert_eq!(svc.signature, "class Service(BaseService, metaclass=Meta)");
        let name = &parsed.symbols[3];
        assert!(name.signature.starts_with("def name( self, prefix: str"));
        assert_eq!(parsed.symbols[5].doc_summary, "Build one user.");

        let refs: Vec<(String, String, String)> = parsed
            .refs
            .iter()
            .map(|r| {
                let src = r
                    .source_symbol_id
                    .split("::")
                    .last()
                    .unwrap_or("")
                    .to_string();
                (src, r.kind.as_str().to_string(), r.target_name.clone())
            })
            .collect();
        let has = |src: &str, kind: EdgeKind, target: &str| {
            refs.iter()
                .any(|(s, k, t)| s.starts_with(src) && k == kind.as_str() && t == target)
        };
        assert!(has("__file__", EdgeKind::Imports, "os/path.py"), "{refs:?}");
        assert!(has("__file__", EdgeKind::Imports, "json.py"), "{refs:?}");
        assert!(has("__file__", EdgeKind::Imports, "models.py"), "{refs:?}");
        assert!(has("__file__", EdgeKind::References, "User"), "{refs:?}");
        assert!(has("__file__", EdgeKind::References, "Account"), "{refs:?}");
        assert!(
            has("Service#", EdgeKind::Implements, "BaseService"),
            "{refs:?}"
        );
        assert!(!refs.iter().any(|(_, _, t)| t == "Meta"), "{refs:?}");
        assert!(has("__init__#", EdgeKind::Calls, "make_store"), "{refs:?}");
        assert!(has("name#", EdgeKind::Calls, "fetch_name"), "{refs:?}");
        assert!(has("refresh#", EdgeKind::Calls, "reload_cache"), "{refs:?}");
        assert!(has("build_user#", EdgeKind::Calls, "User"), "{refs:?}");
        assert!(
            has("test_build_user#", EdgeKind::Tests, "build_user"),
            "{refs:?}"
        );
        // Text in strings and comments is never a call; a `def` in a string is no symbol
        for not_call in ["build_user", "load_user", "fake"] {
            assert!(
                !refs
                    .iter()
                    .any(|(s, k, t)| s.starts_with("name#") && k == "CALLS" && t == not_call),
                "{not_call}: {refs:?}"
            );
        }
        assert!(
            !parsed
                .symbols
                .iter()
                .any(|s| s.name == "fake" || s.name == "inner")
        );
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::PythonExtractor;
    use crate::parser::LanguageExtractor;

    fn names(src: &str) -> Vec<String> {
        PythonExtractor
            .parse("m.py", src)
            .unwrap()
            .symbols
            .iter()
            .skip(1)
            .map(|s| s.qualified_name.clone())
            .collect()
    }

    /// A file mid-edit (one unclosed bracket) must not lose every later definition.
    #[test]
    fn a_syntax_error_does_not_hide_later_definitions() {
        let src = "import os\nCONFIG = {\n    \"a\": 1,\n\ndef one():\n    return two()\n\ndef two():\n    pass\n\nclass K:\n    def m(self):\n        pass\n";
        let got = names(src);
        for want in ["one", "two", "K", "K::m"] {
            assert!(got.iter().any(|n| n == want), "{want}: {got:?}");
        }
    }

    #[test]
    fn definitions_inside_loops_and_match_are_kept() {
        let src = "for i in range(2):\n    def in_for():\n        pass\nwhile False:\n    def in_while():\n        pass\nmatch x:\n    case 1:\n        def in_case():\n            pass\n";
        let got = names(src);
        for want in ["in_for", "in_while", "in_case"] {
            assert!(got.iter().any(|n| n == want), "{want}: {got:?}");
        }
    }
}
