use crate::db::Database;
use crate::model::{Edge, EdgeKind, FileRecord, Symbol, SymbolKind};
use anyhow::Result;
use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Serialize)]
pub struct CommunitySummary {
    pub id: usize,
    pub label: String,
    pub files: Vec<String>,
    pub symbol_count: usize,
    pub total_tokens: usize,
    pub top_hubs: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScoredNode {
    pub symbol_id: String,
    pub name: String,
    pub file_path: String,
    pub kind: SymbolKind,
    pub score: f64,
    pub in_degree: usize,
    pub out_degree: usize,
}

#[derive(Debug, Clone)]
/// The symbol a query names, whether the match was exact, and the other symbols with the
/// same name (so a tool can say the choice was ambiguous instead of guessing silently).
pub struct Resolution<'a> {
    pub symbol: &'a Symbol,
    pub exact: bool,
    pub others: Vec<&'a Symbol>,
}

impl Resolution<'_> {
    /// One line to print under a lookup's header when the answer is not unambiguous.
    pub fn caveat(&self, query: &str) -> Option<String> {
        if !self.exact {
            return Some(format!(
                "NOTE: no symbol is named exactly `{query}`; this is the closest match \
                 ({}:{}). Check it is the one you meant.",
                self.symbol.file_path, self.symbol.start_line
            ));
        }
        if self.others.is_empty() {
            return None;
        }
        let shown: Vec<String> = self
            .others
            .iter()
            .take(5)
            .map(|s| format!("{}:{}", s.file_path, s.start_line))
            .collect();
        Some(format!(
            "AMBIGUOUS: {} other symbol(s) are also named `{query}`: {}{}. Pass `path::name` \
             to pick one.",
            self.others.len(),
            shown.join(", "),
            if self.others.len() > 5 { ", ..." } else { "" }
        ))
    }
}

#[derive(Debug, Clone)]
pub struct ImpactReport {
    pub root: Symbol,
    pub direct_dependents: Vec<Symbol>,
    pub transitive_dependents: Vec<Symbol>,
    pub related_tests: Vec<Symbol>,
    pub affected_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthReport {
    /// Size outliers `(path, tokens, lines)`, largest first: code files above
    /// `bloat_threshold`.
    pub bloated_files: Vec<(String, usize, usize)>,
    /// The size outliers are measured against (see [`CodeGraph::bloat_threshold`]).
    pub bloat_threshold: usize,
    /// Code files the size distribution covers.
    pub code_files: usize,
    /// Groups of ≥ 2 files that depend on each other in a cycle through code.
    pub circular_groups: Vec<Vec<String>>,
    /// Rust files left out of cycle detection (module cycles in one crate are allowed).
    pub cycle_exempt_files: usize,
    /// Code files nothing in another file refers to.
    pub orphan_files: Vec<String>,
    /// Unreferenced files that are not orphans: entry points and scripts run directly,
    /// `(path, reason)`.
    pub orphan_excluded: Vec<(String, String)>,
}

/// `(hop distance, edge kind, symbol)` entries returned by [`CodeGraph::neighborhood`].
pub type HopList<'a> = Vec<(usize, EdgeKind, &'a Symbol)>;

pub struct CodeGraph {
    pub files: HashMap<String, FileRecord>,
    pub symbols: HashMap<String, Symbol>,
    pub symbols_by_file: HashMap<String, Vec<String>>,
    pub edges: Vec<Edge>,
    pub graph: DiGraph<String, EdgeKind>,
    pub id_to_idx: HashMap<String, NodeIndex>,
    pub pagerank: HashMap<String, f64>,
    pub file_communities: HashMap<String, usize>,
}

impl CodeGraph {
    pub fn load(db: &Database) -> Result<Self> {
        let file_list = db.load_all_files()?;
        let sym_list = db.load_all_symbols()?;
        let edges = db.load_all_edges()?;

        let mut files = HashMap::new();
        for f in file_list {
            files.insert(f.path.clone(), f);
        }

        let mut symbols = HashMap::new();
        let mut symbols_by_file: HashMap<String, Vec<String>> = HashMap::new();
        let mut graph = DiGraph::new();
        let mut id_to_idx = HashMap::new();

        for s in sym_list {
            let idx = graph.add_node(s.id.clone());
            id_to_idx.insert(s.id.clone(), idx);
            symbols_by_file
                .entry(s.file_path.clone())
                .or_default()
                .push(s.id.clone());
            symbols.insert(s.id.clone(), s);
        }

        for e in &edges {
            if let (Some(&src), Some(&tgt)) =
                (id_to_idx.get(&e.source_id), id_to_idx.get(&e.target_id))
            {
                graph.add_edge(src, tgt, e.kind);
            }
        }

        let pagerank = compute_pagerank(&graph, &symbols, 25, 0.85);
        let file_communities = detect_file_communities(&files, &symbols, &edges);

        Ok(Self {
            files,
            symbols,
            symbols_by_file,
            edges,
            graph,
            id_to_idx,
            pagerank,
            file_communities,
        })
    }

    /// Top architectural hubs ("God Nodes") ranked by PageRank + degree.
    pub fn god_nodes(&self, limit: usize, include_files: bool) -> Vec<ScoredNode> {
        let mut scored: Vec<ScoredNode> = self
            .symbols
            .values()
            .filter(|s| {
                (include_files || s.kind != SymbolKind::File)
                    && s.kind != SymbolKind::Impl
                    && s.kind != SymbolKind::Section
            })
            .filter_map(|s| {
                let &idx = self.id_to_idx.get(&s.id)?;
                let in_deg = self
                    .graph
                    .edges_directed(idx, Direction::Incoming)
                    .filter(|e| *e.weight() != EdgeKind::Contains)
                    .count();
                let out_deg = self
                    .graph
                    .edges_directed(idx, Direction::Outgoing)
                    .filter(|e| *e.weight() != EdgeKind::Contains)
                    .count();
                let pr = *self.pagerank.get(&s.id).unwrap_or(&0.0);
                // Blend PageRank with non-containment connectivity
                let score = pr * 100.0 + (in_deg as f64) * 0.35 + (out_deg as f64) * 0.1;
                Some(ScoredNode {
                    symbol_id: s.id.clone(),
                    name: s.qualified_name.clone(),
                    file_path: s.file_path.clone(),
                    kind: s.kind,
                    score,
                    in_degree: in_deg,
                    out_degree: out_deg,
                })
            })
            .collect();

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.name.cmp(&b.name))
        });
        scored.truncate(limit);
        scored
    }

    /// Groups files into detected architectural communities.
    pub fn communities(&self) -> Vec<CommunitySummary> {
        let mut groups: HashMap<usize, Vec<String>> = HashMap::new();
        for (path, &cid) in &self.file_communities {
            groups.entry(cid).or_default().push(path.clone());
        }

        let mut summaries = Vec::new();
        for (cid, mut file_paths) in groups {
            file_paths.sort();
            let total_tokens: usize = file_paths
                .iter()
                .filter_map(|p| self.files.get(p))
                .map(|f| f.token_count)
                .sum();
            let mut sym_count = 0usize;
            let mut hub_candidates: Vec<(String, f64)> = Vec::new();
            for p in &file_paths {
                if let Some(ids) = self.symbols_by_file.get(p) {
                    for id in ids {
                        if let Some(s) = self.symbols.get(id)
                            && s.kind != SymbolKind::File
                            && s.kind != SymbolKind::Impl
                        {
                            sym_count += 1;
                            let pr = *self.pagerank.get(id).unwrap_or(&0.0);
                            hub_candidates.push((s.qualified_name.clone(), pr));
                        }
                    }
                }
            }
            hub_candidates
                .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let top_hubs: Vec<String> =
                hub_candidates.into_iter().take(4).map(|(n, _)| n).collect();

            let label = infer_community_label(&file_paths);
            summaries.push(CommunitySummary {
                id: cid,
                label,
                files: file_paths,
                symbol_count: sym_count,
                total_tokens,
                top_hubs,
            });
        }

        summaries.sort_by_key(|c| std::cmp::Reverse(c.total_tokens));
        for (idx, c) in summaries.iter_mut().enumerate() {
            c.id = idx + 1;
        }
        summaries
    }

    /// Hybrid lexical + structural search for symbols matching `query`.
    pub fn search_symbols(&self, query: &str, limit: usize) -> Vec<(&Symbol, f64)> {
        let q_lower = query.trim().to_ascii_lowercase();
        if q_lower.is_empty() {
            return Vec::new();
        }
        let terms: Vec<&str> = q_lower
            .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
            .filter(|t| !t.is_empty())
            .collect();

        let mut scored: Vec<(&Symbol, f64)> = self
            .symbols
            .values()
            .filter(|s| s.kind != SymbolKind::Impl)
            .filter_map(|s| {
                let name_l = s.name.to_ascii_lowercase();
                let qname_l = s.qualified_name.to_ascii_lowercase();
                let sig_l = s.signature.to_ascii_lowercase();
                let doc_l = s.doc_summary.to_ascii_lowercase();
                let path_l = s.file_path.to_ascii_lowercase();

                let mut score = 0.0f64;
                if name_l == q_lower || qname_l == q_lower {
                    score += 100.0;
                } else if name_l.starts_with(&q_lower) {
                    score += 55.0;
                } else if name_l.contains(&q_lower) || qname_l.contains(&q_lower) {
                    score += 35.0;
                }

                for term in &terms {
                    if name_l == *term {
                        score += 30.0;
                    } else if name_l.contains(term) {
                        score += 16.0;
                    }
                    if qname_l.contains(term) {
                        score += 10.0;
                    }
                    if sig_l.contains(term) {
                        score += 7.0;
                    }
                    if doc_l.contains(term) {
                        score += 6.0;
                    }
                    if path_l.contains(term) {
                        score += 5.0;
                    }
                }

                if score <= 0.0 {
                    return None;
                }

                // Prefer substantive code symbols over file/section/module wrappers
                match s.kind {
                    SymbolKind::File => score *= 0.65,
                    SymbolKind::Section | SymbolKind::Dependency => score *= 0.55,
                    SymbolKind::Module if s.start_line == s.end_line => score *= 0.60,
                    _ => {}
                }
                // Boost by PageRank centrality
                let pr = *self.pagerank.get(&s.id).unwrap_or(&0.0);
                score += pr * 25.0;
                Some((s, score))
            })
            .collect();

        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.name.cmp(&b.0.name))
        });
        scored.truncate(limit);
        scored
    }

    /// Resolves a user/AI symbol specifier (exact ID, qualified name, or best search match).
    pub fn resolve_symbol(&self, query: &str) -> Option<&Symbol> {
        self.resolve(query).map(|r| r.symbol)
    }

    /// Resolves a symbol query, most precise form first:
    /// 1. a symbol id;
    /// 2. `path::name` (a file of the index, then a qualified or plain name inside it);
    /// 3. an exact qualified name, then an exact name (the highest-ranked one when several
    ///    share it; the others are returned so the caller can say so);
    /// 4. otherwise the best fuzzy match, marked `exact: false`.
    pub fn resolve(&self, query: &str) -> Option<Resolution<'_>> {
        let q = query.trim();
        if let Some(s) = self.symbols.get(q) {
            return Some(Resolution {
                symbol: s,
                exact: true,
                others: Vec::new(),
            });
        }
        // `path::name`: the longest prefix that is an indexed file
        let norm = q.replace('\\', "/");
        let mut cuts: Vec<usize> = norm.match_indices("::").map(|(i, _)| i).collect();
        cuts.reverse();
        for i in cuts {
            let (path, rest) = (&norm[..i], &norm[i + 2..]);
            if let Some(ids) = self.symbols_by_file.get(path) {
                let in_file: Vec<&Symbol> = ids
                    .iter()
                    .filter_map(|id| self.symbols.get(id))
                    .filter(|s| s.kind != SymbolKind::File)
                    .collect();
                let by_qname: Vec<&Symbol> = in_file
                    .iter()
                    .copied()
                    .filter(|s| s.qualified_name == rest)
                    .collect();
                if !by_qname.is_empty() {
                    return self.pick_ranked(by_qname);
                }
                return self.pick_ranked(in_file.into_iter().filter(|s| s.name == rest).collect());
            }
        }
        let code = |s: &&Symbol| s.kind != SymbolKind::File;
        let by_qname: Vec<&Symbol> = self
            .symbols
            .values()
            .filter(code)
            .filter(|s| s.qualified_name == q)
            .collect();
        if !by_qname.is_empty() {
            return self.pick_ranked(by_qname);
        }
        let by_name: Vec<&Symbol> = self
            .symbols
            .values()
            .filter(code)
            .filter(|s| s.name == q)
            .collect();
        if !by_name.is_empty() {
            return self.pick_ranked(by_name);
        }
        self.search_symbols(q, 1)
            .into_iter()
            .next()
            .map(|(s, _)| Resolution {
                symbol: s,
                exact: false,
                others: Vec::new(),
            })
    }

    /// The highest-ranked of several exact matches; the rest become `others`.
    fn pick_ranked<'a>(&'a self, mut found: Vec<&'a Symbol>) -> Option<Resolution<'a>> {
        found.sort_by_key(|s| {
            (
                std::cmp::Reverse((self.pagerank.get(&s.id).copied().unwrap_or(0.0) * 1e9) as u64),
                s.file_path.clone(),
                s.start_line,
            )
        });
        let mut it = found.into_iter();
        it.next().map(|symbol| Resolution {
            symbol,
            exact: true,
            others: it.collect(),
        })
    }

    /// Like `resolve`, but never a fuzzy guess.
    pub fn resolve_exact(&self, query: &str) -> Option<&Symbol> {
        self.resolve(query).filter(|r| r.exact).map(|r| r.symbol)
    }

    /// Returns direct callers and callees (up to `depth` hops) of a symbol.
    pub fn neighborhood(&self, symbol_id: &str, depth: usize) -> (HopList<'_>, HopList<'_>) {
        let Some(&start_idx) = self.id_to_idx.get(symbol_id) else {
            return (Vec::new(), Vec::new());
        };
        let max_depth = depth.clamp(1, 4);

        let callers = bfs_neighbors(self, start_idx, Direction::Incoming, max_depth);
        let callees = bfs_neighbors(self, start_idx, Direction::Outgoing, max_depth);
        (callers, callees)
    }

    /// Computes blast radius (who depends on this symbol + linked tests).
    pub fn impact_analysis(&self, symbol_id: &str, max_depth: usize) -> Option<ImpactReport> {
        let root = self.symbols.get(symbol_id)?.clone();
        let &start_idx = self.id_to_idx.get(symbol_id)?;
        let depth = max_depth.clamp(1, 4);

        let incoming = bfs_neighbors(self, start_idx, Direction::Incoming, depth);
        let mut direct_dependents = Vec::new();
        let mut transitive_dependents = Vec::new();
        let mut related_tests = Vec::new();
        let mut files_set: HashSet<String> = HashSet::new();
        files_set.insert(root.file_path.clone());

        for (d, kind, sym) in incoming {
            files_set.insert(sym.file_path.clone());
            if sym.kind == SymbolKind::Test || kind == EdgeKind::Tests {
                related_tests.push(sym.clone());
            } else if d == 1 {
                direct_dependents.push(sym.clone());
            } else {
                transitive_dependents.push(sym.clone());
            }
        }

        let mut affected_files: Vec<String> = files_set.into_iter().collect();
        affected_files.sort();

        Some(ImpactReport {
            root,
            direct_dependents,
            transitive_dependents,
            related_tests,
            affected_files,
        })
    }

    /// Finds shortest directed (or undirected fallback) path between two symbols.
    pub fn shortest_path(&self, from_id: &str, to_id: &str) -> Option<Vec<&Symbol>> {
        let &start = self.id_to_idx.get(from_id)?;
        let &goal = self.id_to_idx.get(to_id)?;

        // Try directed BFS first, then undirected if no directed path exists
        let path_indices = bfs_path(&self.graph, start, goal, true)
            .or_else(|| bfs_path(&self.graph, start, goal, false))?;

        let syms = path_indices
            .into_iter()
            .filter_map(|idx| self.symbols.get(&self.graph[idx]))
            .collect();
        Some(syms)
    }

    /// File-to-file dependencies through code only (calls, type uses, impls, imports)
    /// between code files: doc mentions and co-change history are not dependencies. Rust
    /// files are left out: modules of one crate depend on each other freely and such cycles
    /// are not a problem there, unlike import cycles in JS/TS or Python. Each pair keeps one
    /// example `path:line source symbol -> path:line target symbol`. Shared by
    /// [`CodeGraph::health_report`] and the review's `new-cycle` rule.
    pub fn code_file_edges(&self) -> BTreeMap<(String, String), String> {
        let mut out = BTreeMap::new();
        for e in &self.edges {
            if !matches!(
                e.kind,
                EdgeKind::Calls | EdgeKind::UsesType | EdgeKind::Implements | EdgeKind::Imports
            ) {
                continue;
            }
            let (Some(s), Some(t)) = (
                self.symbols.get(&e.source_id),
                self.symbols.get(&e.target_id),
            ) else {
                continue;
            };
            if s.file_path == t.file_path
                || !self.in_cycle_scope(&s.file_path)
                || !self.in_cycle_scope(&t.file_path)
            {
                continue;
            }
            out.entry((s.file_path.clone(), t.file_path.clone()))
                .or_insert_with(|| {
                    format!(
                        "{}:{} {} -> {}:{} {}",
                        s.file_path,
                        s.start_line,
                        s.qualified_name,
                        t.file_path,
                        t.start_line,
                        t.qualified_name
                    )
                });
        }
        out
    }

    /// Whether cycle detection looks at this file: code in a language whose import cycles
    /// are a problem (see [`CodeGraph::code_file_edges`]).
    fn in_cycle_scope(&self, path: &str) -> bool {
        self.files
            .get(path)
            .is_some_and(|f| !CYCLE_EXEMPT_LANGUAGES.contains(&f.language.as_str()))
    }

    /// The size above which a code file counts as bloated in this repository: the upper
    /// Tukey fence of code-file sizes (Q3 + 1.5 × IQR), never below [`BLOAT_FLOOR_TOKENS`].
    /// A fixed threshold flags half of a repository whose files are all fairly large, which
    /// makes the warning useless; the fence flags only the files that stand out from the
    /// rest, and the floor keeps files an agent still reads whole in one read off the list.
    pub fn bloat_threshold(&self) -> usize {
        let mut sizes: Vec<usize> = self
            .files
            .values()
            .filter(|f| !NOT_CODE_LANGUAGES.contains(&f.language.as_str()))
            .map(|f| f.token_count)
            .collect();
        sizes.sort_unstable();
        tukey_upper_fence(&sizes).map_or(BLOAT_FLOOR_TOKENS, |f| f.max(BLOAT_FLOOR_TOKENS))
    }

    /// Computes AI-focused repository health diagnostics:
    /// - bloated files: size outliers among this repository's code files, largest first
    ///   (see [`CodeGraph::bloat_threshold`]);
    /// - circular file dependencies through code (Tarjan SCC over
    ///   [`CodeGraph::code_file_edges`]; Rust module cycles are allowed and not reported);
    /// - orphan code files with no incoming reference, leaving out entry points and scripts
    ///   that are run directly (listed in `orphan_excluded` with the reason).
    pub fn health_report(&self) -> HealthReport {
        let is_code = |f: &&FileRecord| !NOT_CODE_LANGUAGES.contains(&f.language.as_str());

        // 1. Size outliers, largest first
        let code_files = self.files.values().filter(is_code).count();
        let bloat_threshold = self.bloat_threshold();
        let mut bloated_files: Vec<(String, usize, usize)> = self
            .files
            .values()
            .filter(is_code)
            .filter(|f| f.token_count > bloat_threshold)
            .map(|f| (f.path.clone(), f.token_count, f.line_count))
            .collect();
        bloated_files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        // 2. Cycles through code, in languages where they matter
        let mut circular_groups: Vec<Vec<String>> = file_cycles(&self.code_file_edges())
            .into_iter()
            .map(|c| c.into_iter().collect())
            .collect();
        circular_groups.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        let cycle_exempt_files = self.files.values().filter(|f| f.language == "rust").count();

        // 3. Orphan code files: nothing in another file refers to them
        let mut has_incoming: HashSet<&str> = HashSet::new();
        for e in &self.edges {
            if matches!(e.kind, EdgeKind::Contains | EdgeKind::CoChanges) {
                continue;
            }
            if let (Some(s), Some(t)) = (
                self.symbols.get(&e.source_id),
                self.symbols.get(&e.target_id),
            ) && s.file_path != t.file_path
            {
                has_incoming.insert(t.file_path.as_str());
            }
        }
        let mut orphan_files = Vec::new();
        let mut orphan_excluded = Vec::new();
        for f in self.files.values().filter(is_code) {
            if has_incoming.contains(f.path.as_str()) {
                continue;
            }
            match entry_point_reason(&f.path) {
                Some(reason) => orphan_excluded.push((f.path.clone(), reason.to_string())),
                None => orphan_files.push(f.path.clone()),
            }
        }
        orphan_files.sort();
        orphan_excluded.sort();

        HealthReport {
            bloated_files,
            bloat_threshold,
            code_files,
            circular_groups,
            cycle_exempt_files,
            orphan_files,
            orphan_excluded,
        }
    }
}

/// Languages that are not code: never bloat outliers or orphans.
const NOT_CODE_LANGUAGES: &[&str] = &["markdown", "toml", "json"];

/// Languages whose files are left out of cycle detection: Rust modules of one crate may
/// depend on each other in a cycle, and markup/data files have no imports that matter.
const CYCLE_EXEMPT_LANGUAGES: &[&str] = &["markdown", "toml", "json", "rust", "html", "css"];

/// A file at or under this size is never reported as bloated, whatever the distribution:
/// about 800 lines, which an agent still reads whole in one ordinary file read.
pub const BLOAT_FLOOR_TOKENS: usize = 8_000;

/// Upper Tukey fence `Q3 + 1.5 × IQR` of sorted sizes (quartiles by linear
/// interpolation); `None` below 4 values, where quartiles say nothing.
fn tukey_upper_fence(sorted: &[usize]) -> Option<usize> {
    if sorted.len() < 4 {
        return None;
    }
    let q = |p: f64| {
        let pos = p * (sorted.len() - 1) as f64;
        let lo = pos.floor() as usize;
        let hi = pos.ceil() as usize;
        sorted[lo] as f64 + (sorted[hi] as f64 - sorted[lo] as f64) * (pos - lo as f64)
    };
    let (q1, q3) = (q(0.25), q(0.75));
    Some((q3 + 1.5 * (q3 - q1)).round() as usize)
}

/// Why a file with no incoming reference is still not an orphan: it is an entry point or a
/// script that is run directly (by a user, a build tool, CI or a test runner).
fn entry_point_reason(path: &str) -> Option<&'static str> {
    let base = path.rsplit('/').next().unwrap_or(path);
    let ext = base.rsplit_once('.').map_or("", |(_, e)| e);
    let under =
        |dir: &str| path.starts_with(&format!("{dir}/")) || path.contains(&format!("/{dir}/"));
    if under("tests") || under("benches") || under("examples") {
        Some("test, bench or example target")
    } else if under(".github") || under("ci") {
        Some("CI script")
    } else if under("scripts")
        || under("bin")
        || matches!(ext, "ps1" | "sh" | "bash" | "bat" | "cmd")
    {
        Some("script run directly")
    } else if base == "build.rs" {
        Some("build script")
    } else if matches!(
        base,
        "lib.rs"
            | "mod.rs"
            | "index.html"
            | "__main__.py"
            | "__init__.py"
            | "setup.py"
            | "manage.py"
            | "conftest.py"
    ) || base.starts_with("main.")
    {
        Some("entry point or module root")
    } else {
        None
    }
}

/// Groups of files that depend on each other in a cycle (strongly connected, ≥ 2 files).
pub fn file_cycles(edges: &BTreeMap<(String, String), String>) -> Vec<BTreeSet<String>> {
    let mut g: DiGraph<String, ()> = DiGraph::new();
    let mut idx: BTreeMap<String, NodeIndex> = BTreeMap::new();
    for (a, b) in edges.keys() {
        for p in [a, b] {
            if !idx.contains_key(p) {
                idx.insert(p.clone(), g.add_node(p.clone()));
            }
        }
        g.add_edge(idx[a], idx[b], ());
    }
    petgraph::algo::tarjan_scc(&g)
        .into_iter()
        .filter(|scc| scc.len() >= 2)
        .map(|scc| scc.into_iter().map(|i| g[i].clone()).collect())
        .collect()
}

fn bfs_neighbors(
    cg: &CodeGraph,
    start: NodeIndex,
    dir: Direction,
    max_depth: usize,
) -> Vec<(usize, EdgeKind, &Symbol)> {
    let mut visited = HashSet::new();
    visited.insert(start);
    let mut queue = VecDeque::new();
    queue.push_back((start, 0usize));
    let mut out = Vec::new();

    while let Some((curr, dist)) = queue.pop_front() {
        if dist >= max_depth {
            continue;
        }
        for edge in cg.graph.edges_directed(curr, dir) {
            let ek = *edge.weight();
            if ek == EdgeKind::Contains {
                continue;
            }
            let next = match dir {
                Direction::Outgoing => edge.target(),
                Direction::Incoming => edge.source(),
            };
            if visited.insert(next) {
                if let Some(sym) = cg.symbols.get(&cg.graph[next]) {
                    out.push((dist + 1, ek, sym));
                }
                queue.push_back((next, dist + 1));
            }
        }
    }
    out
}

fn bfs_path(
    graph: &DiGraph<String, EdgeKind>,
    start: NodeIndex,
    goal: NodeIndex,
    directed_only: bool,
) -> Option<Vec<NodeIndex>> {
    let mut visited = HashSet::new();
    let mut parent: HashMap<NodeIndex, NodeIndex> = HashMap::new();
    let mut queue = VecDeque::new();
    visited.insert(start);
    queue.push_back(start);

    while let Some(curr) = queue.pop_front() {
        if curr == goal {
            let mut path = vec![goal];
            let mut cur = goal;
            while let Some(&p) = parent.get(&cur) {
                path.push(p);
                cur = p;
            }
            path.reverse();
            return Some(path);
        }

        let mut neighbors = Vec::new();
        for e in graph.edges_directed(curr, Direction::Outgoing) {
            if *e.weight() != EdgeKind::Contains {
                neighbors.push(e.target());
            }
        }
        if !directed_only {
            for e in graph.edges_directed(curr, Direction::Incoming) {
                if *e.weight() != EdgeKind::Contains {
                    neighbors.push(e.source());
                }
            }
        }
        for next in neighbors {
            if visited.insert(next) {
                parent.insert(next, curr);
                queue.push_back(next);
            }
        }
    }
    None
}

fn compute_pagerank(
    graph: &DiGraph<String, EdgeKind>,
    symbols: &HashMap<String, Symbol>,
    iterations: usize,
    damping: f64,
) -> HashMap<String, f64> {
    let n = graph.node_count();
    if n == 0 {
        return HashMap::new();
    }
    let init = 1.0 / n as f64;
    let mut ranks = vec![init; n];
    let mut next = vec![0.0; n];

    // Precompute non-containment outgoing degrees
    let out_deg: Vec<usize> = (0..n)
        .map(|i| {
            graph
                .edges_directed(NodeIndex::new(i), Direction::Outgoing)
                .filter(|e| *e.weight() != EdgeKind::Contains)
                .count()
        })
        .collect();

    for _ in 0..iterations {
        let base = (1.0 - damping) / n as f64;
        next.fill(base);

        for i in 0..n {
            let deg = out_deg[i];
            if deg == 0 {
                continue;
            }
            let share = damping * ranks[i] / deg as f64;
            for e in graph.edges_directed(NodeIndex::new(i), Direction::Outgoing) {
                if *e.weight() != EdgeKind::Contains {
                    next[e.target().index()] += share;
                }
            }
        }
        std::mem::swap(&mut ranks, &mut next);
    }

    let mut out = HashMap::with_capacity(n);
    for i in 0..n {
        let id = &graph[NodeIndex::new(i)];
        if symbols.contains_key(id) {
            out.insert(id.clone(), ranks[i]);
        }
    }
    out
}

/// Deterministic Label Propagation on the file-level dependency projection.
fn detect_file_communities(
    files: &HashMap<String, FileRecord>,
    symbols: &HashMap<String, Symbol>,
    edges: &[Edge],
) -> HashMap<String, usize> {
    let mut paths: Vec<String> = files.keys().cloned().collect();
    paths.sort();

    let mut label_of: HashMap<String, usize> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| (p.clone(), i))
        .collect();

    // Build weighted undirected adjacency between files
    let mut adj: HashMap<String, HashMap<String, f32>> = HashMap::new();
    for e in edges {
        if e.kind == EdgeKind::Contains {
            continue;
        }
        let (Some(src), Some(tgt)) = (symbols.get(&e.source_id), symbols.get(&e.target_id)) else {
            continue;
        };
        if src.file_path == tgt.file_path {
            continue;
        }
        *adj.entry(src.file_path.clone())
            .or_default()
            .entry(tgt.file_path.clone())
            .or_insert(0.0) += e.weight;
        *adj.entry(tgt.file_path.clone())
            .or_default()
            .entry(src.file_path.clone())
            .or_insert(0.0) += e.weight;
    }

    // Also add light directory affinity so isolated files in the same folder cluster naturally
    for i in 0..paths.len() {
        for j in (i + 1)..paths.len() {
            let d1 = paths[i].rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            let d2 = paths[j].rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            if !d1.is_empty() && d1 == d2 {
                *adj.entry(paths[i].clone())
                    .or_default()
                    .entry(paths[j].clone())
                    .or_insert(0.0) += 0.4;
                *adj.entry(paths[j].clone())
                    .or_default()
                    .entry(paths[i].clone())
                    .or_insert(0.0) += 0.4;
            }
        }
    }

    // Iterate label propagation
    for _ in 0..12 {
        let mut changed = false;
        for p in &paths {
            let Some(neighbors) = adj.get(p) else {
                continue;
            };
            let mut scores: HashMap<usize, f32> = HashMap::new();
            for (nbr, &w) in neighbors {
                if let Some(&lbl) = label_of.get(nbr) {
                    *scores.entry(lbl).or_insert(0.0) += w;
                }
            }
            if let Some((&best_lbl, _)) = scores.iter().max_by(|a, b| {
                a.1.partial_cmp(b.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.0.cmp(a.0))
            }) && label_of.get(p) != Some(&best_lbl)
            {
                label_of.insert(p.clone(), best_lbl);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    label_of
}

pub(crate) fn infer_community_label(files: &[String]) -> String {
    if files.len() == 1 {
        return files[0].clone();
    }
    let mut dir_counts: HashMap<&str, usize> = HashMap::new();
    for f in files {
        let dir = f.rsplit_once('/').map(|(d, _)| d).unwrap_or("root");
        *dir_counts.entry(dir).or_insert(0) += 1;
    }
    // Ties are broken by the lexicographically smallest directory so the label is the same on
    // every run (HashMap iteration order alone would make it random).
    dir_counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(d, _)| d.to_string())
        .unwrap_or_else(|| "cluster".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tukey_fence_flags_only_outliers() {
        assert_eq!(tukey_upper_fence(&[1, 2, 3]), None);
        // Q1 = 2, Q3 = 4 (linear interpolation over 1..=5), IQR = 2: fence = 4 + 3 = 7
        assert_eq!(tukey_upper_fence(&[1, 2, 3, 4, 5]), Some(7));
        assert_eq!(tukey_upper_fence(&[10, 10, 10, 10]), Some(10));
    }

    #[test]
    fn scripts_and_entry_points_are_not_orphans() {
        for (path, reason) in [
            ("scripts/install.ps1", "script run directly"),
            ("tools/setup.sh", "script run directly"),
            ("src/bin/cli.rs", "script run directly"),
            (".github/scripts/release.py", "CI script"),
            ("build.rs", "build script"),
            ("tests/api.rs", "test, bench or example target"),
            ("pkg/__main__.py", "entry point or module root"),
            ("cmd/main.go", "entry point or module root"),
        ] {
            assert_eq!(entry_point_reason(path), Some(reason), "{path}");
        }
        for path in [
            "src/util.rs",
            "lib/helpers.py",
            "web/app.ts",
            "src/binary.rs",
        ] {
            assert_eq!(entry_point_reason(path), None, "{path}");
        }
    }
}
