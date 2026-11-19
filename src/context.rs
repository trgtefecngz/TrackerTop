use crate::db::Database;
use crate::graph::CodeGraph;
use crate::indexer::index_repository;
use crate::model::{Language, Symbol, SymbolKind, TokenStatsSummary};
use crate::tokens::{count_tokens, truncate_to_tokens};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
}

impl TrackerEngine {
    pub fn new(repo_root: &Path) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
        }
    }

    fn load_fresh(&self) -> Result<(Database, CodeGraph)> {
        // Fast incremental refresh (mtime + size check skips unchanged files in milliseconds)
        index_repository(&self.repo_root, false)?;
        let db = Database::open_for_repo(&self.repo_root)?;
        let graph = CodeGraph::load(&db)?;
        Ok((db, graph))
    }

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

        let _ = db.record_telemetry(
            tool,
            query_label,
            tokens_sent,
            baseline_tokens,
            files_avoided,
            latency_ms,
        );

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

        // Language breakdown
        let mut lang_counts: HashMap<Language, (usize, usize)> = HashMap::new();
        for f in cg.files.values() {
            let e = lang_counts.entry(f.language).or_insert((0, 0));
            e.0 += 1;
            e.1 += f.token_count;
        }
        let mut lang_vec: Vec<(Language, usize, usize)> = lang_counts
            .into_iter()
            .map(|(l, (c, t))| (l, c, t))
            .collect();
        lang_vec.sort_by(|a, b| b.2.cmp(&a.2));

        let mut out = String::new();
        out.push_str(&format!(
            "# REPO MAP ({total_files} files, {total_syms} symbols, {total_edges} edges, {total_repo_tokens} total tok)\n"
        ));
        let lang_str: Vec<String> = lang_vec
            .iter()
            .map(|(l, c, t)| format!("{}:{c}f/{t}t", l.display_name()))
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

        // Baseline: exploring a repo typically reads up to 15 largest/hub files (or full repo if small)
        let mut file_toks: Vec<usize> = cg.files.values().map(|f| f.token_count).collect();
        file_toks.sort_by(|a, b| b.cmp(a));
        let baseline_files = file_toks.len().min(15);
        let baseline: usize = file_toks.into_iter().take(baseline_files).sum();

        self.finalize(
            &db,
            "repo_map",
            "*",
            out,
            if max_tokens == 0 { 900 } else { max_tokens },
            baseline,
            baseline_files,
            start,
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
            file.language.display_name(),
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
    /// Includes full code for top 1-2 primary symbols + signatures only for 1-hop neighbors + shortest links.
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
            for (_, kind, nbr) in callees.into_iter().chain(callers.into_iter()) {
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

    /// 8. Token-savings telemetry summary.
    pub fn token_stats(&self) -> Result<TokenStatsSummary> {
        let db = Database::open_for_repo(&self.repo_root)?;
        db.get_token_stats(20)
    }
}
