use crate::model::{EdgeKind, RawReference, Symbol, SymbolKind};
use crate::parser::{
    LanguageExtractor, ParsedFile, compact_signature, file_symbol_id, make_file_symbol,
    make_symbol_id,
};
use crate::tokens::count_tokens;
use anyhow::Result;
use std::collections::HashSet;

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

    fn color_hex(&self) -> &'static str {
        "#60a5fa"
    }

    fn parse(&self, file_path: &str, source: &str) -> Result<ParsedFile> {
        let mut out = ParsedFile::default();
        let file_id = file_symbol_id(file_path);
        out.symbols.push(make_file_symbol(file_path, source));

        let lines: Vec<&str> = source.lines().collect();
        let mut in_block_comment = false;
        let mut pending_doc = String::new();
        let mut i = 0usize;

        while i < lines.len() {
            let trimmed = lines[i].trim();
            let line_no = i + 1;

            if in_block_comment {
                if trimmed.contains("*/") {
                    in_block_comment = false;
                } else if pending_doc.is_empty() {
                    let clean = trimmed.trim_start_matches('*').trim();
                    if !clean.is_empty() && !clean.starts_with('@') {
                        pending_doc = compact_signature(clean, 110);
                    }
                }
                i += 1;
                continue;
            }
            if trimmed.starts_with("/*") {
                if !trimmed.contains("*/") {
                    in_block_comment = true;
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

            // ES import / re-export: `import ... from "./foo"` or `import "./foo"`
            if trimmed.starts_with("import ")
                || (trimmed.starts_with("export ") && trimmed.contains(" from "))
            {
                if let Some(spec) = extract_quoted_path(trimmed)
                    && (spec.starts_with('.') || spec.starts_with('/'))
                {
                    out.refs.push(RawReference {
                        source_symbol_id: file_id.clone(),
                        target_name: normalize_ts_import(&spec),
                        kind: EdgeKind::Imports,
                        line: line_no,
                    });
                }
                i += 1;
                continue;
            }

            // CommonJS require("./foo")
            if let Some(req_pos) = trimmed.find("require(")
                && let Some(spec) = extract_quoted_path(&trimmed[req_pos..])
                && (spec.starts_with('.') || spec.starts_with('/'))
            {
                out.refs.push(RawReference {
                    source_symbol_id: file_id.clone(),
                    target_name: normalize_ts_import(&spec),
                    kind: EdgeKind::Imports,
                    line: line_no,
                });
            }

            let (vis, rest) = strip_ts_modifiers(trimmed);

            // `function name(...)`
            if let Some(after) = rest.strip_prefix("function ") {
                let after = after.trim_start_matches('*').trim_start();
                let name = read_ts_ident(after);
                if !name.is_empty() {
                    let end_idx = find_brace_end(&lines, i);
                    let block = lines[i..=end_idx].join("\n");
                    let sig = compact_signature(trimmed.split('{').next().unwrap_or(trimmed), 140);
                    let sym_id = make_symbol_id(file_path, &name, line_no);
                    out.symbols.push(Symbol {
                        id: sym_id.clone(),
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: name.clone(),
                        kind: SymbolKind::Function,
                        signature: sig,
                        start_line: line_no,
                        end_line: end_idx + 1,
                        token_count: count_tokens(&block),
                        visibility: vis,
                        doc_summary: std::mem::take(&mut pending_doc),
                        parent_id: Some(file_id.clone()),
                    });
                    collect_ts_calls(&block, &name, &sym_id, line_no, EdgeKind::Calls, &mut out);
                    i = end_idx + 1;
                    continue;
                }
            }

            // `class Name`, `interface Name`, `enum Name`, `type Name = ...`
            if let Some((kind, after)) = rest
                .strip_prefix("class ")
                .map(|r| (SymbolKind::Class, r))
                .or_else(|| {
                    rest.strip_prefix("interface ")
                        .map(|r| (SymbolKind::Trait, r))
                })
                .or_else(|| rest.strip_prefix("enum ").map(|r| (SymbolKind::Enum, r)))
                .or_else(|| {
                    rest.strip_prefix("type ")
                        .map(|r| (SymbolKind::TypeAlias, r))
                })
            {
                let name = read_ts_ident(after);
                if !name.is_empty() {
                    let end_idx = if kind == SymbolKind::TypeAlias && !trimmed.contains('{') {
                        i
                    } else {
                        find_brace_end(&lines, i)
                    };
                    let block = lines[i..=end_idx].join("\n");
                    let sig = compact_signature(trimmed.split('{').next().unwrap_or(trimmed), 140);
                    let sym_id = make_symbol_id(file_path, &name, line_no);
                    out.symbols.push(Symbol {
                        id: sym_id.clone(),
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: name.clone(),
                        kind,
                        signature: sig,
                        start_line: line_no,
                        end_line: end_idx + 1,
                        token_count: count_tokens(&block),
                        visibility: vis,
                        doc_summary: std::mem::take(&mut pending_doc),
                        parent_id: Some(file_id.clone()),
                    });

                    // Detect `extends Base` / `implements Iface`
                    for kw in ["extends ", "implements "] {
                        if let Some(pos) = trimmed.find(kw) {
                            let target = read_ts_ident(&trimmed[pos + kw.len()..]);
                            if !target.is_empty() {
                                out.refs.push(RawReference {
                                    source_symbol_id: sym_id.clone(),
                                    target_name: target,
                                    kind: EdgeKind::Implements,
                                    line: line_no,
                                });
                            }
                        }
                    }

                    if kind == SymbolKind::Class {
                        extract_class_methods(
                            &lines,
                            i + 1,
                            end_idx,
                            file_path,
                            &name,
                            &sym_id,
                            &mut out,
                        );
                    }
                    i = end_idx + 1;
                    continue;
                }
            }

            // `const foo = (...) => { ... }` or `const foo = async (...) => {`
            if let Some(after) = rest
                .strip_prefix("const ")
                .or_else(|| rest.strip_prefix("let "))
                && trimmed.contains("=>")
            {
                let name = read_ts_ident(after);
                if !name.is_empty() {
                    // Judge the body by code only: a `{` inside a string on this line must
                    // not start a brace search that swallows the following code.
                    let end_idx = if ts_code_only(trimmed).contains('{') {
                        find_brace_end(&lines, i)
                    } else {
                        i
                    };
                    let block = lines[i..=end_idx].join("\n");
                    let sig = compact_signature(trimmed.split("=>").next().unwrap_or(trimmed), 140);
                    let sym_id = make_symbol_id(file_path, &name, line_no);
                    out.symbols.push(Symbol {
                        id: sym_id.clone(),
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: name.clone(),
                        kind: SymbolKind::Function,
                        signature: format!("{sig} => ..."),
                        start_line: line_no,
                        end_line: end_idx + 1,
                        token_count: count_tokens(&block),
                        visibility: vis,
                        doc_summary: std::mem::take(&mut pending_doc),
                        parent_id: Some(file_id.clone()),
                    });
                    collect_ts_calls(&block, &name, &sym_id, line_no, EdgeKind::Calls, &mut out);
                    i = end_idx + 1;
                    continue;
                }
            }

            // Test blocks: `test("...", () => {` / `it("...", () => {`
            if (trimmed.starts_with("test(") || trimmed.starts_with("it("))
                && let Some(test_name) = extract_quoted_path(trimmed)
            {
                // A test is exactly the `it(` / `test(` call: it ends where that call's
                // parenthesis closes. This covers one-line calls (`it("x", runCase);`),
                // block bodies, Allman braces, comments before the `{`, argument lists that
                // continue on later lines and expression-bodied arrows, without guessing
                // from braces (which made the test swallow the following code).
                let end_idx = find_call_end(&lines, i);
                let block = lines[i..=end_idx].join("\n");
                let sym_id = make_symbol_id(file_path, &test_name, line_no);
                out.symbols.push(Symbol {
                    id: sym_id.clone(),
                    file_path: file_path.to_string(),
                    name: test_name.clone(),
                    qualified_name: test_name.clone(),
                    kind: SymbolKind::Test,
                    signature: compact_signature(before_code_brace(trimmed), 120),
                    start_line: line_no,
                    end_line: end_idx + 1,
                    token_count: count_tokens(&block),
                    visibility: "test".to_string(),
                    // Consume the doc comment so it is not attached to the next symbol
                    doc_summary: std::mem::take(&mut pending_doc),
                    parent_id: Some(file_id.clone()),
                });
                collect_ts_calls(
                    &block,
                    &test_name,
                    &sym_id,
                    line_no,
                    EdgeKind::Tests,
                    &mut out,
                );
                i = end_idx + 1;
                continue;
            }

            pending_doc.clear();
            i += 1;
        }

        Ok(out)
    }
}

fn extract_class_methods(
    lines: &[&str],
    start: usize,
    end: usize,
    file_path: &str,
    class_name: &str,
    class_id: &str,
    out: &mut ParsedFile,
) {
    let mut i = start;
    while i < end {
        let t = lines[i].trim();
        let (_, rest) = strip_ts_modifiers(t);
        if let Some(paren) = rest.find('(') {
            let head = rest[..paren].trim();
            let name = read_ts_ident(head);
            if !name.is_empty()
                && !matches!(
                    name.as_str(),
                    "if" | "for" | "while" | "switch" | "catch" | "return" | "new"
                )
                && t.contains('{')
            {
                let m_end = find_brace_end(lines, i).min(end);
                let block = lines[i..=m_end].join("\n");
                let qname = format!("{class_name}::{name}");
                let sym_id = make_symbol_id(file_path, &qname, i + 1);
                out.symbols.push(Symbol {
                    id: sym_id.clone(),
                    file_path: file_path.to_string(),
                    name: name.clone(),
                    qualified_name: qname,
                    kind: SymbolKind::Method,
                    signature: compact_signature(t.split('{').next().unwrap_or(t), 130),
                    start_line: i + 1,
                    end_line: m_end + 1,
                    token_count: count_tokens(&block),
                    visibility: "method".to_string(),
                    doc_summary: String::new(),
                    parent_id: Some(class_id.to_string()),
                });
                collect_ts_calls(&block, &name, &sym_id, i + 1, EdgeKind::Calls, out);
                i = m_end + 1;
                continue;
            }
        }
        i += 1;
    }
}

/// The structural "code only" view of one line, used to judge where a statement ends:
/// - `// …` and `/* … */` comments are removed (comment text, including quotes inside it,
///   cannot affect the result);
/// - the contents of '…', "…" and `…` string literals are dropped (the quotes are kept), so
///   braces, `//` or `;` inside strings (`it("parses {x}", run);`) are not seen as code.
///
/// Single-line convenience wrapper around [`ts_code_line`] (no state carried in or out).
fn ts_code_only(line: &str) -> String {
    let mut carry = ScanCarry::Code;
    ts_code_line(line, &mut carry)
}

/// Lexer state that survives a line break: only template literals and block comments can
/// span lines ('…' and "…" cannot).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ScanCarry {
    #[default]
    Code,
    BlockComment,
    Template,
}

/// Code-only view of one line, given the state left by the previous line (`carry`), which is
/// updated for the next line. A `)` or `{` inside a template literal or block comment that
/// started on an earlier line is therefore never mistaken for code.
fn ts_code_line(line: &str, carry: &mut ScanCarry) -> String {
    let bytes = line.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;

    // Finish whatever construct the previous line left open
    match *carry {
        ScanCarry::Code => {}
        ScanCarry::BlockComment => match line.find("*/") {
            Some(p) => {
                i = p + 2;
                *carry = ScanCarry::Code;
            }
            None => return String::new(),
        },
        ScanCarry::Template => match find_unescaped(bytes, 0, b'`') {
            Some(p) => {
                out.push(b'`');
                i = p + 1;
                *carry = ScanCarry::Code;
            }
            None => return String::new(),
        },
    }

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'/' if bytes.get(i + 1) == Some(&b'/') => break,
            b'/' if bytes.get(i + 1) == Some(&b'*') => match line[i + 2..].find("*/") {
                Some(p) => {
                    i += 2 + p + 2;
                    continue;
                }
                None => {
                    *carry = ScanCarry::BlockComment;
                    break;
                }
            },
            b'/' if regex_can_start(&out) => match find_regex_end(bytes, i + 1) {
                // Regex literal: its contents (`/\)/`, `/[{(]/`) are not code
                Some(close) => {
                    out.extend_from_slice(b"//");
                    i = close;
                }
                // No closing slash on this line: it was a division after all
                None => out.push(b),
            },
            b'"' | b'\'' | b'`' => match find_unescaped(bytes, i + 1, b) {
                Some(close) => {
                    out.push(b);
                    out.push(b);
                    i = close;
                }
                None if b == b'`' => {
                    // Template literal continues on the next line
                    out.push(b);
                    *carry = ScanCarry::Template;
                    break;
                }
                // '…' and "…" cannot span lines in JS/TS, so an unmatched one is plain text
                // (e.g. the apostrophe in JSX text `<p>Don't {x}</p>`): keep scanning.
                None => {}
            },
            _ => out.push(b),
        }
        i += 1;
    }
    // Only ASCII bytes are dropped (whole multi-byte sequences are either copied or skipped),
    // so this is valid UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

/// The part of `line` before its first `{` that is code, not inside a '…', "…" or `…`
/// string (so `it("parses {x}", () => {` keeps the full test name in its signature).
fn before_code_brace(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => return &line[..i],
            q @ (b'"' | b'\'' | b'`') => {
                // An unmatched quote is plain text (same rule as the code-only scanner), so it
                // cannot make the signature run on through the test body.
                if let Some(close) = find_unescaped(bytes, i + 1, q) {
                    i = close;
                }
            }
            _ => {}
        }
        i += 1;
    }
    line
}

/// Whether a `/` at this point starts a regex literal rather than a division, judged by the
/// previous significant code character (the usual lexer rule): after an operator, an opening
/// bracket, a separator, or a keyword like `return`, an operand is expected, so `/` opens a
/// regex; after an identifier, number or `)` / `]`, it divides.
fn regex_can_start(code_so_far: &[u8]) -> bool {
    // `</` right after `<` is a JSX/TSX closing tag (`</p>`), not a regex; treating it as one
    // would hide the code up to the next `/`.
    if code_so_far.last() == Some(&b'<') {
        return false;
    }
    let trimmed = code_so_far.trim_ascii_end();
    let Some(&last) = trimmed.last() else {
        return true;
    };
    if b"(,=:[!&|?{};+-*%<>~^".contains(&last) {
        return true;
    }
    // Keywords after which an expression (operand) follows
    let word_start = trimmed
        .iter()
        .rposition(|c| !(c.is_ascii_alphanumeric() || *c == b'_' || *c == b'$'))
        .map_or(0, |p| p + 1);
    matches!(
        &trimmed[word_start..],
        b"return"
            | b"throw"
            | b"typeof"
            | b"instanceof"
            | b"case"
            | b"in"
            | b"of"
            | b"delete"
            | b"void"
            | b"yield"
            | b"await"
            | b"else"
            | b"do"
    )
}

/// End (index of the closing `/`) of a regex literal whose body starts at `from`, honoring
/// escapes and `[...]` character classes (where `/` does not close the regex).
fn find_regex_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    let mut in_class = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'[' => in_class = true,
            b']' => in_class = false,
            b'/' if !in_class => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Position of the next `quote` at or after `from` that is not escaped with a backslash.
fn find_unescaped(bytes: &[u8], from: usize, quote: u8) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == quote => return Some(i),
            _ => i += 1,
        }
    }
    None
}

fn strip_ts_modifiers(mut s: &str) -> (String, &str) {
    let mut vis = "private".to_string();
    loop {
        if let Some(r) = s.strip_prefix("export ") {
            vis = "export".to_string();
            s = r.trim_start();
        } else if let Some(r) = s.strip_prefix("default ") {
            s = r.trim_start();
        } else if let Some(r) = s.strip_prefix("declare ") {
            s = r.trim_start();
        } else if let Some(r) = s.strip_prefix("async ") {
            s = r.trim_start();
        } else if let Some(r) = s.strip_prefix("abstract ") {
            s = r.trim_start();
        } else if let Some(r) = s.strip_prefix("public ") {
            vis = "public".to_string();
            s = r.trim_start();
        } else if let Some(r) = s.strip_prefix("private ") {
            vis = "private".to_string();
            s = r.trim_start();
        } else if let Some(r) = s.strip_prefix("protected ") {
            vis = "protected".to_string();
            s = r.trim_start();
        } else if let Some(r) = s.strip_prefix("static ") {
            s = r.trim_start();
        } else {
            break;
        }
    }
    (vis, s)
}

fn read_ts_ident(s: &str) -> String {
    s.trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
        .collect()
}

fn extract_quoted_path(s: &str) -> Option<String> {
    for q in ['"', '\'', '`'] {
        if let Some(start) = s.find(q) {
            let rest = &s[start + 1..];
            if let Some(end) = rest.find(q) {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

fn normalize_ts_import(spec: &str) -> String {
    spec.trim_start_matches("./")
        .trim_start_matches("../")
        .trim_start_matches('/')
        .to_string()
}

/// Index of the line where the call whose first `(` is on line `start` closes, scanning the
/// code-only view (parentheses inside strings and comments, including template literals and
/// block comments spanning lines, are ignored). If the call never closes (malformed or
/// truncated source), the call is treated as one line rather than swallowing the rest of
/// the file.
fn find_call_end(lines: &[&str], start: usize) -> usize {
    let mut depth = 0i32;
    let mut opened = false;
    let mut carry = ScanCarry::Code;
    for (idx, line) in lines.iter().enumerate().skip(start) {
        for ch in ts_code_line(line, &mut carry).chars() {
            match ch {
                '(' => {
                    depth += 1;
                    opened = true;
                }
                ')' => {
                    depth -= 1;
                    if opened && depth <= 0 {
                        return idx;
                    }
                }
                _ => {}
            }
        }
    }
    start
}

/// Index of the line that closes the first `{` opened at or after `start`. Braces inside
/// string literals and comments are ignored (`it("parses {x", () => {` must not count the
/// one in the name), using the same multi-line-aware code-only view as [`find_call_end`].
fn find_brace_end(lines: &[&str], start: usize) -> usize {
    let mut depth = 0i32;
    let mut seen = false;
    let mut carry = ScanCarry::Code;
    for (idx, line) in lines.iter().enumerate().skip(start) {
        for ch in ts_code_line(line, &mut carry).chars() {
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

fn collect_ts_calls(
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
        if chars[i].is_ascii_alphabetic() || chars[i] == '_' || chars[i] == '$' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                i += 1;
            }
            let ident: String = chars[start..i].iter().collect();
            if i < chars.len()
                && chars[i] == '('
                && ident.len() >= 2
                && ident != self_name
                && !is_ts_builtin(&ident)
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

    #[test]
    fn code_only_view_ignores_comments_and_string_contents() {
        assert_eq!(ts_code_only("it(\"a//b\", f); // c"), "it(\"\", f); ");
        assert_eq!(ts_code_only("x = `//`;"), "x = ``;");
        assert_eq!(ts_code_only("f('it\\'s'); //x"), "f(''); ");
        assert_eq!(ts_code_only("it(\"parses {x}\", f);"), "it(\"\", f);");
        assert_eq!(
            ts_code_only("it(\"x\", f); /* it's done */"),
            "it(\"\", f); "
        );
        assert_eq!(ts_code_only("a /* c */ b"), "a  b");
        assert_eq!(ts_code_only("it(\"x\", f); /* open"), "it(\"\", f); ");
        assert_eq!(ts_code_only("s = \"çğü{\"; t"), "s = \"\"; t");
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

    #[test]
    fn scanner_carries_template_and_comment_state_across_lines() {
        let mut carry = ScanCarry::Code;
        assert_eq!(ts_code_line("a(`x", &mut carry), "a(`");
        assert_eq!(carry, ScanCarry::Template);
        assert_eq!(ts_code_line(") } still text", &mut carry), "");
        assert_eq!(ts_code_line("end` )", &mut carry), "` )");
        assert_eq!(carry, ScanCarry::Code);
        assert_eq!(ts_code_line("b /* open", &mut carry), "b ");
        assert_eq!(carry, ScanCarry::BlockComment);
        assert_eq!(ts_code_line("{ ) */ c", &mut carry), " c");
        assert_eq!(carry, ScanCarry::Code);
    }

    #[test]
    fn const_arrow_with_brace_in_string_does_not_swallow() {
        let src = "const label = () => \"{\" + name;\n\
                   function helperAfter() {\n  return 1;\n}\n";
        let out = parse(src);
        assert!(out.symbols.iter().any(|s| s.name == "helperAfter"));
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

    #[test]
    fn division_is_not_mistaken_for_a_regex() {
        assert_eq!(
            ts_code_only("const r = (a) / b / (c);"),
            "const r = (a) / b / (c);"
        );
        assert_eq!(ts_code_only("x = /[)}]/g; y()"), "x = //g; y()");
        assert_eq!(ts_code_only(r"return /\)/.test(s)"), "return //.test(s)");
    }

    #[test]
    fn jsx_apostrophe_does_not_hide_the_rest_of_the_line() {
        assert_eq!(ts_code_only("<p>Don't {name}</p>"), "<p>Dont {name}</p>");
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
    }

    #[test]
    fn regex_after_throw_and_other_keywords_is_not_code() {
        assert_eq!(ts_code_only("throw /)/;"), "throw //;");
        assert_eq!(
            ts_code_only("x instanceof /(/.constructor"),
            "x instanceof //.constructor"
        );
        assert_eq!(ts_code_only("else /)/.test(s)"), "else //.test(s)");

        let src = r#"it("throws", () => {
  if (bad) throw /)/;
  check();
});
function helperAfter() {
  return 1;
}
"#;
        assert_eq!(test_span(src), (1, 4, true));
    }

    #[test]
    fn jsx_closing_tag_is_not_a_regex() {
        // `</p>` must not be read as a regex running to the next `/`, hiding `{name}` and `(`
        assert_eq!(
            ts_code_only("<p>{a}</p> <b>{call(x)}</b>"),
            "<p>{a}</p> <b>{call(x)}</b>"
        );
    }

    #[test]
    fn unmatched_quote_does_not_drag_the_body_into_the_signature() {
        let out = parse("it(\"don't panic\" + ', () => {\n  run();\n});\n");
        if let Some(t) = out.symbols.iter().find(|s| s.kind == SymbolKind::Test) {
            assert!(!t.signature.contains("run()"), "{}", t.signature);
        }
        assert_eq!(
            before_code_brace("it(\"x\" + ', () => { run(); }"),
            "it(\"x\" + ', () => "
        );
    }
}
