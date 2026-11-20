//! Search tools: repo_map, find_symbol and smart_grep (with the plain grep scan it renders).

use super::reading::indexed_extensions;
use super::*;

impl TrackerEngine {
    /// 1. Compact architectural map of the repository: languages, communities, god nodes, and key files.
    pub fn repo_map(&self, max_tokens: usize) -> Result<QueryOutput> {
        self.repo_map_inner(max_tokens, true)
    }

    /// Read-only dry-run preview of `repo_map` for UI verification — NEVER pollutes real telemetry stats!
    pub fn preview_repo_map(&self, max_tokens: usize) -> Result<QueryOutput> {
        self.repo_map_inner(max_tokens, false)
    }

    fn repo_map_inner(&self, max_tokens: usize, record_telemetry: bool) -> Result<QueryOutput> {
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;

        let total_files = cg.files.len();
        let total_syms = cg
            .symbols
            .values()
            .filter(|s| s.kind != SymbolKind::File && s.kind != SymbolKind::Impl)
            .count();
        let total_edges = cg.edges.len();
        let total_repo_tokens: usize = cg.files.values().map(|f| f.token_count).sum();

        // Language breakdown (driven dynamically by LanguageRegistry)
        let reg = default_registry();
        let mut lang_counts: HashMap<String, (usize, usize)> = HashMap::new();
        for f in cg.files.values() {
            let e = lang_counts.entry(f.language.clone()).or_insert((0, 0));
            e.0 += 1;
            e.1 += f.token_count;
        }
        let mut lang_vec: Vec<(String, usize, usize)> = lang_counts
            .into_iter()
            .map(|(l, (c, t))| (l, c, t))
            .collect();
        lang_vec.sort_by_key(|x| std::cmp::Reverse(x.2));

        let mut out = String::new();
        out.push_str(&format!(
            "# REPO MAP ({total_files} files, {total_syms} symbols, {total_edges} edges, {total_repo_tokens} total tok)\n"
        ));
        let lang_str: Vec<String> = lang_vec
            .iter()
            .map(|(l, c, t)| format!("{}:{c}f/{t}t", reg.display_name_for(l)))
            .collect();
        out.push_str(&format!("Langs: {}\n", lang_str.join(", ")));

        out.push_str("\n## Architectural Hubs (God Nodes)\n");
        for node in cg.god_nodes(12, false) {
            out.push_str(&format!(
                "- {} [{}] ({}) in:{} out:{}\n",
                node.name,
                node.kind.as_str(),
                node.file_path,
                node.in_degree,
                node.out_degree
            ));
        }

        out.push_str("\n## Module Clusters (Communities)\n");
        let communities = cg.communities();
        out.push_str(&more_note(communities.len(), 10, "communities"));
        for c in communities.into_iter().take(10) {
            let hubs = if c.top_hubs.is_empty() {
                "-".to_string()
            } else {
                c.top_hubs.join(", ")
            };
            let files_preview = if c.files.len() <= 5 {
                c.files.join(", ")
            } else {
                format!("{}, +{} more", c.files[..5].join(", "), c.files.len() - 5)
            };
            out.push_str(&format!(
                "- C{} `{}` ({}f, {} syms, {} tok) hubs:[{}] -> {}\n",
                c.id,
                c.label,
                c.files.len(),
                c.symbol_count,
                c.total_tokens,
                hubs,
                files_preview
            ));
        }

        // Include persistent project-level AI memory notes if any exist
        if let Ok(notes) = db.find_notes_for_targets(&["project"])
            && !notes.is_empty()
        {
            out.push_str("\n## Saved Project Memory\n");
            out.push_str(&more_note(notes.len(), 6, "project notes"));
            for n in notes.into_iter().take(6) {
                out.push_str(&format!("- {}\n", self.note_line(&cg, &n)));
            }
        }

        // Baseline: exploring a repo typically reads up to 15 largest/hub files (or full repo if small)
        let mut file_toks: Vec<usize> = cg.files.values().map(|f| f.token_count).collect();
        file_toks.sort_by(|a, b| b.cmp(a));
        let baseline_files = file_toks.len().min(15);
        let baseline: usize = file_toks.into_iter().take(baseline_files).sum();

        self.finalize_inner(
            &db,
            "repo_map",
            "*",
            out,
            if max_tokens == 0 { 900 } else { max_tokens },
            baseline,
            baseline_files,
            start,
            record_telemetry,
        )
    }

    /// 4. Fast hybrid lexical + PageRank symbol search.
    pub fn find_symbol(&self, query: &str, limit: usize, max_tokens: usize) -> Result<QueryOutput> {
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;
        let cap = if limit == 0 { 12 } else { limit.min(40) };
        let matches = cg.search_symbols(query, cap);

        if matches.is_empty() {
            return self.finalize(
                &db,
                "find_symbol",
                query,
                format!("No symbols matched `{query}`."),
                max_tokens,
                0,
                0,
                start,
            );
        }

        let mut out = format!("# MATCHES for `{query}` ({} shown)\n", matches.len());
        let mut unique_files = HashSet::new();
        for (sym, score) in &matches {
            if unique_files.len() < 6 {
                unique_files.insert(sym.file_path.clone());
            }
            out.push_str(&format!(
                "- {} [{}] {}:L{}-{} ({}t, score:{:.1})\n  `{}`\n",
                sym.qualified_name,
                sym.kind.as_str(),
                sym.file_path,
                sym.start_line,
                sym.end_line,
                sym.token_count,
                score,
                sym.signature
            ));
        }

        let baseline: usize = unique_files
            .iter()
            .filter_map(|p| cg.files.get(p))
            .map(|f| f.token_count)
            .sum();

        self.finalize(
            &db,
            "find_symbol",
            query,
            out,
            if max_tokens == 0 { 900 } else { max_tokens },
            baseline,
            unique_files.len(),
            start,
        )
    }

    /// 9. Graph-aware text search: finds literal/case-insensitive matches across indexed files
    ///    and tags every matching line with its enclosing symbol (`fn`, `struct`, shader, etc.).
    ///    The output always states its coverage: files searched, matches shown vs total,
    ///    unreadable files, and that non-indexed files are not searched.
    pub fn smart_grep(
        &self,
        pattern: &str,
        file_filter: Option<&str>,
        max_tokens: usize,
    ) -> Result<QueryOutput> {
        const PER_FILE: usize = 12;
        const TOTAL: usize = 50;
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;
        if pattern.trim().is_empty() {
            return self.finalize(
                &db,
                "smart_grep",
                pattern,
                "Empty search pattern.".to_string(),
                max_tokens,
                0,
                0,
                start,
            );
        }

        let indexed: Vec<&String> = cg.files.keys().collect();
        let scan = grep_scan(&self.repo_root, &indexed, pattern, file_filter);

        let mut out = format!("# SMART GREP `{pattern}`\n");
        let mut shown = 0usize;
        let mut files_shown = 0usize;
        for file in &scan.files {
            if shown >= TOTAL {
                break;
            }
            files_shown += 1;
            let file_syms: Vec<&Symbol> = cg
                .symbols_by_file
                .get(&file.path)
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| cg.symbols.get(id))
                        .filter(|s| s.kind != SymbolKind::File && s.kind != SymbolKind::Impl)
                        .collect()
                })
                .unwrap_or_default();
            out.push_str(&format!("\n## {}\n", file.path));
            let take = PER_FILE.min(TOTAL - shown);
            for (line_no, txt) in file.lines.iter().take(take) {
                // Tightest enclosing symbol
                let owner = file_syms
                    .iter()
                    .filter(|s| s.start_line <= *line_no && *line_no <= s.end_line)
                    .min_by_key(|s| s.end_line.saturating_sub(s.start_line))
                    .map(|s| format!("{} [{}]", s.qualified_name, s.kind.as_str()))
                    .unwrap_or_else(|| "top-level".to_string());
                let txt = txt.trim();
                let snippet = if txt.chars().count() > 110 {
                    let s: String = txt.chars().take(107).collect();
                    format!("{s}...")
                } else {
                    txt.to_string()
                };
                out.push_str(&format!("- L{line_no} ({owner}): `{snippet}`\n"));
                shown += 1;
            }
            out.push_str(&more_note(file.lines.len(), take, "matches in this file"));
        }

        let total = scan.total_matches();
        if total == 0 {
            out.push_str("No matches found in the searched files.\n");
        }
        out.push_str(&format!(
            "\nCOVERAGE: case-insensitive substring over {} indexed files{}; {total} matching lines \
             in {} files, {shown} shown{}.",
            scan.files_searched,
            if scan.filtered_out > 0 {
                format!(" ({} more excluded by file_filter)", scan.filtered_out)
            } else {
                String::new()
            },
            scan.files.len(),
            if files_shown < scan.files.len() {
                format!(
                    "; {} matching files not shown (narrow pattern or file_filter)",
                    scan.files.len() - files_shown
                )
            } else {
                String::new()
            }
        ));
        if !scan.unreadable.is_empty() {
            out.push_str(&format!(
                " UNREADABLE (not searched): {}.",
                scan.unreadable.join(", ")
            ));
        }
        out.push_str(&format!(
            " Not searched: files outside the index (types other than {}, ignored files).\n",
            indexed_extensions()
        ));

        let matched_files: Vec<&String> = scan.files.iter().map(|f| &f.path).collect();
        let baseline: usize = matched_files
            .iter()
            .filter_map(|p| cg.files.get(*p))
            .map(|f| f.token_count)
            .sum();

        self.finalize(
            &db,
            "smart_grep",
            pattern,
            out,
            if max_tokens == 0 { 1200 } else { max_tokens },
            baseline,
            matched_files.len(),
            start,
        )
    }
}
/// Matches of one file: `(1-based line, line text)`, in line order.
#[derive(Debug)]
pub struct GrepFile {
    pub path: String,
    pub lines: Vec<(usize, String)>,
}

/// Complete (uncapped) result of scanning indexed files; rendering caps it visibly.
#[derive(Debug, Default)]
pub struct GrepScan {
    pub files: Vec<GrepFile>,
    pub files_searched: usize,
    pub filtered_out: usize,
    pub unreadable: Vec<String>,
}

impl GrepScan {
    pub fn total_matches(&self) -> usize {
        self.files.iter().map(|f| f.lines.len()).sum()
    }
}

/// Case-insensitive (Unicode lowercase) substring scan of `files` (repo-relative paths),
/// sorted by path. Every matching line is returned; nothing is capped here.
pub fn grep_scan(
    repo_root: &Path,
    files: &[&String],
    pattern: &str,
    file_filter: Option<&str>,
) -> GrepScan {
    let pat = pattern.trim().to_lowercase();
    let filter = file_filter
        .map(|f| f.trim().to_lowercase())
        .filter(|f| !f.is_empty());
    let mut sorted: Vec<&String> = files.to_vec();
    sorted.sort();
    let mut scan = GrepScan::default();
    for rel in sorted {
        if let Some(flt) = &filter
            && !rel.to_lowercase().contains(flt.as_str())
        {
            scan.filtered_out += 1;
            continue;
        }
        let Ok(content) = std::fs::read_to_string(repo_root.join(rel)) else {
            scan.unreadable.push(rel.clone());
            continue;
        };
        scan.files_searched += 1;
        let lines: Vec<(usize, String)> = content
            .lines()
            .enumerate()
            .filter(|(_, l)| l.to_lowercase().contains(&pat))
            .map(|(i, l)| (i + 1, l.to_string()))
            .collect();
        if !lines.is_empty() {
            scan.files.push(GrepFile {
                path: rel.clone(),
                lines,
            });
        }
    }
    scan
}
