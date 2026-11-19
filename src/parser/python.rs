use crate::model::{EdgeKind, RawReference, Symbol, SymbolKind};
use crate::parser::{
    LanguageExtractor, ParsedFile, compact_signature, file_symbol_id, make_file_symbol,
    make_symbol_id,
};
use crate::tokens::count_tokens;
use anyhow::Result;
use std::collections::HashSet;

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
        let mut out = ParsedFile::default();
        let file_id = file_symbol_id(file_path);
        out.symbols.push(make_file_symbol(file_path, source));

        let lines: Vec<&str> = source.lines().collect();
        let mut class_stack: Vec<(usize, String, String)> = Vec::new(); // (indent, class_name, class_id)
        let mut i = 0usize;

        while i < lines.len() {
            let raw = lines[i];
            let trimmed = raw.trim();
            let line_no = i + 1;

            if trimmed.is_empty() || trimmed.starts_with('#') {
                i += 1;
                continue;
            }

            let indent = leading_spaces(raw);
            while let Some(&(c_ind, _, _)) = class_stack.last() {
                if indent <= c_ind {
                    class_stack.pop();
                } else {
                    break;
                }
            }

            // `from pkg.mod import A, B` or `import pkg.mod`
            if let Some(rest) = trimmed.strip_prefix("from ") {
                if let Some((module_part, imports_part)) = rest.split_once(" import ") {
                    let mod_clean = module_part.trim().trim_start_matches('.').replace('.', "/");
                    if !mod_clean.is_empty() {
                        out.refs.push(RawReference {
                            source_symbol_id: file_id.clone(),
                            target_name: format!("{mod_clean}.py"),
                            kind: EdgeKind::Imports,
                            line: line_no,
                        });
                    }
                    for item in imports_part.split(',') {
                        let name = item.split(" as ").next().unwrap_or(item).trim().trim_matches(|c| c == '(' || c == ')');
                        if !name.is_empty() && name != "*" {
                            out.refs.push(RawReference {
                                source_symbol_id: file_id.clone(),
                                target_name: name.to_string(),
                                kind: EdgeKind::References,
                                line: line_no,
                            });
                        }
                    }
                }
                i += 1;
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("import ") {
                for item in rest.split(',') {
                    let mod_name = item.split(" as ").next().unwrap_or(item).trim().replace('.', "/");
                    if !mod_name.is_empty() {
                        out.refs.push(RawReference {
                            source_symbol_id: file_id.clone(),
                            target_name: format!("{mod_name}.py"),
                            kind: EdgeKind::Imports,
                            line: line_no,
                        });
                    }
                }
                i += 1;
                continue;
            }

            // `class Name(Base):`
            if let Some(rest) = trimmed.strip_prefix("class ") {
                let name = read_py_ident(rest);
                if !name.is_empty() {
                    let end_idx = find_py_block_end(&lines, i, indent);
                    let block = lines[i..=end_idx].join("\n");
                    let doc = extract_py_docstring(&lines, i + 1, end_idx);
                    let sym_id = make_symbol_id(file_path, &name, line_no);

                    out.symbols.push(Symbol {
                        id: sym_id.clone(),
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: name.clone(),
                        kind: SymbolKind::Class,
                        signature: compact_signature(trimmed.trim_end_matches(':'), 140),
                        start_line: line_no,
                        end_line: end_idx + 1,
                        token_count: count_tokens(&block),
                        visibility: if name.starts_with('_') { "private" } else { "pub" }.to_string(),
                        doc_summary: doc,
                        parent_id: Some(file_id.clone()),
                    });

                    // Base classes
                    if let (Some(open), Some(close)) = (rest.find('('), rest.find(')')) {
                        if close > open {
                            for base in rest[open + 1..close].split(',') {
                                let bname = read_py_ident(base.trim().rsplit('.').next().unwrap_or(base));
                                if !bname.is_empty() && !matches!(bname.as_str(), "object" | "ABC" | "Exception") {
                                    out.refs.push(RawReference {
                                        source_symbol_id: sym_id.clone(),
                                        target_name: bname,
                                        kind: EdgeKind::Implements,
                                        line: line_no,
                                    });
                                }
                            }
                        }
                    }

                    class_stack.push((indent, name, sym_id));
                    i += 1;
                    continue;
                }
            }

            // `def foo(...)` or `async def foo(...)`
            let def_rest = trimmed
                .strip_prefix("async def ")
                .or_else(|| trimmed.strip_prefix("def "));
            if let Some(rest) = def_rest {
                let name = read_py_ident(rest);
                if !name.is_empty() {
                    let end_idx = find_py_block_end(&lines, i, indent);
                    let block = lines[i..=end_idx].join("\n");
                    let doc = extract_py_docstring(&lines, i + 1, end_idx);
                    let is_test = name.starts_with("test_");
                    let (kind, qname, parent_id) = if is_test {
                        (SymbolKind::Test, name.clone(), file_id.clone())
                    } else if let Some((_, cname, cid)) = class_stack.last() {
                        (SymbolKind::Method, format!("{cname}::{name}"), cid.clone())
                    } else {
                        (SymbolKind::Function, name.clone(), file_id.clone())
                    };
                    let sym_id = make_symbol_id(file_path, &qname, line_no);

                    out.symbols.push(Symbol {
                        id: sym_id.clone(),
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: qname,
                        kind,
                        signature: compact_signature(trimmed.trim_end_matches(':'), 140),
                        start_line: line_no,
                        end_line: end_idx + 1,
                        token_count: count_tokens(&block),
                        visibility: if name.starts_with('_') { "private" } else { "pub" }.to_string(),
                        doc_summary: doc,
                        parent_id: Some(parent_id),
                    });

                    let edge_kind = if is_test { EdgeKind::Tests } else { EdgeKind::Calls };
                    collect_py_calls(&block, &name, &sym_id, line_no, edge_kind, &mut out);
                    i = end_idx + 1;
                    continue;
                }
            }

            i += 1;
        }

        Ok(out)
    }
}

fn leading_spaces(line: &str) -> usize {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

fn read_py_ident(s: &str) -> String {
    s.trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

fn find_py_block_end(lines: &[&str], start: usize, base_indent: usize) -> usize {
    let mut last_non_empty = start;
    for (idx, line) in lines.iter().enumerate().skip(start + 1) {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if leading_spaces(line) <= base_indent {
            break;
        }
        last_non_empty = idx;
    }
    last_non_empty
}

fn extract_py_docstring(lines: &[&str], start: usize, end: usize) -> String {
    if start > end || start >= lines.len() {
        return String::new();
    }
    let first = lines[start].trim();
    for quote in ["\"\"\"", "'''"] {
        if let Some(rest) = first.strip_prefix(quote) {
            let clean = rest.trim_end_matches(quote).trim();
            if !clean.is_empty() {
                return compact_signature(clean, 110);
            }
            if let Some(next) = lines.get(start + 1) {
                return compact_signature(next.trim().trim_end_matches(quote), 110);
            }
        }
    }
    String::new()
}

fn collect_py_calls(
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
                && !is_py_builtin(&ident)
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

fn is_py_builtin(name: &str) -> bool {
    matches!(
        name,
        "def" | "class"
            | "if"
            | "elif"
            | "for"
            | "while"
            | "return"
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
