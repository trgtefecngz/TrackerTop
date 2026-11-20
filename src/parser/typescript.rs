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
                    let end_idx = if trimmed.contains('{') {
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
                // `it("works", runCase);` has no body of its own: searching for the next `{`
                // would swallow the following functions/classes into this test. Judge the
                // line without a trailing `// comment`, and keep Allman-style tests whose
                // `{` opens on the next line (`it("x", function ()` + newline + `{`).
                let code = strip_ts_line_comment(trimmed).trim_end();
                let next_line_opens_body = lines[i + 1..]
                    .iter()
                    .map(|l| l.trim())
                    .find(|l| !l.is_empty())
                    .is_some_and(|l| l.starts_with('{'));
                let statement_ends_here = !code.contains('{')
                    && (code.ends_with(';') || (code.ends_with(')') && !next_line_opens_body));
                let end_idx = if statement_ends_here {
                    i
                } else {
                    find_brace_end(&lines, i)
                };
                let block = lines[i..=end_idx].join("\n");
                let sym_id = make_symbol_id(file_path, &test_name, line_no);
                out.symbols.push(Symbol {
                    id: sym_id.clone(),
                    file_path: file_path.to_string(),
                    name: test_name.clone(),
                    qualified_name: test_name.clone(),
                    kind: SymbolKind::Test,
                    signature: compact_signature(trimmed.split('{').next().unwrap_or(trimmed), 120),
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

/// Removes a trailing `// comment`, ignoring `//` inside '…', "…" and `…` string literals
/// (e.g. `it("fetches http://x", run); // note` keeps the URL).
fn strip_ts_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == b'\\' {
                    i += 1; // skip the escaped character
                } else if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' | b'`' => quote = Some(b),
                b'/' if bytes.get(i + 1) == Some(&b'/') => return &line[..i],
                _ => {}
            },
        }
        i += 1;
    }
    line
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

fn find_brace_end(lines: &[&str], start: usize) -> usize {
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
    fn line_comment_stripping_respects_string_literals() {
        assert_eq!(
            strip_ts_line_comment("it(\"a//b\", f); // c"),
            "it(\"a//b\", f); "
        );
        assert_eq!(strip_ts_line_comment("x = `//`;"), "x = `//`;");
        assert_eq!(strip_ts_line_comment("f('it\\'s'); //x"), "f('it\\'s'); ");
    }
}
