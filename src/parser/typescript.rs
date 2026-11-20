//! TypeScript / JavaScript extractor backed by tree-sitter.
//!
//! `.ts` uses the TypeScript grammar, `.tsx` the TSX grammar and `.js`/`.jsx`/`.mjs`/`.cjs`
//! the JavaScript grammar (which includes JSX). Symbol spans come from the syntax tree, so
//! strings, template literals, regex literals, comments and JSX can no longer make a symbol
//! swallow the code after it (the failure mode of the previous line-based scanner).
//!
//! Extracted: functions (declarations, ambient signatures, `const f = () => …` and
//! `const f = function …`), classes with their methods and arrow-function fields,
//! interfaces, enums, type aliases, `it(…)`/`test(…)` test blocks (also inside `describe`),
//! relative ES/CommonJS/dynamic imports, `extends`/`implements` edges and call edges.

use crate::model::{EdgeKind, RawReference, Symbol, SymbolKind};
use crate::parser::{
    LanguageExtractor, ParsedFile, compact_signature, file_symbol_id, make_file_symbol,
    make_symbol_id,
};
use crate::tokens::count_tokens;
use anyhow::{Context, Result};
use std::collections::HashSet;
use tree_sitter::{Language, Node, Parser};

pub struct TypeScriptExtractor;

impl LanguageExtractor for TypeScriptExtractor {
    fn id(&self) -> &'static str {
        "typescript"
    }

    fn display_name(&self) -> &'static str {
        "TypeScript/JS"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["ts", "tsx", "js", "jsx", "mjs", "cjs"]
    }

    fn is_syntax_tree(&self) -> bool {
        true
    }

    fn color_hex(&self) -> &'static str {
        "#60a5fa"
    }

    fn parse(&self, file_path: &str, source: &str) -> Result<ParsedFile> {
        let mut parser = Parser::new();
        parser
            .set_language(&language_for(file_path))
            .context("failed to load tree-sitter TypeScript/JavaScript grammar")?;
        let tree = parser
            .parse(source, None)
            .context("tree-sitter TypeScript/JavaScript parser returned None")?;

        let mut out = ParsedFile::default();
        let file_id = file_symbol_id(file_path);
        out.symbols.push(make_file_symbol(file_path, source));

        let ctx = Ctx {
            file_path,
            bytes: source.as_bytes(),
            lines: source.lines().collect(),
            file_id: &file_id,
        };
        let root = tree.root_node();
        walk(root, &ctx, &mut out);
        collect_imports(root, &ctx, &mut out);
        Ok(out)
    }
}

fn language_for(file_path: &str) -> Language {
    let ext = file_path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "ts" | "mts" | "cts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        _ => tree_sitter_javascript::LANGUAGE.into(),
    }
}

struct Ctx<'a> {
    file_path: &'a str,
    bytes: &'a [u8],
    lines: Vec<&'a str>,
    file_id: &'a str,
}

impl Ctx<'_> {
    fn text(&self, node: Node) -> &str {
        node.utf8_text(self.bytes).unwrap_or("")
    }

    /// Source text from `start` up to (not including) byte `end`.
    fn slice(&self, start: usize, end: usize) -> &str {
        let end = end.min(self.bytes.len());
        std::str::from_utf8(&self.bytes[start.min(end)..end]).unwrap_or("")
    }

    /// Whole source lines `start_line..=end_line` (1-based), the text read_symbol serves.
    fn block(&self, start_line: usize, end_line: usize) -> String {
        let s = start_line.saturating_sub(1).min(self.lines.len());
        let e = end_line.min(self.lines.len()).max(s);
        self.lines[s..e].join("\n")
    }
}

fn line_of(node: Node) -> usize {
    node.start_position().row + 1
}

fn end_line_of(node: Node) -> usize {
    node.end_position().row + 1
}

/// The node that carries a declaration's modifiers and leading comments: the declaration
/// itself, or the `export …` / `declare …` statement wrapping it.
fn outer_of(node: Node) -> Node {
    let mut cur = node;
    while let Some(p) = cur.parent() {
        if matches!(p.kind(), "export_statement" | "ambient_declaration") {
            cur = p;
        } else {
            break;
        }
    }
    cur
}

fn visibility_of(node: Node) -> String {
    let mut cur = node;
    while let Some(p) = cur.parent() {
        match p.kind() {
            "export_statement" => return "export".to_string(),
            "ambient_declaration" => cur = p,
            _ => break,
        }
    }
    "private".to_string()
}

/// First meaningful line of the comment run directly above `outer` (no blank line between),
/// skipping JSDoc `@tags`. Comments that trail code on the previous line are not docs.
fn doc_of(outer: Node, ctx: &Ctx) -> String {
    let mut cur = outer;
    let mut first: Option<Node> = None;
    while let Some(prev) = cur.prev_named_sibling() {
        if prev.kind() != "comment" || prev.end_position().row + 1 < cur.start_position().row {
            break;
        }
        // A comment sharing its line with earlier code (`foo(); // note`) belongs to that code
        if let Some(before) = prev.prev_sibling()
            && before.end_position().row == prev.start_position().row
        {
            break;
        }
        first = Some(prev);
        cur = prev;
    }
    let Some(comment) = first else {
        return String::new();
    };
    for raw in ctx.text(comment).lines() {
        let clean = raw
            .trim()
            .trim_start_matches("/**")
            .trim_start_matches("/*")
            .trim_start_matches("//")
            .trim_end_matches("*/")
            .trim()
            .trim_start_matches('*')
            .trim_start_matches('/')
            .trim();
        if !clean.is_empty() && !clean.starts_with('@') {
            return compact_signature(clean, 110);
        }
    }
    String::new()
}

#[allow(clippy::too_many_arguments)]
fn push_symbol(
    ctx: &Ctx,
    out: &mut ParsedFile,
    name: &str,
    qualified_name: &str,
    kind: SymbolKind,
    signature: String,
    start_line: usize,
    end_line: usize,
    visibility: String,
    doc_summary: String,
    parent_id: &str,
) -> String {
    let id = make_symbol_id(ctx.file_path, qualified_name, start_line);
    out.symbols.push(Symbol {
        id: id.clone(),
        file_path: ctx.file_path.to_string(),
        name: name.to_string(),
        qualified_name: qualified_name.to_string(),
        kind,
        signature,
        start_line,
        end_line,
        token_count: count_tokens(&ctx.block(start_line, end_line)),
        visibility,
        doc_summary,
        parent_id: Some(parent_id.to_string()),
    });
    id
}

/// Walks statements looking for declarations. Declarations that are extracted are not
/// descended into (nested helpers stay part of their parent), except classes for methods.
fn walk(node: Node, ctx: &Ctx, out: &mut ParsedFile) {
    if node.is_error() {
        extract_broken_tests(node, ctx, out);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "comment" | "import_statement" => {}
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                extract_function(child, ctx, out)
            }
            "class_declaration" | "abstract_class_declaration" => extract_class(child, ctx, out),
            "interface_declaration" => extract_type_like(child, SymbolKind::Trait, ctx, out),
            "enum_declaration" => extract_type_like(child, SymbolKind::Enum, ctx, out),
            "type_alias_declaration" => extract_type_like(child, SymbolKind::TypeAlias, ctx, out),
            "lexical_declaration" | "variable_declaration" => {
                if !extract_function_variables(child, ctx, out) {
                    walk(child, ctx, out);
                }
            }
            "expression_statement" => {
                if !extract_test(child, ctx, out) {
                    walk(child, ctx, out);
                }
            }
            _ => walk(child, ctx, out),
        }
    }
}

fn extract_function(node: Node, ctx: &Ctx, out: &mut ParsedFile) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return; // `export default function () {}` has no name to look up
    };
    let name = ctx.text(name_node).to_string();
    let outer = outer_of(node);
    let sig_end = node
        .child_by_field_name("body")
        .map_or(node.end_byte(), |b| b.start_byte());
    let signature = compact_signature(ctx.slice(outer.start_byte(), sig_end).trim(), 140);
    let start = line_of(outer);
    let id = push_symbol(
        ctx,
        out,
        &name,
        &name,
        SymbolKind::Function,
        signature,
        start,
        end_line_of(outer),
        visibility_of(node),
        doc_of(outer, ctx),
        ctx.file_id,
    );
    collect_calls(node, &name, &id, EdgeKind::Calls, ctx, out);
}

fn extract_type_like(node: Node, kind: SymbolKind, ctx: &Ctx, out: &mut ParsedFile) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = ctx.text(name_node).to_string();
    let outer = outer_of(node);
    let sig_end = node
        .child_by_field_name("body")
        .filter(|_| kind != SymbolKind::TypeAlias)
        .map_or(node.end_byte(), |b| b.start_byte());
    let signature = compact_signature(ctx.slice(outer.start_byte(), sig_end).trim(), 140);
    let id = push_symbol(
        ctx,
        out,
        &name,
        &name,
        kind,
        signature,
        line_of(outer),
        end_line_of(outer),
        visibility_of(node),
        doc_of(outer, ctx),
        ctx.file_id,
    );
    if kind == SymbolKind::Trait {
        let mut cursor = node.walk();
        for clause in node.named_children(&mut cursor) {
            if clause.kind() == "extends_type_clause" {
                push_heritage(clause, &id, ctx, out);
            }
        }
    }
}

fn extract_class(node: Node, ctx: &Ctx, out: &mut ParsedFile) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = ctx.text(name_node).to_string();
    let outer = outer_of(node);
    let body = node.child_by_field_name("body");
    let sig_end = body.map_or(node.end_byte(), |b| b.start_byte());
    let signature = compact_signature(ctx.slice(outer.start_byte(), sig_end).trim(), 140);
    let class_id = push_symbol(
        ctx,
        out,
        &name,
        &name,
        SymbolKind::Class,
        signature,
        line_of(outer),
        end_line_of(outer),
        visibility_of(node),
        doc_of(outer, ctx),
        ctx.file_id,
    );

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "class_heritage" {
            let mut c2 = child.walk();
            let clauses: Vec<Node> = child.named_children(&mut c2).collect();
            if clauses
                .iter()
                .any(|c| matches!(c.kind(), "extends_clause" | "implements_clause"))
            {
                for clause in clauses {
                    push_heritage(clause, &class_id, ctx, out);
                }
            } else {
                // JavaScript grammar: `class A extends B` puts the expression directly here
                push_heritage(child, &class_id, ctx, out);
            }
        }
    }

    let Some(body) = body else {
        return;
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        let (name_node, fn_node) = match member.kind() {
            "method_definition" => (member.child_by_field_name("name"), Some(member)),
            "public_field_definition" | "field_definition" => {
                let value = member.child_by_field_name("value");
                let is_fn = value.is_some_and(|v| {
                    matches!(
                        v.kind(),
                        "arrow_function" | "function_expression" | "function"
                    )
                });
                if !is_fn {
                    continue;
                }
                let name = member
                    .child_by_field_name("name")
                    .or_else(|| member.child_by_field_name("property"));
                (name, value)
            }
            _ => continue,
        };
        let (Some(name_node), Some(fn_node)) = (name_node, fn_node) else {
            continue;
        };
        let method = ctx.text(name_node).to_string();
        let qname = format!("{name}::{method}");
        let sig_end = fn_node
            .child_by_field_name("body")
            .map_or(member.end_byte(), |b| b.start_byte());
        let signature = compact_signature(ctx.slice(member.start_byte(), sig_end).trim(), 130);
        let id = push_symbol(
            ctx,
            out,
            &method,
            &qname,
            SymbolKind::Method,
            signature,
            line_of(member),
            end_line_of(member),
            "method".to_string(),
            doc_of(member, ctx),
            &class_id,
        );
        collect_calls(member, &method, &id, EdgeKind::Calls, ctx, out);
    }
}

/// `const f = () => …`, `let g = async function () {…}`: one Function per declarator whose
/// value is a function. Returns false when the declaration holds no function at all.
fn extract_function_variables(node: Node, ctx: &Ctx, out: &mut ParsedFile) -> bool {
    let outer = outer_of(node);
    let mut cursor = node.walk();
    let declarators: Vec<Node> = node
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "variable_declarator")
        .collect();
    let single = declarators.len() == 1;
    let mut any = false;
    for decl in declarators {
        let (Some(name_node), Some(value)) = (
            decl.child_by_field_name("name"),
            decl.child_by_field_name("value"),
        ) else {
            continue;
        };
        let is_arrow = value.kind() == "arrow_function";
        if !is_arrow
            && !matches!(
                value.kind(),
                "function_expression" | "function" | "generator_function"
            )
        {
            continue;
        }
        if name_node.kind() != "identifier" {
            continue; // destructuring patterns have no single name
        }
        any = true;
        let name = ctx.text(name_node).to_string();
        let (start_node, end_node) = if single { (outer, outer) } else { (decl, decl) };
        let sig_end = value
            .child_by_field_name("body")
            .map_or(value.end_byte(), |b| b.start_byte());
        let raw = ctx.slice(start_node.start_byte(), sig_end).trim();
        let signature = if is_arrow {
            let head = raw.strip_suffix("=>").unwrap_or(raw).trim_end();
            format!("{} => ...", compact_signature(head, 140))
        } else {
            compact_signature(raw, 140)
        };
        let id = push_symbol(
            ctx,
            out,
            &name,
            &name,
            SymbolKind::Function,
            signature,
            line_of(start_node),
            end_line_of(end_node),
            visibility_of(node),
            if single {
                doc_of(outer, ctx)
            } else {
                String::new()
            },
            ctx.file_id,
        );
        collect_calls(value, &name, &id, EdgeKind::Calls, ctx, out);
    }
    any
}

/// `it("name", …)` / `test("name", …)` (also `.only` / `.skip` / `.todo` / `.concurrent`).
/// The test is exactly the call: it ends where the call's parenthesis closes.
fn extract_test(stmt: Node, ctx: &Ctx, out: &mut ParsedFile) -> bool {
    let Some(call) = stmt
        .named_child(0)
        .filter(|c| c.kind() == "call_expression")
    else {
        return false;
    };
    let Some(callee) = call.child_by_field_name("function") else {
        return false;
    };
    let is_test_fn = |n: Node| matches!(ctx.text(n), "it" | "test");
    let is_test = match callee.kind() {
        "identifier" => is_test_fn(callee),
        "member_expression" => {
            callee
                .child_by_field_name("object")
                .is_some_and(|o| o.kind() == "identifier" && is_test_fn(o))
                && callee
                    .child_by_field_name("property")
                    .is_some_and(|p| matches!(ctx.text(p), "only" | "skip" | "todo" | "concurrent"))
        }
        _ => false,
    };
    if !is_test {
        return false;
    }
    let Some(args) = call.child_by_field_name("arguments") else {
        return false;
    };
    let mut cursor = args.walk();
    let arg_nodes: Vec<Node> = args.named_children(&mut cursor).collect();
    let Some(name) = arg_nodes.first().and_then(|a| string_value(*a, ctx)) else {
        return false;
    };
    // An unterminated call (truncated or broken source) would otherwise run to the end of
    // the file; claim only its first line instead of swallowing the code after it.
    let broken = call.has_error() || call.is_missing() || args.is_missing() || {
        let last = args.child(args.child_count().saturating_sub(1));
        last.is_none_or(|l| l.kind() != ")" || l.is_missing())
    };
    let start = line_of(stmt);
    let end = if broken { start } else { end_line_of(call) };
    let body_start = arg_nodes
        .iter()
        .skip(1)
        .filter(|a| {
            matches!(
                a.kind(),
                "arrow_function" | "function_expression" | "function"
            )
        })
        .find_map(|a| a.child_by_field_name("body"))
        .filter(|b| b.kind() == "statement_block")
        .map(|b| b.start_byte());
    let sig_raw = match body_start {
        Some(b) if !broken => ctx.slice(call.start_byte(), b),
        _ => ctx.lines.get(start - 1).copied().unwrap_or(""),
    };
    let id = push_symbol(
        ctx,
        out,
        &name,
        &name,
        SymbolKind::Test,
        compact_signature(sig_raw.trim(), 120),
        start,
        end,
        "test".to_string(),
        doc_of(stmt, ctx),
        ctx.file_id,
    );
    if !broken {
        collect_calls(args, &name, &id, EdgeKind::Tests, ctx, out);
    }
    true
}

/// Inside a syntax error, `it("name"` / `test("name"` whose call never closes: the parser
/// cannot tell where the test ends, so it is recorded as its first line only (no call
/// edges) instead of being dropped or swallowing the code after it.
fn extract_broken_tests(error: Node, ctx: &Ctx, out: &mut ParsedFile) {
    let mut cursor = error.walk();
    let children: Vec<Node> = error.named_children(&mut cursor).collect();
    for pair in children.windows(2) {
        let (callee, arg) = (pair[0], pair[1]);
        if callee.kind() != "identifier" || !matches!(ctx.text(callee), "it" | "test") {
            continue;
        }
        let Some(name) = string_value(arg, ctx) else {
            continue;
        };
        let start = line_of(callee);
        let first_line = ctx.lines.get(start - 1).copied().unwrap_or("");
        push_symbol(
            ctx,
            out,
            &name,
            &name,
            SymbolKind::Test,
            compact_signature(first_line.trim(), 120),
            start,
            start,
            "test".to_string(),
            String::new(),
            ctx.file_id,
        );
    }
}

/// Contents of a string or template literal (without its quotes).
fn string_value(node: Node, ctx: &Ctx) -> Option<String> {
    match node.kind() {
        "string" => {
            let t = ctx.text(node);
            Some(
                t.get(1..t.len().saturating_sub(1))
                    .unwrap_or("")
                    .to_string(),
            )
        }
        "template_string" => {
            let t = ctx.text(node);
            Some(t.trim_matches('`').to_string())
        }
        _ => None,
    }
}

/// Base type/class name of one heritage entry (`Base`, `ns.Base` → `Base`, `Base<T>` → `Base`).
fn base_name(node: Node, ctx: &Ctx) -> Option<String> {
    match node.kind() {
        "identifier" | "type_identifier" => Some(ctx.text(node).to_string()),
        "generic_type" => node
            .child_by_field_name("name")
            .and_then(|n| base_name(n, ctx)),
        "member_expression" => node
            .child_by_field_name("property")
            .map(|p| ctx.text(p).to_string()),
        "nested_type_identifier" => node
            .child_by_field_name("name")
            .map(|p| ctx.text(p).to_string()),
        _ => None,
    }
}

fn push_heritage(clause: Node, source_id: &str, ctx: &Ctx, out: &mut ParsedFile) {
    let mut cursor = clause.walk();
    for entry in clause.named_children(&mut cursor) {
        if let Some(target) = base_name(entry, ctx) {
            out.refs.push(RawReference {
                source_symbol_id: source_id.to_string(),
                target_name: target,
                kind: EdgeKind::Implements,
                line: line_of(entry),
            });
        }
    }
}

/// Call edges from every `f(…)`, `obj.m(…)` and `new C(…)` inside `node`, first occurrence
/// per name. Builtins and the symbol's own name are skipped.
fn collect_calls(
    node: Node,
    self_name: &str,
    source_id: &str,
    edge_kind: EdgeKind,
    ctx: &Ctx,
    out: &mut ParsedFile,
) {
    let mut seen = HashSet::new();
    let mut stack = vec![node];
    let mut found: Vec<(usize, usize, String)> = Vec::new();
    while let Some(n) = stack.pop() {
        let callee = match n.kind() {
            "call_expression" => n.child_by_field_name("function"),
            "new_expression" => n.child_by_field_name("constructor"),
            _ => None,
        };
        if let Some(c) = callee {
            let name = match c.kind() {
                "identifier" => Some(ctx.text(c)),
                "member_expression" => c.child_by_field_name("property").map(|p| ctx.text(p)),
                _ => None,
            };
            if let Some(name) = name {
                found.push((n.start_byte(), line_of(n), name.to_string()));
            }
        }
        let mut cursor = n.walk();
        for child in n.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    found.sort_by_key(|(byte, _, _)| *byte);
    for (_, line, ident) in found {
        if ident.len() >= 2
            && ident != self_name
            && !is_ts_builtin(&ident)
            && seen.insert(ident.clone())
        {
            out.refs.push(RawReference {
                source_symbol_id: source_id.to_string(),
                target_name: ident,
                kind: edge_kind,
                line,
            });
        }
    }
}

/// Relative imports anywhere in the file: `import … from "./x"`, `export … from "./x"`,
/// `require("./x")` and `import("./x")`. Package imports are not graph edges.
fn collect_imports(root: Node, ctx: &Ctx, out: &mut ParsedFile) {
    let mut stack = vec![root];
    let mut found: Vec<(usize, usize, String)> = Vec::new();
    while let Some(n) = stack.pop() {
        let spec = match n.kind() {
            "import_statement" | "export_statement" => n.child_by_field_name("source"),
            "call_expression" => {
                let is_loader = n
                    .child_by_field_name("function")
                    .is_some_and(|f| f.kind() == "import" || ctx.text(f) == "require");
                if is_loader {
                    n.child_by_field_name("arguments")
                        .and_then(|a| a.named_child(0))
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(spec) = spec.and_then(|s| string_value(s, ctx))
            && (spec.starts_with('.') || spec.starts_with('/'))
        {
            found.push((n.start_byte(), line_of(n), normalize_ts_import(&spec)));
        }
        let mut cursor = n.walk();
        for child in n.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    found.sort_by_key(|(byte, _, _)| *byte);
    for (_, line, target) in found {
        out.refs.push(RawReference {
            source_symbol_id: ctx.file_id.to_string(),
            target_name: target,
            kind: EdgeKind::Imports,
            line,
        });
    }
}

fn normalize_ts_import(spec: &str) -> String {
    spec.trim_start_matches("./")
        .trim_start_matches("../")
        .trim_start_matches('/')
        .to_string()
}

fn is_ts_builtin(name: &str) -> bool {
    matches!(
        name,
        "if" | "for"
            | "while"
            | "switch"
            | "catch"
            | "return"
            | "typeof"
            | "function"
            | "constructor"
            | "super"
            | "require"
            | "log"
            | "error"
            | "warn"
            | "push"
            | "pop"
            | "map"
            | "filter"
            | "reduce"
            | "forEach"
            | "find"
            | "includes"
            | "slice"
            | "splice"
            | "join"
            | "split"
            | "trim"
            | "keys"
            | "values"
            | "entries"
            | "parse"
            | "stringify"
            | "then"
            | "resolve"
            | "reject"
            | "expect"
            | "describe"
            | "it"
            | "test"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> ParsedFile {
        TypeScriptExtractor.parse("src/a.test.ts", src).unwrap()
    }

    fn parse_as(path: &str, src: &str) -> ParsedFile {
        TypeScriptExtractor.parse(path, src).unwrap()
    }

    fn sym<'a>(out: &'a ParsedFile, name: &str) -> &'a Symbol {
        out.symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("symbol {name} not extracted"))
    }

    fn span(out: &ParsedFile, name: &str) -> (usize, usize) {
        let s = sym(out, name);
        (s.start_line, s.end_line)
    }

    fn has_ref(out: &ParsedFile, target: &str, kind: EdgeKind) -> bool {
        out.refs
            .iter()
            .any(|r| r.target_name == target && r.kind == kind)
    }

    #[test]
    fn one_line_test_call_does_not_swallow_following_functions() {
        let src = "it(\"runs shared case\", runCase);\n\
                   \n\
                   function helperAfter() {\n  return 1;\n}\n";
        let out = parse(src);
        let test = out
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Test)
            .unwrap();
        assert_eq!((test.start_line, test.end_line), (1, 1));
        assert!(
            out.symbols.iter().any(|s| s.name == "helperAfter"),
            "function after a one-line test must still be extracted"
        );
    }

    #[test]
    fn doc_comment_above_test_belongs_to_the_test() {
        let src = "// verifies the happy path\n\
                   test(\"happy\", () => {\n  expect(run()).toBe(1);\n});\n\
                   function plain() {\n  return 2;\n}\n";
        let out = parse(src);
        let test = out
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Test)
            .unwrap();
        assert!(test.doc_summary.contains("happy path"));
        let plain = out.symbols.iter().find(|s| s.name == "plain").unwrap();
        assert!(
            plain.doc_summary.is_empty(),
            "comment leaked to the next symbol"
        );
    }

    #[test]
    fn one_line_test_with_trailing_comment_does_not_swallow() {
        let src = "it(\"fetches http://example.com\", runCase); // shared case\n\
                   function helperAfter() {\n  return 1;\n}\n";
        let out = parse(src);
        let test = out
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Test)
            .unwrap();
        assert_eq!(test.end_line, 1);
        assert!(out.symbols.iter().any(|s| s.name == "helperAfter"));
        assert!(
            sym(&out, "helperAfter").doc_summary.is_empty(),
            "a trailing comment on the previous code line is not a doc comment"
        );
    }

    #[test]
    fn allman_style_test_keeps_its_body() {
        let src = "it(\"allman\", function ()\n{\n  expect(run()).toBe(1);\n});\n";
        let out = parse(src);
        let test = out
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Test)
            .unwrap();
        assert_eq!((test.start_line, test.end_line), (1, 4));
    }

    // Replaces the old lexer unit test `code_only_view_ignores_comments_and_string_contents`:
    // the same inputs must not change where the test ends.
    #[test]
    fn comments_and_string_contents_do_not_change_spans() {
        for header in [
            "it(\"a//b\", f); // c",
            "it(\"x\", () => `//`);",
            "it('it\\'s', f); //x",
            "it(\"parses {x}\", f);",
            "it(\"x\", f); /* it's done */",
            "it(\"x\", f); /* open\n */",
            "it(\"çğü{\", f);",
        ] {
            let src = format!("{header}\nfunction helperAfter() {{\n  return 1;\n}}\n");
            let out = parse(&src);
            let t = out
                .symbols
                .iter()
                .find(|s| s.kind == SymbolKind::Test)
                .unwrap_or_else(|| panic!("no test for {header}"));
            assert_eq!(t.end_line, 1, "{header}");
            let helper_start = header.lines().count() + 1;
            assert_eq!(span(&out, "helperAfter").0, helper_start, "{header}");
        }
    }

    #[test]
    fn one_line_test_with_brace_in_name_or_block_comment_does_not_swallow() {
        for header in [
            "it(\"parses {x}\", runCase);",
            "it(\"plain\", runCase); /* it's shared */",
        ] {
            let src = format!("{header}\nfunction helperAfter() {{\n  return 1;\n}}\n");
            let out = parse(&src);
            let test = out
                .symbols
                .iter()
                .find(|s| s.kind == SymbolKind::Test)
                .unwrap();
            assert_eq!(test.end_line, 1, "test swallowed following code: {header}");
            assert!(
                out.symbols.iter().any(|s| s.name == "helperAfter"),
                "{header}"
            );
        }
    }

    #[test]
    fn unmatched_brace_in_test_name_does_not_extend_the_body() {
        let src = "it(\"parses {x\", () => {\n  run();\n});\n\
                   function helperAfter() {\n  return 1;\n}\n";
        let out = parse(src);
        let test = out
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Test)
            .unwrap();
        assert_eq!((test.start_line, test.end_line), (1, 3));
        assert!(out.symbols.iter().any(|s| s.name == "helperAfter"));
    }

    #[test]
    fn test_call_with_arguments_continuing_on_next_line_keeps_its_body() {
        // First line ends with `)` (from `makeConfig()`) but the call is still open
        let src = "it(\"multi\", withSetup(makeConfig()\n  , () => {\n    run();\n  }));\n";
        let out = parse(src);
        let test = out
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Test)
            .unwrap();
        assert_eq!((test.start_line, test.end_line), (1, 4));
    }

    fn test_span(src: &str) -> (usize, usize, bool) {
        let out = parse(src);
        let t = out
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Test)
            .unwrap();
        let helper_kept = out.symbols.iter().any(|s| s.name == "helperAfter");
        (t.start_line, t.end_line, helper_kept)
    }

    #[test]
    fn comment_between_test_header_and_allman_brace_keeps_the_body() {
        let src = "it(\"commented\", function ()\n// arrange\n{\n  run();\n});\n\
                   function helperAfter() {\n  return 1;\n}\n";
        assert_eq!(test_span(src), (1, 5, true));
    }

    #[test]
    fn expression_bodied_multi_line_test_does_not_swallow() {
        let src = "it(\"expr\", () =>\n  expect(run()).toBe(1));\n\
                   function helperAfter() {\n  return 1;\n}\n";
        assert_eq!(test_span(src), (1, 2, true));
    }

    #[test]
    fn unterminated_test_call_is_one_line_not_the_rest_of_the_file() {
        let src = "it(\"broken\", () => {\nfunction helperAfter() {\n  return 1;\n}\n";
        let (start, end, _) = test_span(src);
        assert_eq!((start, end), (1, 1));
    }

    #[test]
    fn multi_line_template_literal_does_not_end_the_test_early() {
        // The unbalanced `)` and `}` on line 3 are inside a template literal opened on line 2;
        // a line-local scanner would see that `)` as closing the it( call.
        let src = "it(\"renders\", () => {\n  const html = `<p>\n    done :) }\n  </p>`;\n  \
                   check(html);\n});\n\
                   function helperAfter() {\n  return 1;\n}\n";
        assert_eq!(test_span(src), (1, 6, true));
    }

    #[test]
    fn multi_line_block_comment_does_not_end_the_test_early() {
        let src = "it(\"commented body\", () => {\n  /* note:\n     ) } closes nothing\n  */\n  \
                   run();\n});\n\
                   function helperAfter() {\n  return 1;\n}\n";
        assert_eq!(test_span(src), (1, 6, true));
    }

    // Replaces `scanner_carries_template_and_comment_state_across_lines`: template
    // interpolations and multi-line comments around braces/parens keep spans exact.
    #[test]
    fn template_interpolation_and_comments_keep_function_spans() {
        let src = "function a() {\n  const s = `x ${f(`)`)} }\n  y`;\n  /* { ( */\n  return s;\n}\n\
                   function helperAfter() {\n  return 1;\n}\n";
        let out = parse(src);
        assert_eq!(span(&out, "a"), (1, 6));
        assert_eq!(span(&out, "helperAfter"), (7, 9));
    }

    #[test]
    fn const_arrow_with_brace_in_string_does_not_swallow() {
        let src = "const label = () => \"{\" + name;\n\
                   function helperAfter() {\n  return 1;\n}\n";
        let out = parse(src);
        assert!(out.symbols.iter().any(|s| s.name == "helperAfter"));
        assert_eq!(span(&out, "label"), (1, 1));
        assert_eq!(sym(&out, "label").signature, "const label = () => ...");
    }

    #[test]
    fn regex_literal_in_test_body_does_not_end_the_test_early() {
        // `/\)+/` and `/[(]/` contain an unbalanced `)` / `(` that are regex, not code
        let src = r#"it("matches", () => {
  expect(/\)+/.test(s)).toBe(true);
  check([/[(]/]);
});
function helperAfter() {
  return 1;
}
"#;
        assert_eq!(test_span(src), (1, 4, true));
    }

    // Replaces the lexer unit test of the same name.
    #[test]
    fn division_is_not_mistaken_for_a_regex() {
        let src = "function ratio(a, b, c) {\n  const r = (a) / b / (c);\n  return r;\n}\n\
                   function helperAfter() {\n  return 1;\n}\n";
        let out = parse(src);
        assert_eq!(span(&out, "ratio"), (1, 4));
        assert_eq!(span(&out, "helperAfter"), (5, 7));
    }

    // Replaces the lexer unit test of the same name, on real JSX.
    #[test]
    fn jsx_apostrophe_does_not_hide_the_rest_of_the_line() {
        let src = "export function Greeting({ name }) {\n  return <p>Don't {format(name)}</p>;\n}\n\
                   function helperAfter() {\n  return 1;\n}\n";
        for path in ["src/a.tsx", "src/a.jsx"] {
            let out = parse_as(path, src);
            assert_eq!(span(&out, "Greeting"), (1, 3), "{path}");
            assert_eq!(span(&out, "helperAfter"), (4, 6), "{path}");
            assert!(has_ref(&out, "format", EdgeKind::Calls), "{path}");
        }
    }

    #[test]
    fn test_signature_keeps_braces_inside_the_name() {
        let out = parse("it(\"parses {x}\", () => {\n  run();\n});\n");
        let t = out
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Test)
            .unwrap();
        assert!(t.signature.contains("parses {x}"), "{}", t.signature);
        assert!(!t.signature.contains("run()"), "{}", t.signature);
    }

    #[test]
    fn regex_after_throw_and_other_keywords_is_not_code() {
        let src = r#"it("throws", () => {
  if (bad) throw /)/;
  if (x instanceof /(/.constructor) {} else /)/.test(s);
  check();
});
function helperAfter() {
  return 1;
}
"#;
        assert_eq!(test_span(src), (1, 5, true));
    }

    // Replaces the lexer unit test of the same name, on real TSX.
    #[test]
    fn jsx_closing_tag_is_not_a_regex() {
        let src = "const View = () => (\n  <div><p>{a}</p> <b>{call(x)}</b></div>\n);\n\
                   function helperAfter() {\n  return 1;\n}\n";
        let out = parse_as("src/view.tsx", src);
        assert_eq!(span(&out, "View"), (1, 3));
        assert!(has_ref(&out, "call", EdgeKind::Calls));
        assert_eq!(span(&out, "helperAfter"), (4, 6));
    }

    #[test]
    fn unmatched_quote_does_not_drag_the_body_into_the_signature() {
        let out = parse("it(\"don't panic\" + ', () => {\n  run();\n});\n");
        if let Some(t) = out.symbols.iter().find(|s| s.kind == SymbolKind::Test) {
            assert!(!t.signature.contains("run()"), "{}", t.signature);
        }
    }

    #[test]
    fn classes_methods_heritage_and_visibility() {
        let src = "/** Stores users. */\nexport class UserStore extends Base<T> implements IStore, ns.Other {\n  \
                   private cache = new Map();\n  \
                   onChange = () => {\n    notify();\n  };\n  \
                   async load(id: string): Promise<User> {\n    return fetchUser(id);\n  }\n}\n\
                   interface IStore extends Readable {\n  load(id: string): Promise<User>;\n}\n\
                   export enum Color { Red, Green }\n\
                   type Id = string | number;\n\
                   declare function ambient(x: number): void;\n";
        let out = parse_as("src/store.ts", src);
        let class = sym(&out, "UserStore");
        assert_eq!(class.kind, SymbolKind::Class);
        assert_eq!((class.start_line, class.end_line), (2, 10));
        assert_eq!(class.visibility, "export");
        assert_eq!(class.doc_summary, "Stores users.");
        assert!(!class.signature.contains("cache"), "{}", class.signature);
        for base in ["Base", "IStore", "Other", "Readable"] {
            assert!(has_ref(&out, base, EdgeKind::Implements), "{base}");
        }
        let load = sym(&out, "load");
        assert_eq!(load.kind, SymbolKind::Method);
        assert_eq!(load.qualified_name, "UserStore::load");
        assert_eq!((load.start_line, load.end_line), (7, 9));
        assert_eq!(load.parent_id.as_deref(), Some(class.id.as_str()));
        assert_eq!(span(&out, "onChange"), (4, 6));
        assert!(
            !out.symbols.iter().any(|s| s.name == "cache"),
            "non-function field"
        );
        assert!(has_ref(&out, "fetchUser", EdgeKind::Calls));
        assert!(has_ref(&out, "notify", EdgeKind::Calls));
        assert_eq!(sym(&out, "IStore").kind, SymbolKind::Trait);
        assert_eq!(sym(&out, "Color").kind, SymbolKind::Enum);
        assert_eq!(sym(&out, "Id").kind, SymbolKind::TypeAlias);
        assert_eq!(sym(&out, "Id").signature, "type Id = string | number;");
        assert_eq!(sym(&out, "ambient").kind, SymbolKind::Function);
        assert_eq!(sym(&out, "Color").visibility, "export");
        assert_eq!(sym(&out, "Id").visibility, "private");
    }

    #[test]
    fn relative_imports_requires_and_dynamic_imports() {
        let src = "import { a } from \"./a\";\nimport React from 'react';\n\
                   export { b } from '../b';\nconst c = require(\"./c\");\n\
                   async function lazy() {\n  return import(\"./d\");\n}\n";
        let out = parse_as("src/x.js", src);
        let imports: Vec<(&str, usize)> = out
            .refs
            .iter()
            .filter(|r| r.kind == EdgeKind::Imports)
            .map(|r| (r.target_name.as_str(), r.line))
            .collect();
        assert_eq!(imports, vec![("a", 1), ("b", 3), ("c", 4), ("d", 6)]);
    }

    #[test]
    fn tests_inside_describe_and_modifiers_are_found() {
        let src = "describe(\"suite\", () => {\n  it.only(\"first\", () => {\n    runA();\n  });\n  \
                   test(`second`, async () => {\n    await runB();\n  });\n});\n";
        let out = parse(src);
        assert_eq!(span(&out, "first"), (2, 4));
        assert_eq!(span(&out, "second"), (5, 7));
        assert!(has_ref(&out, "runA", EdgeKind::Tests));
        assert!(has_ref(&out, "runB", EdgeKind::Tests));
    }

    #[test]
    fn calls_in_strings_and_comments_are_not_edges() {
        let src = "function f() {\n  // ghost()\n  const s = \"phantom()\";\n  real();\n}\n";
        let out = parse(src);
        assert!(has_ref(&out, "real", EdgeKind::Calls));
        assert!(!has_ref(&out, "ghost", EdgeKind::Calls));
        assert!(!has_ref(&out, "phantom", EdgeKind::Calls));
    }
}
