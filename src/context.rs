use crate::db::Database;
use crate::graph::CodeGraph;
use crate::indexer::index_repository;
use crate::model::{Symbol, SymbolKind, TokenStatsSummary};
use crate::parser::default_registry;
use crate::tokens::{count_tokens, truncate_to_tokens};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Result of an AI-first query, including exact token-savings metrics.
#[derive(Debug, Clone)]
pub struct QueryOutput {
    pub text: String,
    pub tokens_sent: usize,
    pub baseline_tokens: usize,
    pub tokens_saved: usize,
    pub files_avoided: usize,
    pub latency_ms: u64,
}

pub struct TrackerEngine {
    pub repo_root: PathBuf,
    /// Last built graph, tagged with the DB `index_generation` it was built from.
    /// Long-lived engines (the MCP server) reuse it until the index changes.
    graph_cache: Mutex<Option<(i64, Arc<CodeGraph>)>>,
}

impl TrackerEngine {
    pub fn new(repo_root: &Path) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            graph_cache: Mutex::new(None),
        }
    }

    fn load_fresh(&self) -> Result<(Database, Arc<CodeGraph>)> {
        // Fast incremental refresh (mtime + size check skips unchanged files in milliseconds)
        let stats = index_repository(&self.repo_root, false)?;
        let db = Database::open_for_repo(&self.repo_root)?;

        let mut cache = self
            .graph_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((generation, graph)) = cache.as_ref()
            && *generation == stats.generation
        {
            return Ok((db, Arc::clone(graph)));
        }
        let graph = Arc::new(CodeGraph::load(&db)?);
        *cache = Some((stats.generation, Arc::clone(&graph)));
        Ok((db, graph))
    }

    #[allow(clippy::too_many_arguments)]
    fn finalize(
        &self,
        db: &Database,
        tool: &str,
        query_label: &str,
        body: String,
        max_tokens: usize,
        baseline_tokens: usize,
        files_avoided: usize,
        start: Instant,
    ) -> Result<QueryOutput> {
        self.finalize_inner(
            db,
            tool,
            query_label,
            body,
            max_tokens,
            baseline_tokens,
            files_avoided,
            start,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finalize_inner(
        &self,
        db: &Database,
        tool: &str,
        query_label: &str,
        body: String,
        max_tokens: usize,
        baseline_tokens: usize,
        files_avoided: usize,
        start: Instant,
        record_telemetry: bool,
    ) -> Result<QueryOutput> {
        let budget = if max_tokens == 0 { 1500 } else { max_tokens };
        // Reserve ~16 tokens for the compact savings footer
        let (trimmed, _, _) = truncate_to_tokens(&body, budget.saturating_sub(16).max(64));
        let sent_est = count_tokens(&trimmed) + 14;
        let saved = baseline_tokens.saturating_sub(sent_est);
        let pct = if baseline_tokens > 0 {
            (saved as f64 / baseline_tokens as f64) * 100.0
        } else {
            0.0
        };

        let final_text = format!(
            "{trimmed}\n\n[ttop:{tool} | sent {sent_est} tok | baseline {baseline_tokens} tok | saved {saved} tok ({pct:.0}%)]"
        );
        let tokens_sent = count_tokens(&final_text);
        let tokens_saved = baseline_tokens.saturating_sub(tokens_sent);
        let latency_ms = start.elapsed().as_millis() as u64;

        if record_telemetry {
            let _ = db.record_telemetry(
                tool,
                query_label,
                tokens_sent,
                baseline_tokens,
                files_avoided,
                latency_ms,
            );
        }

        Ok(QueryOutput {
            text: final_text,
            tokens_sent,
            baseline_tokens,
            tokens_saved,
            files_avoided,
            latency_ms,
        })
    }

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
        for c in cg.communities().into_iter().take(10) {
            let hubs = if c.top_hubs.is_empty() {
                "-".to_string()
            } else {
                c.top_hubs.join(", ")
            };
            let files_preview = if c.files.len() <= 5 {
                c.files.join(", ")
            } else {
                format!(
                    "{}, +{} more",
                    c.files[..5].join(", "),
                    c.files.len() - 5
                )
            };
            out.push_str(&format!(
                "- C{} `{}` ({}f, {} syms, {} tok) hubs:[{}] -> {}\n",
                c.id, c.label, c.files.len(), c.symbol_count, c.total_tokens, hubs, files_preview
            ));
        }

        // Include persistent project-level AI memory notes if any exist
        if let Ok(notes) = db.find_notes_for_targets(&["project"])
            && !notes.is_empty() {
                out.push_str("\n## Saved Project Memory\n");
                for n in notes.into_iter().take(6) {
                    out.push_str(&format!("- [{}] {}\n", n.target, n.content));
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
                format!("File not found in index: `{path}`"),
                max_tokens,
                0,
                0,
                start,
            );
        };

        let mut out = format!(
            "# OUTLINE {} ({}, {} lines, {} full tok)\n",
            file.path,
            default_registry().display_name_for(&file.language),
            file.line_count,
            file.token_count
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

        let Some(sym) = cg.resolve_symbol(symbol_query) else {
            return self.finalize(
                &db,
                "read_symbol",
                symbol_query,
                format!("Symbol not found: `{symbol_query}`. Try `find_symbol` first."),
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
        if !sym.doc_summary.is_empty() {
            out.push_str(&format!("Doc: {}\n", sym.doc_summary));
        }
        if let Ok(notes) = db.find_notes_for_targets(&[&sym.name, &sym.qualified_name, &sym.file_path]) {
            for n in notes.into_iter().take(4) {
                out.push_str(&format!("Note [{}]: {}\n", n.target, n.content));
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
                for (_, kind, dep) in callees.into_iter().take(10) {
                    touched_files.insert(dep.file_path.clone());
                    out.push_str(&format!(
                        "- [{}] {} ({}:L{}) -> `{}`\n",
                        kind.as_str(),
                        dep.qualified_name,
                        dep.file_path,
                        dep.start_line,
                        dep.signature
                    ));
                }
            }
            if !callers.is_empty() {
                out.push_str("\n## Direct Callers\n");
                for (_, kind, caller) in callers.into_iter().take(10) {
                    touched_files.insert(caller.file_path.clone());
                    out.push_str(&format!(
                        "- [{}] {} ({}:L{})\n",
                        kind.as_str(),
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

    /// 4. Fast hybrid lexical + PageRank symbol search.
    pub fn find_symbol(
        &self,
        query: &str,
        limit: usize,
        max_tokens: usize,
    ) -> Result<QueryOutput> {
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

    /// 5. Directed N-hop call & dependency neighborhood around a symbol.
    pub fn call_graph(
        &self,
        symbol_query: &str,
        depth: usize,
        max_tokens: usize,
    ) -> Result<QueryOutput> {
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;

        let Some(sym) = cg.resolve_symbol(symbol_query) else {
            return self.finalize(
                &db,
                "call_graph",
                symbol_query,
                format!("Symbol not found: `{symbol_query}`"),
                max_tokens,
                0,
                0,
                start,
            );
        };

        let (callers, callees) = cg.neighborhood(&sym.id, depth.max(1));
        let mut touched_files = HashSet::new();
        touched_files.insert(sym.file_path.clone());

        let mut out = format!(
            "# CALL GRAPH {} ({}:L{}, depth={})\n",
            sym.qualified_name,
            sym.file_path,
            sym.start_line,
            depth.max(1)
        );
        out.push_str(&format!("Sig: `{}`\n", sym.signature));

        out.push_str(&format!("\n## Incoming / Callers ({})\n", callers.len()));
        for (d, kind, s) in callers.iter().take(20) {
            touched_files.insert(s.file_path.clone());
            out.push_str(&format!(
                "- d{d} [{}] {} ({}:L{})\n",
                kind.as_str(),
                s.qualified_name,
                s.file_path,
                s.start_line
            ));
        }

        out.push_str(&format!("\n## Outgoing / Callees & Types ({})\n", callees.len()));
        for (d, kind, s) in callees.iter().take(20) {
            touched_files.insert(s.file_path.clone());
            out.push_str(&format!(
                "- d{d} [{}] {} ({}:L{}) -> `{}`\n",
                kind.as_str(),
                s.qualified_name,
                s.file_path,
                s.start_line,
                s.signature
            ));
        }

        let baseline: usize = touched_files
            .iter()
            .filter_map(|p| cg.files.get(p))
            .map(|f| f.token_count)
            .sum();

        self.finalize(
            &db,
            "call_graph",
            symbol_query,
            out,
            if max_tokens == 0 { 1000 } else { max_tokens },
            baseline,
            touched_files.len(),
            start,
        )
    }

    /// 6. Blast radius & affected tests when changing a symbol.
    pub fn impact_analysis(
        &self,
        symbol_query: &str,
        depth: usize,
        max_tokens: usize,
    ) -> Result<QueryOutput> {
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;

        let Some(sym) = cg.resolve_symbol(symbol_query) else {
            return self.finalize(
                &db,
                "impact_analysis",
                symbol_query,
                format!("Symbol not found: `{symbol_query}`"),
                max_tokens,
                0,
                0,
                start,
            );
        };

        let Some(report) = cg.impact_analysis(&sym.id, depth.max(2)) else {
            return self.finalize(
                &db,
                "impact_analysis",
                symbol_query,
                format!("No impact data for `{symbol_query}`"),
                max_tokens,
                0,
                0,
                start,
            );
        };

        let mut out = format!(
            "# IMPACT ANALYSIS {} ({}:L{})\nAffected files: {} | Direct: {} | Transitive: {} | Tests: {}\n",
            report.root.qualified_name,
            report.root.file_path,
            report.root.start_line,
            report.affected_files.len(),
            report.direct_dependents.len(),
            report.transitive_dependents.len(),
            report.related_tests.len(),
        );

        if !report.direct_dependents.is_empty() {
            out.push_str("\n## Direct Dependents (1-hop)\n");
            for s in report.direct_dependents.iter().take(15) {
                out.push_str(&format!(
                    "- {} [{}] ({}:L{})\n",
                    s.qualified_name,
                    s.kind.as_str(),
                    s.file_path,
                    s.start_line
                ));
            }
        }

        if !report.transitive_dependents.is_empty() {
            out.push_str("\n## Transitive Dependents (2+ hops)\n");
            for s in report.transitive_dependents.iter().take(15) {
                out.push_str(&format!(
                    "- {} [{}] ({}:L{})\n",
                    s.qualified_name,
                    s.kind.as_str(),
                    s.file_path,
                    s.start_line
                ));
            }
        }

        if !report.related_tests.is_empty() {
            out.push_str("\n## Linked Tests to Re-run\n");
            for t in &report.related_tests {
                out.push_str(&format!("- {} ({}:L{})\n", t.name, t.file_path, t.start_line));
            }
        }

        let baseline: usize = report
            .affected_files
            .iter()
            .filter_map(|p| cg.files.get(p))
            .map(|f| f.token_count)
            .sum();

        self.finalize(
            &db,
            "impact_analysis",
            symbol_query,
            out,
            if max_tokens == 0 { 1000 } else { max_tokens },
            baseline,
            report.affected_files.len(),
            start,
        )
    }

    /// 7. AI-First Flagship Tool: packs the highest-value surgical context for a task inside `max_tokens`!
    ///    Includes full code for top 1-2 primary symbols + signatures only for 1-hop neighbors + shortest links.
    pub fn task_context(&self, task: &str, max_tokens: usize) -> Result<QueryOutput> {
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;
        let budget = if max_tokens == 0 { 1800 } else { max_tokens };

        let hits = cg.search_symbols(task, 8);
        if hits.is_empty() {
            // Fall back to repo_map if query has no direct lexical match
            return self.repo_map(budget);
        }

        let mut out = format!("# TASK CONTEXT PACK for `{task}` (budget={budget} tok)\n");
        let mut touched_files = HashSet::new();
        let mut included_ids = HashSet::new();

        // Top 2-3 substantive code symbols get full surgical source code (within ~55% of budget)
        let primary_budget = (budget as f64 * 0.55) as usize;
        let mut primary_used = 0usize;

        let is_substantive_code = |s: &Symbol| {
            !matches!(
                s.kind,
                SymbolKind::File
                    | SymbolKind::Section
                    | SymbolKind::Module
                    | SymbolKind::Dependency
                    | SymbolKind::Impl
            )
        };

        out.push_str("\n## Core Targets (Surgical Code)\n");
        let core_candidates: Vec<&Symbol> = {
            let code_only: Vec<&Symbol> = hits
                .iter()
                .map(|(s, _)| *s)
                .filter(|s| is_substantive_code(s))
                .collect();
            if !code_only.is_empty() {
                code_only
            } else {
                hits.iter()
                    .map(|(s, _)| *s)
                    .filter(|s| s.kind != SymbolKind::File)
                    .collect()
            }
        };
        for sym in core_candidates.into_iter().take(3) {
            if primary_used > primary_budget && !included_ids.is_empty() {
                break;
            }
            let abs = self.repo_root.join(&sym.file_path);
            let content = std::fs::read_to_string(&abs).unwrap_or_default();
            let lines: Vec<&str> = content.lines().collect();
            let s_idx = sym.start_line.saturating_sub(1).min(lines.len());
            let e_idx = sym.end_line.min(lines.len());
            let snippet = lines[s_idx..e_idx].join("\n");
            let snip_toks = count_tokens(&snippet);

            if primary_used + snip_toks <= primary_budget || included_ids.is_empty() {
                touched_files.insert(sym.file_path.clone());
                included_ids.insert(sym.id.clone());
                primary_used += snip_toks;
                out.push_str(&format!(
                    "\n### {} [{}] ({}:L{}-{})\n```\n",
                    sym.qualified_name,
                    sym.kind.as_str(),
                    sym.file_path,
                    sym.start_line,
                    sym.end_line
                ));
                for (i, line) in lines[s_idx..e_idx].iter().enumerate() {
                    out.push_str(&format!("{:>4} | {line}\n", sym.start_line + i));
                }
                out.push_str("```\n");
            }
        }

        // Gather 1-hop neighbors of core targets (signatures only!)
        out.push_str("\n## Connected Context (1-Hop Signatures Only)\n");
        let mut neighbor_seen = included_ids.clone();
        for core_id in &included_ids {
            let (callers, callees) = cg.neighborhood(core_id, 1);
            for (_, kind, nbr) in callees.into_iter().chain(callers) {
                if neighbor_seen.insert(nbr.id.clone()) && nbr.kind != SymbolKind::File {
                    touched_files.insert(nbr.file_path.clone());
                    out.push_str(&format!(
                        "- [{}] {} ({}:L{}) `{}`\n",
                        kind.as_str(),
                        nbr.qualified_name,
                        nbr.file_path,
                        nbr.start_line,
                        nbr.signature
                    ));
                }
            }
        }

        // Remaining ranked hits as 1-line pointers
        let secondary: Vec<_> = hits
            .iter()
            .filter(|(s, _)| !neighbor_seen.contains(&s.id))
            .collect();
        if !secondary.is_empty() {
            out.push_str("\n## Other Related Symbols (Use read_symbol if needed)\n");
            for (sym, _) in secondary {
                touched_files.insert(sym.file_path.clone());
                out.push_str(&format!(
                    "- {} [{}] ({}:L{}-{}) `{}`\n",
                    sym.qualified_name,
                    sym.kind.as_str(),
                    sym.file_path,
                    sym.start_line,
                    sym.end_line,
                    sym.signature
                ));
            }
        }

        let baseline: usize = touched_files
            .iter()
            .filter_map(|p| cg.files.get(p))
            .map(|f| f.token_count)
            .sum();

        self.finalize(
            &db,
            "task_context",
            task,
            out,
            budget,
            baseline,
            touched_files.len(),
            start,
        )
    }

    /// Bonus helper: shortest path between two symbols/files.
    pub fn shortest_path(&self, from: &str, to: &str, max_tokens: usize) -> Result<QueryOutput> {
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;

        let (Some(a), Some(b)) = (cg.resolve_symbol(from), cg.resolve_symbol(to)) else {
            return self.finalize(
                &db,
                "shortest_path",
                &format!("{from}->{to}"),
                format!("Could not resolve both endpoints: `{from}` and `{to}`"),
                max_tokens,
                0,
                0,
                start,
            );
        };

        let Some(path) = cg.shortest_path(&a.id, &b.id) else {
            return self.finalize(
                &db,
                "shortest_path",
                &format!("{from}->{to}"),
                format!("No graph path found between `{}` and `{}`.", a.qualified_name, b.qualified_name),
                max_tokens,
                0,
                0,
                start,
            );
        };

        let mut touched_files = HashSet::new();
        let mut out = format!(
            "# PATH {} -> {} ({} hops)\n",
            a.qualified_name,
            b.qualified_name,
            path.len().saturating_sub(1)
        );
        for (i, s) in path.iter().enumerate() {
            touched_files.insert(s.file_path.clone());
            out.push_str(&format!(
                "{}. {} [{}] ({}:L{})\n",
                i + 1,
                s.qualified_name,
                s.kind.as_str(),
                s.file_path,
                s.start_line
            ));
        }

        let baseline: usize = touched_files
            .iter()
            .filter_map(|p| cg.files.get(p))
            .map(|f| f.token_count)
            .sum();

        self.finalize(
            &db,
            "shortest_path",
            &format!("{from}->{to}"),
            out,
            if max_tokens == 0 { 600 } else { max_tokens },
            baseline,
            touched_files.len(),
            start,
        )
    }

    /// 9. Graph-aware text search: finds literal/case-insensitive matches across indexed files
    ///    and tags every matching line with its enclosing symbol (`fn`, `struct`, shader, etc.).
    pub fn smart_grep(
        &self,
        pattern: &str,
        file_filter: Option<&str>,
        max_tokens: usize,
    ) -> Result<QueryOutput> {
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;
        let pat_lower = pattern.trim().to_ascii_lowercase();
        if pat_lower.is_empty() {
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

        let filter_l = file_filter.map(|f| f.trim().to_ascii_lowercase());
        let mut sorted_files: Vec<&String> = cg.files.keys().collect();
        sorted_files.sort();

        let mut out = format!("# SMART GREP `{pattern}`\n");
        let mut total_hits = 0usize;
        let mut matched_files = Vec::new();

        for rel_path in sorted_files {
            if let Some(ref flt) = filter_l
                && !flt.is_empty() && !rel_path.to_ascii_lowercase().contains(flt) {
                    continue;
                }
            let abs = self.repo_root.join(rel_path);
            let Ok(content) = std::fs::read_to_string(&abs) else {
                continue;
            };
            if !content.to_ascii_lowercase().contains(&pat_lower) {
                continue;
            }

            let file_syms: Vec<&Symbol> = cg
                .symbols_by_file
                .get(rel_path)
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| cg.symbols.get(id))
                        .filter(|s| s.kind != SymbolKind::File && s.kind != SymbolKind::Impl)
                        .collect()
                })
                .unwrap_or_default();

            let mut file_hits = Vec::new();
            for (idx, line) in content.lines().enumerate() {
                if line.to_ascii_lowercase().contains(&pat_lower) {
                    let line_no = idx + 1;
                    // Find tightest enclosing symbol
                    let owner = file_syms
                        .iter()
                        .filter(|s| s.start_line <= line_no && line_no <= s.end_line)
                        .min_by_key(|s| s.end_line.saturating_sub(s.start_line))
                        .map(|s| format!("{} [{}]", s.qualified_name, s.kind.as_str()))
                        .unwrap_or_else(|| "top-level".to_string());
                    file_hits.push((line_no, owner, line.trim().to_string()));
                    if file_hits.len() >= 12 {
                        break;
                    }
                }
            }

            if !file_hits.is_empty() {
                matched_files.push(rel_path.clone());
                out.push_str(&format!("\n## {rel_path}\n"));
                for (ln, owner, txt) in file_hits {
                    total_hits += 1;
                    let snippet = if txt.chars().count() > 110 {
                        let s: String = txt.chars().take(107).collect();
                        format!("{s}...")
                    } else {
                        txt
                    };
                    out.push_str(&format!("- L{ln} ({owner}): `{snippet}`\n"));
                }
                if total_hits >= 50 {
                    break;
                }
            }
        }

        if total_hits == 0 {
            out.push_str("No matches found.\n");
        }

        let baseline: usize = matched_files
            .iter()
            .filter_map(|p| cg.files.get(p))
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

        for q in symbol_queries.iter().take(10) {
            let Some(sym) = cg.resolve_symbol(q) else {
                out.push_str(&format!("\n## `{q}` — NOT FOUND\n"));
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
            if let Ok(notes) =
                db.find_notes_for_targets(&[&sym.name, &sym.qualified_name, &sym.file_path])
            {
                for n in notes.into_iter().take(2) {
                    out.push_str(&format!("Note [{}]: {}\n", n.target, n.content));
                }
            }
            out.push_str("```\n");
            for (i, line) in lines[s_idx..e_idx].iter().enumerate() {
                out.push_str(&format!("{:>4} | {line}\n", sym.start_line + i));
            }
            out.push_str("```\n");

            if include_deps {
                let (_, callees) = cg.neighborhood(&sym.id, 1);
                for (_, kind, dep) in callees.into_iter().take(5) {
                    touched_files.insert(dep.file_path.clone());
                    out.push_str(&format!(
                        "- [{}] {} ({}:L{}) `{}`\n",
                        kind.as_str(),
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

    /// 11. Git-powered diff blast radius: maps uncommitted (or latest commit) changed files
    ///     to dependent files, git co-changing files, and linked tests to re-run.
    pub fn diff_impact(&self, max_tokens: usize) -> Result<QueryOutput> {
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;
        let (scope_label, changed_files) = crate::git::changed_files_for_diff(&self.repo_root);

        if changed_files.is_empty() {
            return self.finalize(
                &db,
                "diff_impact",
                &scope_label,
                format!("# DIFF IMPACT ({scope_label})\nNo modified files detected."),
                max_tokens,
                0,
                0,
                start,
            );
        }

        let mut out = format!(
            "# DIFF IMPACT ({scope_label} — {} modified files)\n",
            changed_files.len()
        );
        let mut affected_files: HashSet<String> = HashSet::new();
        let mut cochange_suggestions: HashSet<String> = HashSet::new();
        let mut tests_to_run: HashSet<String> = HashSet::new();

        for path in &changed_files {
            affected_files.insert(path.clone());
            out.push_str(&format!("- Modified: `{path}`\n"));
            if let Some(sym_ids) = cg.symbols_by_file.get(path) {
                for sid in sym_ids {
                    if let Some(imp) = cg.impact_analysis(sid, 2) {
                        for af in imp.affected_files {
                            affected_files.insert(af);
                        }
                        for t in imp.related_tests {
                            tests_to_run.insert(format!("{} ({}:L{})", t.name, t.file_path, t.start_line));
                        }
                    }
                    // Check CO_CHANGES neighbors
                    let (_, outgoing) = cg.neighborhood(sid, 1);
                    for (_, kind, nbr) in outgoing {
                        if kind == crate::model::EdgeKind::CoChanges
                            && !changed_files.contains(&nbr.file_path)
                        {
                            cochange_suggestions.insert(nbr.file_path.clone());
                        }
                    }
                }
            }
        }

        let mut dep_files: Vec<String> = affected_files
            .iter()
            .filter(|f| !changed_files.contains(*f))
            .cloned()
            .collect();
        dep_files.sort();

        if !dep_files.is_empty() {
            out.push_str(&format!("\n## Impacted Dependent Files ({})\n", dep_files.len()));
            for f in dep_files.iter().take(20) {
                out.push_str(&format!("- `{f}`\n"));
            }
        }

        if !cochange_suggestions.is_empty() {
            let mut co: Vec<String> = cochange_suggestions.into_iter().collect();
            co.sort();
            out.push_str("\n## Git Co-Change Alert (Often Modified Together)\n");
            for f in co.iter().take(12) {
                out.push_str(&format!("- `{f}` (check if it needs a matching update)\n"));
            }
        }

        if !tests_to_run.is_empty() {
            let mut ts: Vec<String> = tests_to_run.into_iter().collect();
            ts.sort();
            out.push_str("\n## Linked Tests to Re-run\n");
            for t in ts {
                out.push_str(&format!("- {t}\n"));
            }
        }

        let baseline: usize = affected_files
            .iter()
            .filter_map(|p| cg.files.get(p))
            .map(|f| f.token_count)
            .sum();

        self.finalize(
            &db,
            "diff_impact",
            &scope_label,
            out,
            if max_tokens == 0 { 1200 } else { max_tokens },
            baseline,
            affected_files.len(),
            start,
        )
    }

    /// 12. Repository health diagnostics: token-bloat files, circular dependencies, orphan files.
    pub fn health_check(&self, max_tokens: usize) -> Result<QueryOutput> {
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;
        let hr = cg.health_report();

        let mut out = format!(
            "# CODE HEALTH CHECK\nBloated files (>=3500 tok): {} | Circular clusters: {} | Orphan code files: {}\n",
            hr.bloated_files.len(),
            hr.circular_groups.len(),
            hr.orphan_files.len()
        );

        if !hr.bloated_files.is_empty() {
            out.push_str("\n## Token Bloat Warnings (Consider Splitting for Better AI Accuracy)\n");
            for (p, toks, lines) in hr.bloated_files.iter().take(15) {
                out.push_str(&format!("- `{p}` — {toks} tok ({lines} lines)\n"));
            }
        }
        if !hr.circular_groups.is_empty() {
            out.push_str("\n## Circular Dependency Cycles\n");
            for cyc in hr.circular_groups.iter().take(10) {
                out.push_str(&format!("- Cycle ({} files): {}\n", cyc.len(), cyc.join(" <-> ")));
            }
        }
        if !hr.orphan_files.is_empty() {
            out.push_str("\n## Potential Orphan / Unreferenced Code Files\n");
            for p in hr.orphan_files.iter().take(20) {
                out.push_str(&format!("- `{p}`\n"));
            }
        }

        let baseline: usize = cg.files.values().map(|f| f.token_count).sum::<usize>().min(40_000);
        self.finalize(
            &db,
            "health_check",
            "*",
            out,
            if max_tokens == 0 { 1000 } else { max_tokens },
            baseline,
            cg.files.len(),
            start,
        )
    }

    /// 13. Persists an architectural memory note attached to a symbol, file, or `"project"`.
    pub fn remember(&self, target: &str, note: &str) -> Result<String> {
        let db = Database::open_for_repo(&self.repo_root)?;
        let id = db.add_note(target, note)?;
        Ok(format!(
            "Saved memory note #{id} for `{}` — future sessions will see it automatically.",
            if target.trim().is_empty() { "project" } else { target.trim() }
        ))
    }

    /// 14. Recalls saved architectural memory notes.
    pub fn recall(&self, query: Option<&str>) -> Result<String> {
        let db = Database::open_for_repo(&self.repo_root)?;
        let notes = db.list_notes(query)?;
        if notes.is_empty() {
            return Ok("No saved memory notes yet. Use `remember(target, note)` to store architectural facts.".to_string());
        }
        let mut out = format!("# SAVED MEMORY NOTES ({})\n", notes.len());
        for n in notes {
            out.push_str(&format!("- #{} [{}]: {}\n", n.id, n.target, n.content));
        }
        Ok(out)
    }

    /// 8. Token-savings telemetry summary.
    pub fn token_stats(&self) -> Result<TokenStatsSummary> {
        let db = Database::open_for_repo(&self.repo_root)?;
        db.get_token_stats(20)
    }
}
