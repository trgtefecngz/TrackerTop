use crate::model::{EdgeKind, RawReference, Symbol, SymbolKind};
use crate::parser::{
    LanguageExtractor, ParsedFile, compact_signature, file_symbol_id, make_file_symbol,
    make_symbol_id,
};
use crate::tokens::count_tokens;
use anyhow::Result;
use std::collections::HashSet;

pub struct WgslExtractor;

impl LanguageExtractor for WgslExtractor {
    fn id(&self) -> &'static str {
        "wgsl"
    }

    fn display_name(&self) -> &'static str {
        "WGSL"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["wgsl"]
    }

    fn color_hex(&self) -> &'static str {
        "#a78bfa"
    }

    fn parse(&self, file_path: &str, source: &str) -> Result<ParsedFile> {
        let mut out = ParsedFile::default();
        let file_id = file_symbol_id(file_path);
        out.symbols.push(make_file_symbol(file_path, source));

        let lines: Vec<&str> = source.lines().collect();
        let stripped = strip_wgsl_comments_preserving_lines(source);
        let stripped_lines: Vec<&str> = stripped.lines().collect();

        let mut i = 0usize;
        let mut pending_attrs: Vec<String> = Vec::new();
        let mut pending_doc: String = String::new();

        while i < stripped_lines.len() {
            let raw_line = lines.get(i).copied().unwrap_or("").trim();
            let line = stripped_lines[i].trim();

            // Capture doc comment `///` or `//` right above a declaration
            if raw_line.starts_with("///") || raw_line.starts_with("//") {
                let doc = raw_line.trim_start_matches('/').trim();
                if !doc.is_empty() && pending_doc.is_empty() {
                    pending_doc = compact_signature(doc, 110);
                }
            }

            if line.is_empty() {
                if !raw_line.starts_with("//") {
                    pending_doc.clear();
                }
                i += 1;
                continue;
            }

            // Preprocessor / Bevy naga_oil `#import foo::bar`
            if let Some(rest) = line.strip_prefix("#import ") {
                let target = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_matches('"')
                    .trim_end_matches(';');
                if !target.is_empty() {
                    out.refs.push(RawReference {
                        source_symbol_id: file_id.clone(),
                        target_name: target.to_string(),
                        kind: EdgeKind::Imports,
                        line: i + 1,
                    });
                }
                i += 1;
                continue;
            }

            // Collect leading attributes like `@vertex`, `@fragment`, `@compute`, `@group(0) @binding(1)`
            let mut cursor = line;
            while cursor.starts_with('@') {
                if let Some((attr, rest)) = split_wgsl_attr(cursor) {
                    pending_attrs.push(attr.to_string());
                    cursor = rest.trim_start();
                } else {
                    break;
                }
            }
            if cursor.is_empty() {
                i += 1;
                continue;
            }

            let start_line = i + 1;

            // `fn name(...)`
            if let Some(rest) = cursor.strip_prefix("fn ") {
                let name = read_ident(rest);
                if !name.is_empty() {
                    let end_idx = find_matching_brace_line(&stripped_lines, i);
                    let end_line = end_idx + 1;
                    let block_text = lines[i..=end_idx].join("\n");
                    let stripped_block = stripped_lines[i..=end_idx].join("\n");

                    let stage = detect_shader_stage(&pending_attrs);
                    let (kind, vis) = if let Some(st) = stage {
                        (SymbolKind::ShaderEntry, st)
                    } else {
                        (SymbolKind::Function, "fn".to_string())
                    };

                    let attr_prefix = if pending_attrs.is_empty() {
                        String::new()
                    } else {
                        format!("{} ", pending_attrs.join(" "))
                    };
                    let sig_body = block_text.split('{').next().unwrap_or(cursor).trim();
                    let sig = compact_signature(&format!("{attr_prefix}{sig_body}"), 140);
                    let sym_id = make_symbol_id(file_path, &name, start_line);

                    out.symbols.push(Symbol {
                        id: sym_id.clone(),
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: name.clone(),
                        kind,
                        signature: sig,
                        start_line,
                        end_line,
                        token_count: count_tokens(&block_text),
                        visibility: vis,
                        doc_summary: std::mem::take(&mut pending_doc),
                        parent_id: Some(file_id.clone()),
                    });

                    extract_wgsl_body_refs(&stripped_block, &name, &sym_id, start_line, &mut out);
                    pending_attrs.clear();
                    i = end_idx + 1;
                    continue;
                }
            }

            // `struct Name { ... }`
            if let Some(rest) = cursor.strip_prefix("struct ") {
                let name = read_ident(rest);
                if !name.is_empty() {
                    let end_idx = find_matching_brace_line(&stripped_lines, i);
                    let end_line = end_idx + 1;
                    let block_text = lines[i..=end_idx].join("\n");
                    let stripped_block = stripped_lines[i..=end_idx].join("\n");
                    let sym_id = make_symbol_id(file_path, &name, start_line);

                    out.symbols.push(Symbol {
                        id: sym_id.clone(),
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: name.clone(),
                        kind: SymbolKind::Struct,
                        signature: compact_signature(&format!("struct {name}"), 120),
                        start_line,
                        end_line,
                        token_count: count_tokens(&block_text),
                        visibility: "struct".to_string(),
                        doc_summary: std::mem::take(&mut pending_doc),
                        parent_id: Some(file_id.clone()),
                    });

                    extract_wgsl_type_refs(&stripped_block, &name, &sym_id, start_line, &mut out);
                    pending_attrs.clear();
                    i = end_idx + 1;
                    continue;
                }
            }

            // `var<uniform|storage|...> name: Type;` or `var name: Type;`
            if cursor.starts_with("var<") || cursor.starts_with("var ") {
                let end_idx = find_semicolon_line(&stripped_lines, i);
                let stmt = lines[i..=end_idx].join(" ");
                let stripped_stmt = stripped_lines[i..=end_idx].join(" ");
                if let Some(name) = extract_wgsl_var_name(&stripped_stmt) {
                    let attr_prefix = if pending_attrs.is_empty() {
                        String::new()
                    } else {
                        format!("{} ", pending_attrs.join(" "))
                    };
                    let is_binding = pending_attrs.iter().any(|a| a.starts_with("@binding"));
                    let kind = if is_binding {
                        SymbolKind::Binding
                    } else {
                        SymbolKind::Const
                    };
                    let sym_id = make_symbol_id(file_path, &name, start_line);
                    out.symbols.push(Symbol {
                        id: sym_id.clone(),
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: name.clone(),
                        kind,
                        signature: compact_signature(&format!("{attr_prefix}{stmt}"), 140),
                        start_line,
                        end_line: end_idx + 1,
                        token_count: count_tokens(&stmt),
                        visibility: if is_binding {
                            pending_attrs.join(" ")
                        } else {
                            "var".to_string()
                        },
                        doc_summary: std::mem::take(&mut pending_doc),
                        parent_id: Some(file_id.clone()),
                    });
                    extract_wgsl_type_refs(&stripped_stmt, &name, &sym_id, start_line, &mut out);
                }
                pending_attrs.clear();
                i = end_idx + 1;
                continue;
            }

            // `const`, `override`, `alias`
            if let Some((kw, rest)) = cursor
                .strip_prefix("const ")
                .map(|r| ("const", r))
                .or_else(|| cursor.strip_prefix("override ").map(|r| ("override", r)))
                .or_else(|| cursor.strip_prefix("alias ").map(|r| ("alias", r)))
            {
                let name = read_ident(rest);
                let end_idx = find_semicolon_line(&stripped_lines, i);
                if !name.is_empty() {
                    let stmt = lines[i..=end_idx].join(" ");
                    let kind = if kw == "alias" {
                        SymbolKind::TypeAlias
                    } else {
                        SymbolKind::Const
                    };
                    let sym_id = make_symbol_id(file_path, &name, start_line);
                    out.symbols.push(Symbol {
                        id: sym_id,
                        file_path: file_path.to_string(),
                        name: name.clone(),
                        qualified_name: name,
                        kind,
                        signature: compact_signature(&stmt, 140),
                        start_line,
                        end_line: end_idx + 1,
                        token_count: count_tokens(&stmt),
                        visibility: kw.to_string(),
                        doc_summary: std::mem::take(&mut pending_doc),
                        parent_id: Some(file_id.clone()),
                    });
                }
                pending_attrs.clear();
                i = end_idx + 1;
                continue;
            }

            pending_attrs.clear();
            pending_doc.clear();
            i += 1;
        }

        Ok(out)
    }
}

fn strip_wgsl_comments_preserving_lines(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    let mut block_depth = 0usize;
    while i < bytes.len() {
        if block_depth > 0 {
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                block_depth += 1;
                i += 2;
            } else if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                block_depth -= 1;
                i += 2;
            } else {
                if bytes[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
        } else if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            block_depth = 1;
            i += 2;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn split_wgsl_attr(s: &str) -> Option<(&str, &str)> {
    if !s.starts_with('@') {
        return None;
    }
    let bytes = s.as_bytes();
    let mut i = 1;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    if i == 1 {
        return None;
    }
    if i < bytes.len() && bytes[i] == b'(' {
        let mut depth = 0usize;
        while i < bytes.len() {
            if bytes[i] == b'(' {
                depth += 1;
            } else if bytes[i] == b')' {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    i += 1;
                    break;
                }
            }
            i += 1;
        }
    }
    Some((&s[..i], &s[i..]))
}

fn detect_shader_stage(attrs: &[String]) -> Option<String> {
    for a in attrs {
        if a.starts_with("@vertex") {
            return Some("@vertex".to_string());
        }
        if a.starts_with("@fragment") {
            return Some("@fragment".to_string());
        }
        if a.starts_with("@compute") {
            return Some("@compute".to_string());
        }
    }
    None
}

fn read_ident(s: &str) -> String {
    s.trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

fn extract_wgsl_var_name(stmt: &str) -> Option<String> {
    let after_var = stmt.find("var").map(|idx| &stmt[idx + 3..])?.trim_start();
    let after_addr = if after_var.starts_with('<') {
        let close = after_var.find('>')?;
        after_var[close + 1..].trim_start()
    } else {
        after_var
    };
    let ident = read_ident(after_addr);
    if ident.is_empty() { None } else { Some(ident) }
}

fn find_matching_brace_line(lines: &[&str], start: usize) -> usize {
    let mut depth = 0i32;
    let mut seen_open = false;
    for (idx, line) in lines.iter().enumerate().skip(start) {
        for ch in line.chars() {
            if ch == '{' {
                depth += 1;
                seen_open = true;
            } else if ch == '}' {
                depth -= 1;
                if seen_open && depth <= 0 {
                    return idx;
                }
            }
        }
    }
    start
}

fn find_semicolon_line(lines: &[&str], start: usize) -> usize {
    for (idx, line) in lines.iter().enumerate().skip(start) {
        if line.contains(';') {
            return idx;
        }
    }
    start
}

fn extract_wgsl_body_refs(
    block: &str,
    self_name: &str,
    source_id: &str,
    base_line: usize,
    out: &mut ParsedFile,
) {
    let mut seen_calls = HashSet::new();
    let mut seen_types = HashSet::new();
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
            let mut j = i;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
            if j < chars.len() && chars[j] == '(' {
                if ident != self_name
                    && !is_wgsl_builtin(&ident)
                    && seen_calls.insert(ident.clone())
                {
                    out.refs.push(RawReference {
                        source_symbol_id: source_id.to_string(),
                        target_name: ident,
                        kind: EdgeKind::Calls,
                        line: base_line + line_off,
                    });
                }
            } else if ident != self_name
                && ident.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && !is_wgsl_builtin(&ident)
                && seen_types.insert(ident.clone())
            {
                out.refs.push(RawReference {
                    source_symbol_id: source_id.to_string(),
                    target_name: ident,
                    kind: EdgeKind::UsesType,
                    line: base_line + line_off,
                });
            }
        } else {
            i += 1;
        }
    }
}

fn extract_wgsl_type_refs(
    text: &str,
    self_name: &str,
    source_id: &str,
    base_line: usize,
    out: &mut ParsedFile,
) {
    let mut seen = HashSet::new();
    for token in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if token.len() > 1
            && token != self_name
            && token.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && !is_wgsl_builtin(token)
            && seen.insert(token.to_string())
        {
            out.refs.push(RawReference {
                source_symbol_id: source_id.to_string(),
                target_name: token.to_string(),
                kind: EdgeKind::UsesType,
                line: base_line,
            });
        }
    }
}

fn is_wgsl_builtin(name: &str) -> bool {
    matches!(
        name,
        "if" | "else"
            | "for"
            | "while"
            | "loop"
            | "switch"
            | "case"
            | "return"
            | "fn"
            | "struct"
            | "var"
            | "let"
            | "const"
            | "array"
            | "atomic"
            | "ptr"
            | "f32"
            | "f16"
            | "i32"
            | "u32"
            | "bool"
            | "vec2"
            | "vec3"
            | "vec4"
            | "vec2f"
            | "vec3f"
            | "vec4f"
            | "vec2u"
            | "vec3u"
            | "vec4u"
            | "vec2i"
            | "vec3i"
            | "vec4i"
            | "mat2x2"
            | "mat3x3"
            | "mat4x4"
            | "mat4x4f"
            | "mat3x3f"
            | "texture_2d"
            | "texture_3d"
            | "texture_storage_2d"
            | "sampler"
            | "textureSample"
            | "textureSampleLevel"
            | "textureLoad"
            | "textureStore"
            | "textureDimensions"
            | "dot"
            | "cross"
            | "normalize"
            | "length"
            | "distance"
            | "mix"
            | "clamp"
            | "smoothstep"
            | "step"
            | "min"
            | "max"
            | "abs"
            | "sin"
            | "cos"
            | "tan"
            | "pow"
            | "sqrt"
            | "floor"
            | "ceil"
            | "fract"
            | "reflect"
            | "refract"
            | "select"
            | "workgroupBarrier"
            | "storageBarrier"
            | "atomicAdd"
            | "atomicLoad"
            | "atomicStore"
    )
}
