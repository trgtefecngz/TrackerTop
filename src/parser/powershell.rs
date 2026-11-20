use crate::model::{EdgeKind, RawReference, Symbol, SymbolKind};
use crate::parser::{
    LanguageExtractor, ParsedFile, compact_signature, file_symbol_id, make_file_symbol,
    make_symbol_id,
};
use crate::tokens::count_tokens;
use anyhow::Result;
use std::collections::HashSet;

pub struct PowerShellExtractor;

impl LanguageExtractor for PowerShellExtractor {
    fn id(&self) -> &'static str {
        "powershell"
    }

    fn display_name(&self) -> &'static str {
        "PowerShell"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["ps1", "psm1", "psd1"]
    }

    fn color_hex(&self) -> &'static str {
        "#22d3ee"
    }

    fn parse(&self, file_path: &str, source: &str) -> Result<ParsedFile> {
        let mut out = ParsedFile::default();
        let file_id = file_symbol_id(file_path);
        out.symbols.push(make_file_symbol(file_path, source));

        let lines: Vec<&str> = source.lines().collect();
        let mut in_block_comment = false;
        let mut i = 0usize;
        let mut pending_doc = String::new();

        while i < lines.len() {
            let raw = lines[i];
            let trimmed = raw.trim();
            let line_no = i + 1;

            if in_block_comment {
                if let Some(end_pos) = trimmed.find("#>") {
                    in_block_comment = false;
                    let before = trimmed[..end_pos].trim();
                    if pending_doc.is_empty() && !before.is_empty() && !before.starts_with('.') {
                        pending_doc = compact_signature(before, 110);
                    }
                } else if trimmed.starts_with(".SYNOPSIS")
                    && let Some(next_l) = lines.get(i + 1)
                {
                    let s = next_l.trim();
                    if !s.is_empty() && !s.starts_with('.') {
                        pending_doc = compact_signature(s, 110);
                    }
                }
                i += 1;
                continue;
            }

            if trimmed.starts_with("<#") {
                if !trimmed.contains("#>") {
                    in_block_comment = true;
                }
                i += 1;
                continue;
            }

            // `#Requires -Modules Foo, Bar`
            if let Some(rest) = strip_prefix_ascii_ci(trimmed, "#requires ") {
                if let Some(mod_idx) = rest.to_ascii_lowercase().find("-modules") {
                    let after = rest[mod_idx + 8..].trim();
                    for m in after.split(',') {
                        let clean = m.trim().trim_matches(|c| c == '\'' || c == '"');
                        if !clean.is_empty() {
                            out.refs.push(RawReference {
                                source_symbol_id: file_id.clone(),
                                target_name: clean.to_string(),
                                kind: EdgeKind::Imports,
                                line: line_no,
                            });
                        }
                    }
                }
                i += 1;
                continue;
            }

            if trimmed.starts_with('#') {
                let doc = trimmed.trim_start_matches('#').trim();
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

            // `Import-Module Foo` or `using module Foo`
            if let Some(rest) = strip_prefix_ascii_ci(trimmed, "import-module ")
                .or_else(|| strip_prefix_ascii_ci(trimmed, "using module "))
            {
                let target = extract_ps_path_arg(rest);
                if !target.is_empty() {
                    out.refs.push(RawReference {
                        source_symbol_id: file_id.clone(),
                        target_name: target,
                        kind: EdgeKind::Imports,
                        line: line_no,
                    });
                }
                i += 1;
                continue;
            }

            // Dot-sourcing: `. ./helper.ps1` or `. "$PSScriptRoot\lib.ps1"`
            if let Some(rest) = trimmed.strip_prefix(". ") {
                let target = extract_ps_path_arg(rest);
                if target.ends_with(".ps1") || target.ends_with(".psm1") {
                    out.refs.push(RawReference {
                        source_symbol_id: file_id.clone(),
                        target_name: target,
                        kind: EdgeKind::Imports,
                        line: line_no,
                    });
                }
                i += 1;
                continue;
            }

            // `function Name`, `filter Name`, `workflow Name`
            if let Some((kw, rest)) = strip_prefix_ascii_ci(trimmed, "function ")
                .map(|r| ("function", r))
                .or_else(|| strip_prefix_ascii_ci(trimmed, "filter ").map(|r| ("filter", r)))
                .or_else(|| strip_prefix_ascii_ci(trimmed, "workflow ").map(|r| ("workflow", r)))
            {
                let name = read_ps_ident(rest);
                if !name.is_empty() {
                    let end_idx = find_ps_block_end(&lines, i);
                    let block = lines[i..=end_idx].join("\n");
                    let param_sig = extract_ps_param_signature(&lines[i..=end_idx]);
                    let sig = if param_sig.is_empty() {
                        format!("{kw} {name}")
                    } else {
                        compact_signature(&format!("{kw} {name}({param_sig})"), 140)
                    };
                    let kind = if kw == "filter" {
                        SymbolKind::Filter
                    } else {
                        SymbolKind::Function
                    };
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
                        visibility: kw.to_string(),
                        doc_summary: std::mem::take(&mut pending_doc),
                        parent_id: Some(file_id.clone()),
                    });

                    collect_ps_calls(&lines[i..=end_idx], &name, &sym_id, line_no, &mut out);
                    i = end_idx + 1;
                    continue;
                }
            }

            // `class Name` or `enum Name`
            if let Some((kw, rest)) = strip_prefix_ascii_ci(trimmed, "class ")
                .map(|r| ("class", r))
                .or_else(|| strip_prefix_ascii_ci(trimmed, "enum ").map(|r| ("enum", r)))
            {
                let name = read_ps_ident(rest);
                if !name.is_empty() {
                    let end_idx = find_ps_block_end(&lines, i);
                    let block = lines[i..=end_idx].join("\n");
                    let sym_id = make_symbol_id(file_path, &name, line_no);
                    let kind = if kw == "class" {
                        SymbolKind::Class
                    } else {
                        SymbolKind::Enum
                    };
                    out.symbols.push(Symbol {
                        id: sym_id,
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: name,
                        kind,
                        signature: compact_signature(trimmed.trim_end_matches('{'), 130),
                        start_line: line_no,
                        end_line: end_idx + 1,
                        token_count: count_tokens(&block),
                        visibility: kw.to_string(),
                        doc_summary: std::mem::take(&mut pending_doc),
                        parent_id: Some(file_id.clone()),
                    });
                    i = end_idx + 1;
                    continue;
                }
            }

            pending_doc.clear();
            i += 1;
        }

        // Also capture top-level calls outside functions (scripts often run statements directly)
        collect_ps_calls(&lines, "", &file_id, 1, &mut out);

        Ok(out)
    }
}

fn strip_prefix_ascii_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

fn read_ps_ident(s: &str) -> String {
    let trimmed = s.trim_start();
    // Skip optional scope prefix like `global:` or `script:`
    let trimmed = trimmed.split_once(':').map(|(_, r)| r).unwrap_or(trimmed);
    trimmed
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

fn extract_ps_path_arg(rest: &str) -> String {
    let trimmed = rest.trim();
    // Skip `-Name` / `-Path` flag if present
    let after_flag = if trimmed.starts_with('-') {
        trimmed
            .split_once(char::is_whitespace)
            .map(|(_, r)| r.trim_start())
            .unwrap_or("")
    } else {
        trimmed
    };
    let raw = if after_flag.starts_with('"') || after_flag.starts_with('\'') {
        let q = after_flag.chars().next().unwrap();
        after_flag[1..].split(q).next().unwrap_or("")
    } else {
        after_flag.split_whitespace().next().unwrap_or("")
    };
    // Normalize `$PSScriptRoot\foo.ps1` -> `foo.ps1`
    let cleaned = raw
        .replace("$PSScriptRoot\\", "")
        .replace("$PSScriptRoot/", "")
        .replace('\\', "/");
    cleaned.trim_start_matches("./").to_string()
}

fn find_ps_block_end(lines: &[&str], start: usize) -> usize {
    let mut depth = 0i32;
    let mut seen_open = false;
    for (idx, line) in lines.iter().enumerate().skip(start) {
        let mut in_single = false;
        let mut in_double = false;
        for ch in line.chars() {
            match ch {
                '\'' if !in_double => in_single = !in_single,
                '"' if !in_single => in_double = !in_double,
                '#' if !in_single && !in_double => break,
                '{' if !in_single && !in_double => {
                    depth += 1;
                    seen_open = true;
                }
                '}' if !in_single && !in_double => {
                    depth -= 1;
                    if seen_open && depth <= 0 {
                        return idx;
                    }
                }
                _ => {}
            }
        }
    }
    start
}

fn extract_ps_param_signature(block_lines: &[&str]) -> String {
    let mut params = Vec::new();
    let mut in_param = false;
    let mut paren_depth = 0i32;

    for line in block_lines {
        let t = line.trim();
        if !in_param {
            if let Some(pos) = t.to_ascii_lowercase().find("param(") {
                in_param = true;
                let rest = &t[pos + 5..];
                for ch in rest.chars() {
                    if ch == '(' {
                        paren_depth += 1;
                    } else if ch == ')' {
                        paren_depth -= 1;
                    }
                }
                collect_dollar_vars(rest, &mut params);
                if paren_depth <= 0 {
                    break;
                }
            }
        } else {
            collect_dollar_vars(t, &mut params);
            for ch in t.chars() {
                if ch == '(' {
                    paren_depth += 1;
                } else if ch == ')' {
                    paren_depth -= 1;
                }
            }
            if paren_depth <= 0 {
                break;
            }
        }
    }
    params.join(", ")
}

fn collect_dollar_vars(line: &str, out: &mut Vec<String>) {
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            let mut name = String::from("$");
            while let Some(&nc) = chars.peek() {
                if nc.is_ascii_alphanumeric() || nc == '_' {
                    name.push(nc);
                    chars.next();
                } else {
                    break;
                }
            }
            if name.len() > 1 && !out.contains(&name) {
                out.push(name);
            }
        }
    }
}

fn collect_ps_calls(
    lines: &[&str],
    self_name: &str,
    source_id: &str,
    base_line: usize,
    out: &mut ParsedFile,
) {
    let mut seen = HashSet::new();
    for (idx, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        // Look for Verb-Noun commands or script invocations
        for tok in
            t.split(|c: char| c.is_whitespace() || matches!(c, '|' | ';' | '(' | ')' | '{' | '}'))
        {
            if tok.contains('-') && !tok.starts_with('-') && !tok.starts_with('$') {
                let clean = tok
                    .trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'));
                if clean.len() >= 4
                    && !clean.eq_ignore_ascii_case(self_name)
                    && !is_common_ps_builtin(clean)
                    && seen.insert(clean.to_string())
                {
                    out.refs.push(RawReference {
                        source_symbol_id: source_id.to_string(),
                        target_name: clean.to_string(),
                        kind: EdgeKind::Calls,
                        line: base_line + idx,
                    });
                }
            }
        }
    }
}

fn is_common_ps_builtin(cmd: &str) -> bool {
    let lower = cmd.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "write-host"
            | "write-output"
            | "write-warning"
            | "write-error"
            | "write-verbose"
            | "get-item"
            | "get-childitem"
            | "get-content"
            | "set-content"
            | "out-file"
            | "out-null"
            | "test-path"
            | "join-path"
            | "split-path"
            | "new-item"
            | "remove-item"
            | "copy-item"
            | "move-item"
            | "select-object"
            | "where-object"
            | "foreach-object"
            | "sort-object"
            | "measure-object"
            | "format-table"
            | "format-list"
            | "convertto-json"
            | "convertfrom-json"
            | "start-process"
            | "start-sleep"
            | "get-location"
            | "set-location"
            | "get-command"
            | "get-date"
            | "import-module"
    )
}
