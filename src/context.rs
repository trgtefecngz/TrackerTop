use crate::db::Database;
use crate::graph::CodeGraph;
use crate::indexer::index_repository;
use crate::model::{ProjectNote, Symbol, SymbolKind, TokenStatsSummary};
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
    /// Telemetry source tag for calls made through this engine (see [`Self::set_source`]).
    source: Mutex<String>,
}

impl TrackerEngine {
    /// The source defaults to `cli`, or to `TRACKERTOP_TELEMETRY_SOURCE` when set (e.g.
    /// `benchmark` for scripted measurement runs, so they never count as agent usage).
    pub fn new(repo_root: &Path) -> Self {
        let source = std::env::var("TRACKERTOP_TELEMETRY_SOURCE")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "cli".to_string());
        Self {
            repo_root: repo_root.to_path_buf(),
            graph_cache: Mutex::new(None),
            source: Mutex::new(source),
        }
    }

    /// Tags later telemetry rows: `mcp:<client>` for MCP sessions, `ui` for the desktop app.
    /// An explicit `TRACKERTOP_TELEMETRY_SOURCE` always wins, so benchmarks stay separate.
    pub fn set_source(&self, source: &str) {
        if std::env::var("TRACKERTOP_TELEMETRY_SOURCE").is_ok_and(|s| !s.trim().is_empty()) {
            return;
        }
        *self
            .source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = source.to_string();
    }

    pub fn source(&self) -> String {
        self.source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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
                &self.source(),
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
                out.push_str(&more_note(callers.len(), 10, "callers"));
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
                Self::symbol_miss(&cg, symbol_query),
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
        out.push_str(&more_note(callers.len(), 20, "callers"));
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

        out.push_str(&format!(
            "\n## Outgoing / Callees & Types ({})\n",
            callees.len()
        ));
        out.push_str(&more_note(callees.len(), 20, "callees"));
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
                Self::symbol_miss(&cg, symbol_query),
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
            out.push_str(&more_note(
                report.direct_dependents.len(),
                15,
                "direct dependents",
            ));
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
            out.push_str(&more_note(
                report.transitive_dependents.len(),
                15,
                "transitive dependents",
            ));
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
                out.push_str(&format!(
                    "- {} ({}:L{})\n",
                    t.name, t.file_path, t.start_line
                ));
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
                [
                    (from, cg.resolve_symbol(from).is_none()),
                    (to, cg.resolve_symbol(to).is_none()),
                ]
                .into_iter()
                .filter(|(_, missing)| *missing)
                .map(|(q, _)| Self::symbol_miss(&cg, q))
                .collect::<Vec<_>>()
                .join("\n"),
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
                format!(
                    "No graph path found between `{}` and `{}`.",
                    a.qualified_name, b.qualified_name
                ),
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
                            tests_to_run
                                .insert(format!("{} ({}:L{})", t.name, t.file_path, t.start_line));
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
            out.push_str(&format!(
                "\n## Impacted Dependent Files ({})\n",
                dep_files.len()
            ));
            out.push_str(&more_note(dep_files.len(), 20, "dependent files"));
            for f in dep_files.iter().take(20) {
                out.push_str(&format!("- `{f}`\n"));
            }
        }

        if !cochange_suggestions.is_empty() {
            let mut co: Vec<String> = cochange_suggestions.into_iter().collect();
            co.sort();
            out.push_str("\n## Git Co-Change Alert (Often Modified Together)\n");
            out.push_str(&more_note(co.len(), 12, "co-change files"));
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
            out.push_str(&more_note(hr.bloated_files.len(), 15, "bloated files"));
            for (p, toks, lines) in hr.bloated_files.iter().take(15) {
                out.push_str(&format!("- `{p}` — {toks} tok ({lines} lines)\n"));
            }
        }
        if !hr.circular_groups.is_empty() {
            out.push_str("\n## Circular Dependency Cycles\n");
            out.push_str(&more_note(
                hr.circular_groups.len(),
                10,
                "dependency cycles",
            ));
            for cyc in hr.circular_groups.iter().take(10) {
                out.push_str(&format!(
                    "- Cycle ({} files): {}\n",
                    cyc.len(),
                    cyc.join(" <-> ")
                ));
            }
        }
        if !hr.orphan_files.is_empty() {
            out.push_str("\n## Potential Orphan / Unreferenced Code Files\n");
            out.push_str(&more_note(hr.orphan_files.len(), 20, "orphan files"));
            for p in hr.orphan_files.iter().take(20) {
                out.push_str(&format!("- `{p}`\n"));
            }
        }

        let baseline: usize = cg
            .files
            .values()
            .map(|f| f.token_count)
            .sum::<usize>()
            .min(40_000);
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

    /// 13. Persists a note, decision or dismissed review lead attached to a symbol, file, or
    ///     `"project"`. Decisions and dismissed leads require a reason.
    pub fn remember(&self, target: &str, note: &str) -> Result<String> {
        self.remember_kind(target, note, "note", "")
    }

    pub fn remember_kind(
        &self,
        target: &str,
        note: &str,
        kind: &str,
        reason: &str,
    ) -> Result<String> {
        let (db, cg) = self.load_fresh()?;
        // Provenance: which commit, and a hash of the target's code, so a later read can
        // flag the note when that code has changed
        let commit = crate::changes::git(&self.repo_root, &["rev-parse", "--short=12", "HEAD"])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let code_hash = self.note_code_hash(&cg, target).unwrap_or_default();
        let (id, mirror_err) =
            db.add_note_with_provenance(target, note, kind, reason, &commit, &code_hash)?;
        let target = if target.trim().is_empty() {
            "project"
        } else {
            target.trim()
        };
        let mut out = format!(
            "Saved {kind} #{id} for `{target}`. It is shown automatically when that symbol or \
             file is read (read_symbol, read_symbols_batch, file_outline) and in recall."
        );
        if let Some(e) = mirror_err {
            out.push_str(&format!(
                "\nWARNING: stored in the index DB, but the Markdown copy in .trackertop/notes/ \
                 was not written ({e}); it will not survive an index rebuild."
            ));
        }
        Ok(out)
    }

    /// Hash of the code a note target names, as it is now: a review finding id
    /// (`rule:path:symbol`), an indexed file path, or a symbol. `None` when the target is
    /// not code (`project`) or cannot be found.
    fn note_code_hash(&self, cg: &CodeGraph, target: &str) -> Option<String> {
        let t = target.trim().replace('\\', "/");
        if t.is_empty() || t.eq_ignore_ascii_case("project") {
            return None;
        }
        let hash = |text: &str| blake3::hash(text.as_bytes()).to_hex()[..16].to_string();
        let file_text = |path: &str| std::fs::read_to_string(self.repo_root.join(path)).ok();
        let symbol_text = |s: &Symbol| {
            let text = file_text(&s.file_path)?;
            Some(
                text.lines()
                    .skip(s.start_line.saturating_sub(1))
                    .take(s.end_line + 1 - s.start_line.min(s.end_line + 1))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        };
        // A review finding id: rule:path:qualified_name
        let mut parts = t.splitn(3, ':');
        if let (Some(_rule), Some(path), Some(qname)) = (parts.next(), parts.next(), parts.next())
            && !path.is_empty()
            && (cg.files.contains_key(path) || self.repo_root.join(path).is_file())
        {
            let sym = cg.symbols_by_file.get(path).and_then(|ids| {
                ids.iter()
                    .filter_map(|id| cg.symbols.get(id))
                    .find(|s| s.qualified_name == qname)
            });
            return match sym {
                Some(s) => symbol_text(s).map(|x| hash(&x)),
                // The symbol is gone (a removed function): its file stands in for it
                None => file_text(path).map(|x| hash(&x)),
            };
        }
        if cg.files.contains_key(&t) {
            return file_text(&t).map(|x| hash(&x));
        }
        let s = cg.resolve_symbol(&t)?;
        symbol_text(s).map(|x| hash(&x))
    }

    /// A note as shown next to code: its text, plus a warning when the code it was written
    /// about has changed since, so an old decision or dismissed lead is re-checked rather
    /// than trusted.
    fn note_line(&self, cg: &CodeGraph, n: &ProjectNote) -> String {
        let mut line = n.display_line();
        if n.code_hash.is_empty() {
            return line;
        }
        let at = if n.commit.is_empty() {
            String::new()
        } else {
            format!(" at commit {}", n.commit)
        };
        match self.note_code_hash(cg, &n.target) {
            Some(now) if now == n.code_hash => {}
            Some(_) => line.push_str(&format!(
                " [POSSIBLY OUTDATED: the code changed since this note was written{at}; \
                 re-check before relying on it]"
            )),
            None => line.push_str(&format!(
                " [POSSIBLY OUTDATED: `{}` is no longer found (renamed or removed?); written{at}]",
                n.target
            )),
        }
        line
    }

    /// Whether the code a note was written about is unchanged (true when the note has no
    /// recorded hash: nothing to compare).
    fn note_still_current(&self, cg: &CodeGraph, n: &ProjectNote) -> bool {
        n.code_hash.is_empty()
            || self.note_code_hash(cg, &n.target).as_deref() == Some(&n.code_hash)
    }

    /// 14. Recalls saved notes, decisions and dismissed leads (all of them; no silent cap).
    pub fn recall(&self, query: Option<&str>) -> Result<String> {
        let (db, cg) = self.load_fresh()?;
        let notes = db.list_notes(query)?;
        if notes.is_empty() {
            return Ok(match query {
                Some(q) if !q.trim().is_empty() => {
                    format!("No saved notes match `{q}` (searched target, text, reason and kind).")
                }
                _ => "No saved memory notes yet. Use `remember(target, note)` to store \
                      architectural facts, decisions and dismissed review leads."
                    .to_string(),
            });
        }
        let mut out = format!("# SAVED MEMORY NOTES ({})\n", notes.len());
        for n in notes {
            out.push_str(&format!("- {}\n", self.note_line(&cg, &n)));
        }
        Ok(out)
    }

    /// 15. Runs the project's own test/lint/build/format commands (detected from project
    ///     files) and returns a compact summary that lists every recognized failure and
    ///     never reports an unexplained failure as success.
    pub fn run_checks(
        &self,
        kind: &str,
        extra_args: Option<&str>,
        timeout_secs: u64,
        max_tokens: usize,
    ) -> Result<QueryOutput> {
        use crate::checks::{CheckKind, detect_commands, render, run_command, summarize};
        let start = Instant::now();
        let kinds: Vec<CheckKind> = if kind.trim().eq_ignore_ascii_case("all") {
            vec![CheckKind::Format, CheckKind::Lint, CheckKind::Test]
        } else {
            vec![CheckKind::parse(kind).ok_or_else(|| {
                anyhow::anyhow!("unknown kind `{kind}`: use test, lint, build, format or all")
            })?]
        };
        let extra: Vec<String> = extra_args
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let all = detect_commands(&self.repo_root);
        let selected: Vec<_> = all.iter().filter(|c| kinds.contains(&c.kind)).collect();
        if !extra.is_empty() && selected.len() != 1 {
            anyhow::bail!(
                "extra_args needs exactly one matching command, but kind `{kind}` matches {}: {}",
                selected.len(),
                selected
                    .iter()
                    .map(|c| format!("`{}`", c.display(&[])))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let timeout = std::time::Duration::from_secs(timeout_secs.clamp(1, 3600));

        let mut summary_lines = Vec::new();
        let mut sections = Vec::new();
        let mut raw_tokens = 0usize;
        for k in &kinds {
            let cmds: Vec<_> = selected.iter().filter(|c| c.kind == *k).collect();
            if cmds.is_empty() {
                summary_lines.push(format!(
                    "- {}: NOT DETECTED (checked Cargo.toml, package.json scripts, go.mod, \
                     pytest/ruff config); run it yourself if the project has one",
                    k.as_str()
                ));
                continue;
            }
            for cmd in cmds {
                match run_command(&self.repo_root, cmd, &extra, timeout) {
                    Ok(run) => {
                        raw_tokens += count_tokens(&run.output);
                        let text = render(cmd, &run, &summarize(&run.output));
                        let result = text
                            .lines()
                            .find_map(|l| l.strip_prefix("RESULT: "))
                            .unwrap_or("?")
                            .to_string();
                        summary_lines.push(format!("- {} `{}`: {result}", k.as_str(), run.command));
                        sections.push(text);
                    }
                    Err(e) => summary_lines.push(format!(
                        "- {} `{}`: COULD NOT RUN: {e:#}",
                        k.as_str(),
                        cmd.display(&extra)
                    )),
                }
            }
        }

        let mut out = format!("# RUN_CHECKS ({kind})\n{}\n", summary_lines.join("\n"));
        for s in sections {
            out.push('\n');
            out.push_str(&s);
        }
        let db = Database::open_for_repo(&self.repo_root)?;
        self.finalize(
            &db,
            "run_checks",
            kind,
            out,
            if max_tokens == 0 { 1500 } else { max_tokens },
            raw_tokens,
            0,
            start,
        )
    }

    /// 16. What changed (working tree incl. staged/unstaged/untracked) since a git ref, mapped
    ///     onto symbols, optionally limited to paths or to hunks touching given symbols.
    pub fn changes_since(
        &self,
        git_ref: &str,
        paths: &[String],
        symbols: &[String],
        max_tokens: usize,
    ) -> Result<QueryOutput> {
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;
        let report = crate::changes::changes_since(&self.repo_root, &cg, git_ref, paths, symbols)?;
        self.finalize(
            &db,
            "changes_since",
            git_ref,
            report.text,
            if max_tokens == 0 { 2000 } else { max_tokens },
            report.baseline_tokens,
            report.files,
            start,
        )
    }

    /// Local review of the branch and uncommitted work against `base` (default branch when
    /// empty). Findings dismissed earlier with `remember(target=<id>, kind=dismissed)` are
    /// not shown again; they are listed by id and reason so nothing disappears silently.
    pub fn review_changes(&self, base: &str, max_tokens: usize) -> Result<QueryOutput> {
        let start = Instant::now();
        let base_ref = if base.trim().is_empty() {
            crate::review::default_base(&self.repo_root)?
        } else {
            base.trim().to_string()
        };
        let mut cs = crate::review::review(&self.repo_root, &base_ref)?;
        let (db, cg) = self.load_fresh()?;
        let notes = db.list_notes(None)?;
        let mut dismissed: Vec<(String, String)> = Vec::new();
        // A dismissal holds only while the code it was made about is unchanged; after that
        // the finding is raised again, saying it was dismissed before
        cs.findings.retain_mut(|f| {
            match notes
                .iter()
                .find(|n| n.kind == "dismissed" && n.target.trim().eq_ignore_ascii_case(&f.id))
            {
                Some(n) if self.note_still_current(&cg, n) => {
                    dismissed.push((f.id.clone(), n.reason.clone()));
                    false
                }
                Some(n) => {
                    f.evidence.push(format!(
                        "dismissed earlier (note #{}: {}), but its code changed since: re-check",
                        n.id, n.reason
                    ));
                    true
                }
                None => true,
            }
        });
        let mut text = crate::review::render_markdown(&cs);
        let mut extra = String::new();
        if !dismissed.is_empty() {
            extra.push_str(&format!(
                "Dismissed earlier, not shown again ({}):\n",
                dismissed.len()
            ));
            for (id, reason) in &dismissed {
                extra.push_str(&format!("- `{id}`: {reason}\n"));
            }
        }
        if !cs.findings.is_empty() {
            extra.push_str(
                "After checking a finding that is not a problem: \
                 remember(target=\"<id>\", kind=\"dismissed\", reason=\"...\").\n",
            );
        }
        // Keep this next to the findings, ahead of the long symbol list a budget cuts first
        match text.find("\n## Changed symbols") {
            Some(at) => text.insert_str(at, &format!("\n{extra}")),
            None => text.push_str(&extra),
        }
        let baseline: usize = cs
            .files
            .iter()
            .filter_map(|f| cg.files.get(&f.path))
            .map(|f| f.token_count)
            .sum();
        self.finalize(
            &db,
            "review_changes",
            &base_ref,
            text,
            if max_tokens == 0 { 2500 } else { max_tokens },
            baseline,
            cs.files.len(),
            start,
        )
    }

    /// 17. Build/test/lint/format commands, CI steps, toolchain versions, conventions and
    ///     agent instruction files, each marked detected (declared by the project) or guessed.
    pub fn project_playbook(&self, max_tokens: usize) -> Result<QueryOutput> {
        let start = Instant::now();
        let pb = crate::playbook::build_playbook(&self.repo_root);
        let text = crate::playbook::render(&pb);
        let db = Database::open_for_repo(&self.repo_root)?;
        // Baseline: the files an agent would otherwise open to learn the same things
        let baseline: usize = pb
            .commands
            .iter()
            .chain(&pb.ci_steps)
            .chain(&pb.toolchain)
            .chain(&pb.conventions)
            .map(|i| i.source.split_whitespace().next().unwrap_or("").to_string())
            .collect::<HashSet<String>>()
            .iter()
            .filter_map(|f| std::fs::read_to_string(self.repo_root.join(f)).ok())
            .map(|t| count_tokens(&t))
            .sum();
        self.finalize(
            &db,
            "project_playbook",
            "playbook",
            text,
            if max_tokens == 0 { 1200 } else { max_tokens },
            baseline,
            0,
            start,
        )
    }

    /// 8. Token-savings telemetry summary.
    pub fn token_stats(&self) -> Result<TokenStatsSummary> {
        let db = Database::open_for_repo(&self.repo_root)?;
        db.get_token_stats(20)
    }
}

/// One line telling the agent that a list was cut, so a partial list never looks complete.
fn more_note(total: usize, shown: usize, what: &str) -> String {
    if total > shown {
        format!(
            "(showing {shown} of {total} {what}; {} more not shown)\n",
            total - shown
        )
    } else {
        String::new()
    }
}

/// Indexed extensions, e.g. `.rs .ts .tsx …`, for "not indexed" explanations.
fn indexed_extensions() -> String {
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
    fn path_miss(&self, cg: &CodeGraph, path: &str) -> String {
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
    fn symbol_miss(cg: &CodeGraph, query: &str) -> String {
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
