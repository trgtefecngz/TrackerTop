//! Graph analysis tools: call_graph, impact_analysis, task_context, shortest_path, diff_impact and health_check.

use super::*;

impl TrackerEngine {
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
        if let Some(c) = cg
            .resolve(symbol_query)
            .and_then(|r| r.caveat(symbol_query))
        {
            out.push_str(&format!("{c}\n"));
        }
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
        if let Some(c) = cg
            .resolve(symbol_query)
            .and_then(|r| r.caveat(symbol_query))
        {
            out.push_str(&format!("{c}\n"));
        }

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
            "# CODE HEALTH CHECK\nOversized files (>{} tok): {} | Circular clusters: {} | Orphan code files: {}\n\
             Coverage: sizes of {} code files; cycles through code in languages with import \
             cycles ({} Rust files left out: module cycles inside one crate are allowed); \
             orphans leave out {} entry points and scripts run directly.\n",
            hr.bloat_threshold,
            hr.bloated_files.len(),
            hr.circular_groups.len(),
            hr.orphan_files.len(),
            hr.code_files,
            hr.cycle_exempt_files,
            hr.orphan_excluded.len()
        );

        if !hr.bloated_files.is_empty() {
            out.push_str(&format!(
                "\n## Oversized Files (size outliers: above Q3 + 1.5×IQR of this repo's code \
                 files, at least {} tok; consider splitting)\n",
                crate::graph::BLOAT_FLOOR_TOKENS
            ));
            out.push_str(&more_note(hr.bloated_files.len(), 15, "oversized files"));
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
        if !hr.orphan_excluded.is_empty() {
            out.push_str("\n## Unreferenced but not orphans (entry points, scripts)\n");
            out.push_str(&more_note(hr.orphan_excluded.len(), 10, "excluded files"));
            for (p, why) in hr.orphan_excluded.iter().take(10) {
                out.push_str(&format!("- `{p}` — {why}\n"));
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
}
