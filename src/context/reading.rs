//! Reading tools: file_outline, read_symbol (symbols and line ranges), read_symbols_batch, and the not-indexed / not-found explanations they share.

use super::*;

impl TrackerEngine {
    /// 2. Signature-only outline of a file (saves ~85% tokens vs reading whole file).
    pub fn file_outline(&self, path: &str, max_tokens: usize) -> Result<QueryOutput> {
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;
        let norm = path.replace('\\', "/").trim_start_matches("./").to_string();

        // Support exact or suffix match
        let file = cg.files.get(&norm).or_else(|| {
            cg.files
                .values()
                .find(|f| f.path.ends_with(&norm) || f.path.contains(&norm))
        });

        let Some(file) = file else {
            return self.finalize(
                &db,
                "file_outline",
                path,
                self.path_miss(&cg, path),
                max_tokens,
                0,
                0,
                start,
            );
        };

        let mut out = format!(
            "# OUTLINE {} ({}, {} lines, {} full tok; {})\n",
            file.path,
            default_registry().display_name_for(&file.language),
            file.line_count,
            file.token_count,
            default_registry().extractor_quality(&file.language)
        );

        let mut syms: Vec<&Symbol> = cg
            .symbols_by_file
            .get(&file.path)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| cg.symbols.get(id))
                    .filter(|s| s.kind != SymbolKind::File)
                    .collect()
            })
            .unwrap_or_default();
        syms.sort_by_key(|s| s.start_line);

        // Notes, decisions and dismissed leads on this file or any of its symbols
        let mut targets: Vec<&str> = vec![file.path.as_str()];
        for s in &syms {
            targets.push(&s.name);
            targets.push(&s.qualified_name);
        }
        if let Ok(notes) = db.find_notes_for_targets(&targets)
            && !notes.is_empty()
        {
            out.push_str(&more_note(notes.len(), 6, "notes for this file"));
            for n in notes.iter().take(6) {
                out.push_str(&format!("{}\n", self.note_line(&cg, n)));
            }
        }

        for s in syms {
            let doc = if s.doc_summary.is_empty() {
                String::new()
            } else {
                format!(" // {}", s.doc_summary)
            };
            out.push_str(&format!(
                "L{}-{} [{}] {}{}\n",
                s.start_line,
                s.end_line,
                s.kind.as_str(),
                s.signature,
                doc
            ));
        }

        self.finalize(
            &db,
            "file_outline",
            &file.path,
            out,
            if max_tokens == 0 { 800 } else { max_tokens },
            file.token_count,
            1,
            start,
        )
    }

    /// 3. Surgical symbol reader: exact source lines of target symbol + optional 1-hop callee/caller signatures!
    pub fn read_symbol(
        &self,
        symbol_query: &str,
        include_deps: bool,
        max_tokens: usize,
    ) -> Result<QueryOutput> {
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;

        if let Some((path, from, to)) = parse_line_range(symbol_query) {
            let text = self.read_line_range(&cg, &path, from, to);
            return self.finalize(
                &db,
                "read_symbol",
                symbol_query,
                text,
                max_tokens,
                cg.files.get(&path).map_or(0, |f| f.token_count),
                1,
                start,
            );
        }

        let Some(sym) = cg.resolve_symbol(symbol_query) else {
            return self.finalize(
                &db,
                "read_symbol",
                symbol_query,
                Self::symbol_miss(&cg, symbol_query),
                max_tokens,
                0,
                0,
                start,
            );
        };

        let abs = self.repo_root.join(&sym.file_path);
        let content = std::fs::read_to_string(&abs).unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();
        let s_idx = sym.start_line.saturating_sub(1).min(lines.len());
        let e_idx = sym.end_line.min(lines.len());

        let mut out = format!(
            "# SYMBOL {} [{}] ({}:L{}-{})\n",
            sym.qualified_name,
            sym.kind.as_str(),
            sym.file_path,
            sym.start_line,
            sym.end_line
        );
        if let Some(c) = cg
            .resolve(symbol_query)
            .and_then(|r| r.caveat(symbol_query))
        {
            out.push_str(&format!("{c}\n"));
        }
        if let Some(f) = cg.files.get(&sym.file_path) {
            let quality = default_registry().extractor_quality(&f.language);
            if quality != "tree-sitter" {
                out.push_str(&format!("Span: {quality}\n"));
            }
        }
        if !sym.doc_summary.is_empty() {
            out.push_str(&format!("Doc: {}\n", sym.doc_summary));
        }
        if let Ok(notes) =
            db.find_notes_for_targets(&[&sym.name, &sym.qualified_name, &sym.file_path])
        {
            out.push_str(&more_note(notes.len(), 4, "memory notes"));
            for n in notes.into_iter().take(4) {
                out.push_str(&format!("{}\n", self.note_line(&cg, &n)));
            }
        }
        out.push_str("```\n");
        for (i, line) in lines[s_idx..e_idx].iter().enumerate() {
            out.push_str(&format!("{:>4} | {line}\n", sym.start_line + i));
        }
        out.push_str("```\n");

        let mut touched_files: HashSet<String> = HashSet::new();
        touched_files.insert(sym.file_path.clone());

        if include_deps {
            let (callers, callees) = cg.neighborhood(&sym.id, 1);
            if !callees.is_empty() {
                out.push_str("\n## Direct Callees / Types (Signatures Only)\n");
                out.push_str(&more_note(callees.len(), 10, "callees"));
                for (_, kind, conf, dep) in callees.into_iter().take(10) {
                    touched_files.insert(dep.file_path.clone());
                    out.push_str(&format!(
                        "- [{} {}] {} ({}:L{}) -> `{}`\n",
                        kind.as_str(),
                        conf.as_str(),
                        dep.qualified_name,
                        dep.file_path,
                        dep.start_line,
                        dep.signature
                    ));
                }
            }
            if !callers.is_empty() {
                out.push_str("\n## Direct Callers\n");
                out.push_str(&more_note(callers.len(), 10, "callers"));
                for (_, kind, conf, caller) in callers.into_iter().take(10) {
                    touched_files.insert(caller.file_path.clone());
                    out.push_str(&format!(
                        "- [{} {}] {} ({}:L{})\n",
                        kind.as_str(),
                        conf.as_str(),
                        caller.qualified_name,
                        caller.file_path,
                        caller.start_line
                    ));
                }
            }
        }

        let baseline: usize = touched_files
            .iter()
            .filter_map(|p| cg.files.get(p))
            .map(|f| f.token_count)
            .sum();

        self.finalize(
            &db,
            "read_symbol",
            symbol_query,
            out,
            if max_tokens == 0 { 1400 } else { max_tokens },
            baseline,
            touched_files.len(),
            start,
        )
    }

    /// 10. Reads multiple symbols in a single MCP round-trip strictly within `max_tokens`.
    pub fn read_symbols_batch(
        &self,
        symbol_queries: &[String],
        include_deps: bool,
        max_tokens: usize,
    ) -> Result<QueryOutput> {
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;
        let budget = if max_tokens == 0 { 2200 } else { max_tokens };

        let mut out = format!("# BATCH READ ({} requested)\n", symbol_queries.len());
        let mut touched_files = HashSet::new();

        if symbol_queries.len() > 10 {
            out.push_str(&format!(
                "NOT READ (max 10 per call, request them again): {}\n",
                symbol_queries[10..].join(", ")
            ));
        }
        for q in symbol_queries.iter().take(10) {
            let Some(sym) = cg.resolve_symbol(q) else {
                out.push_str(&format!("\n## `{q}`\n{}\n", Self::symbol_miss(&cg, q)));
                continue;
            };
            touched_files.insert(sym.file_path.clone());
            let abs = self.repo_root.join(&sym.file_path);
            let content = std::fs::read_to_string(&abs).unwrap_or_default();
            let lines: Vec<&str> = content.lines().collect();
            let s_idx = sym.start_line.saturating_sub(1).min(lines.len());
            let e_idx = sym.end_line.min(lines.len());

            out.push_str(&format!(
                "\n## {} [{}] ({}:L{}-{})\n",
                sym.qualified_name,
                sym.kind.as_str(),
                sym.file_path,
                sym.start_line,
                sym.end_line
            ));
            if let Some(c) = cg.resolve(q).and_then(|r| r.caveat(q)) {
                out.push_str(&format!("{c}\n"));
            }
            if let Ok(notes) =
                db.find_notes_for_targets(&[&sym.name, &sym.qualified_name, &sym.file_path])
            {
                out.push_str(&more_note(notes.len(), 2, "memory notes"));
                for n in notes.into_iter().take(2) {
                    out.push_str(&format!("{}\n", self.note_line(&cg, &n)));
                }
            }
            out.push_str("```\n");
            for (i, line) in lines[s_idx..e_idx].iter().enumerate() {
                out.push_str(&format!("{:>4} | {line}\n", sym.start_line + i));
            }
            out.push_str("```\n");

            if include_deps {
                let (_, callees) = cg.neighborhood(&sym.id, 1);
                out.push_str(&more_note(callees.len(), 5, "callees"));
                for (_, kind, conf, dep) in callees.into_iter().take(5) {
                    touched_files.insert(dep.file_path.clone());
                    out.push_str(&format!(
                        "- [{} {}] {} ({}:L{}) `{}`\n",
                        kind.as_str(),
                        conf.as_str(),
                        dep.qualified_name,
                        dep.file_path,
                        dep.start_line,
                        dep.signature
                    ));
                }
            }
        }

        let baseline: usize = touched_files
            .iter()
            .filter_map(|p| cg.files.get(p))
            .map(|f| f.token_count)
            .sum();

        self.finalize(
            &db,
            "read_symbols_batch",
            &symbol_queries.join(", "),
            out,
            budget,
            baseline,
            touched_files.len(),
            start,
        )
    }
}
/// `path:Lstart-end`, `path:start-end` or `path:Lline` -> (path, start, end), 1-based.
pub(super) fn parse_line_range(query: &str) -> Option<(String, usize, usize)> {
    let (path, range) = query.trim().rsplit_once(':')?;
    let range = range.strip_prefix('L').unwrap_or(range);
    let (a, b) = range.split_once('-').unwrap_or((range, range));
    let b = b.strip_prefix('L').unwrap_or(b);
    let (a, b): (usize, usize) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    if a == 0 || b < a || path.is_empty() || path.ends_with(':') {
        return None;
    }
    Some((path.trim().replace('\\', "/"), a, b))
}

/// Indexed extensions, e.g. `.rs .ts .tsx …`, for "not indexed" explanations.
pub(super) fn indexed_extensions() -> String {
    let mut exts: Vec<String> = default_registry()
        .languages()
        .into_iter()
        .flat_map(|l| l.extensions)
        .map(|e| format!(".{e}"))
        .collect();
    exts.sort();
    exts.dedup();
    exts.join(" ")
}

impl TrackerEngine {
    /// Explains a path lookup miss, separating "not indexed" (the file exists but the index
    /// cannot see it) from "not found" (no such file), so the agent knows whether to fall
    /// back to a plain read.
    pub(super) fn path_miss(&self, cg: &CodeGraph, path: &str) -> String {
        let norm = path.replace('\\', "/");
        let abs = self.repo_root.join(norm.trim_start_matches("./"));
        if abs.is_file() {
            if default_registry().extractor_for_path(&abs).is_none() {
                format!(
                    "NOT INDEXED: `{path}` exists, but its file type is not indexed (indexed: {}). \
                     Read the file directly.",
                    indexed_extensions()
                )
            } else {
                format!(
                    "NOT INDEXED: `{path}` exists and its type is supported, but it is not in the \
                     index (ignored by .gitignore/.trackertopignore, too large, or unreadable). \
                     Read the file directly."
                )
            }
        } else {
            format!(
                "NOT FOUND: no file `{path}` exists in the repository, and no indexed path ends \
                 with or contains it ({} files indexed).",
                cg.files.len()
            )
        }
    }

    /// Explains a symbol lookup miss with the index's coverage, so "no such symbol in the
    /// index" is never read as "no such symbol in the repository".
    /// Lines `from..=to` of an indexed file, numbered, with the innermost symbol around them.
    fn read_line_range(&self, cg: &CodeGraph, path: &str, from: usize, to: usize) -> String {
        let Some(rec) = cg.files.get(path) else {
            return format!(
                "NOT INDEXED: `{path}` is not in the index (missing, git-ignored, or a file \
                 type TrackerTop does not index: {}). Pass a path relative to the project \
                 root, e.g. `src/lib.rs:L10-40`.\n",
                indexed_extensions()
            );
        };
        let content = std::fs::read_to_string(self.repo_root.join(path)).unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        if from > total {
            return format!("OUT OF RANGE: {path} has {total} lines; asked for L{from}-{to}.\n");
        }
        let end = to.min(total);
        let mut out = format!("# LINES {path}:L{from}-{end}");
        let around = cg
            .symbols_by_file
            .get(&rec.path)
            .into_iter()
            .flatten()
            .filter_map(|id| cg.symbols.get(id))
            .filter(|s| s.kind != SymbolKind::File && s.start_line <= from && s.end_line >= end)
            .min_by_key(|s| s.end_line - s.start_line);
        if let Some(s) = around {
            out.push_str(&format!(
                " (inside {} [{}] L{}-{})",
                s.qualified_name,
                s.kind.as_str(),
                s.start_line,
                s.end_line
            ));
        }
        out.push('\n');
        if end < to {
            out.push_str(&format!("(clamped: the file has {total} lines)\n"));
        }
        out.push_str("```\n");
        for (i, line) in lines[from - 1..end].iter().enumerate() {
            out.push_str(&format!("{:>4} | {line}\n", from + i));
        }
        out.push_str("```\n");
        out
    }

    pub(super) fn symbol_miss(cg: &CodeGraph, query: &str) -> String {
        format!(
            "NOT FOUND IN INDEX: no symbol matches `{query}` ({} symbols in {} indexed files; \
             indexed types: {}). Code in other file types, generated or ignored files is not \
             indexed: use `find_symbol` for fuzzy matches or `smart_grep` before concluding it \
             does not exist.",
            cg.symbols.len(),
            cg.files.len(),
            indexed_extensions()
        )
    }
}
