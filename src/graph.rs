use crate::db::Database;
use crate::model::{Edge, EdgeKind, FileRecord, Symbol, SymbolKind};
use anyhow::Result;
use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};

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
pub struct ImpactReport {
    pub root: Symbol,
    pub direct_dependents: Vec<Symbol>,
    pub transitive_dependents: Vec<Symbol>,
    pub related_tests: Vec<Symbol>,
    pub affected_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthReport {
    pub bloated_files: Vec<(String, usize, usize)>, // (path, tokens, lines)
    pub circular_groups: Vec<Vec<String>>,          // strongly connected file cycles (>= 2 files)
    pub orphan_files: Vec<String>,                  // code files with 0 incoming non-containment edges
}

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
                        if let Some(s) = self.symbols.get(id) {
                            if s.kind != SymbolKind::File && s.kind != SymbolKind::Impl {
                                sym_count += 1;
                                let pr = *self.pagerank.get(id).unwrap_or(&0.0);
                                hub_candidates.push((s.qualified_name.clone(), pr));
                            }
                        }
                    }
                }
            }
            hub_candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let top_hubs: Vec<String> = hub_candidates
                .into_iter()
                .take(4)
                .map(|(n, _)| n)
                .collect();

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

        summaries.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens));
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
        if let Some(s) = self.symbols.get(query) {
            return Some(s);
        }
        self.search_symbols(query, 1).into_iter().next().map(|(s, _)| s)
    }

    /// Returns direct callers and callees (up to `depth` hops) of a symbol.
    pub fn neighborhood(
        &self,
        symbol_id: &str,
        depth: usize,
    ) -> (Vec<(usize, EdgeKind, &Symbol)>, Vec<(usize, EdgeKind, &Symbol)>) {
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

    /// Computes AI-focused repository health diagnostics:
    /// - Bloated files (> 3,500 tokens) that hurt LLM context efficiency
    /// - Circular file dependency cycles (via Tarjan SCC)
    /// - Orphan code files with 0 incoming references
    pub fn health_report(&self) -> HealthReport {
        // 1. Token-bloat files
        let mut bloated_files: Vec<(String, usize, usize)> = self
            .files
            .values()
            .filter(|f| f.token_count >= 3500)
            .map(|f| (f.path.clone(), f.token_count, f.line_count))
            .collect();
        bloated_files.sort_by(|a, b| b.1.cmp(&a.1));

        // 2. Build directed file-level dependency graph (excluding CoChanges & Contains)
        let mut file_graph: DiGraph<String, ()> = DiGraph::new();
        let mut path_to_idx: HashMap<String, NodeIndex> = HashMap::new();
        let mut sorted_paths: Vec<String> = self.files.keys().cloned().collect();
        sorted_paths.sort();
        for p in &sorted_paths {
            let idx = file_graph.add_node(p.clone());
            path_to_idx.insert(p.clone(), idx);
        }
        let mut seen_file_edges: HashSet<(NodeIndex, NodeIndex)> = HashSet::new();
        let mut has_incoming: HashSet<String> = HashSet::new();

        for e in &self.edges {
            if matches!(e.kind, EdgeKind::Contains | EdgeKind::CoChanges) {
                continue;
            }
            let (Some(src), Some(tgt)) =
                (self.symbols.get(&e.source_id), self.symbols.get(&e.target_id))
            else {
                continue;
            };
            if src.file_path == tgt.file_path {
                continue;
            }
            has_incoming.insert(tgt.file_path.clone());
            if let (Some(&si), Some(&ti)) =
                (path_to_idx.get(&src.file_path), path_to_idx.get(&tgt.file_path))
            {
                if seen_file_edges.insert((si, ti)) {
                    file_graph.add_edge(si, ti, ());
                }
            }
        }

        // Strongly connected components of size >= 2 are real circular dependency cycles
        let mut circular_groups: Vec<Vec<String>> = petgraph::algo::tarjan_scc(&file_graph)
            .into_iter()
            .filter(|scc| scc.len() >= 2)
            .map(|scc| {
                let mut names: Vec<String> =
                    scc.into_iter().map(|i| file_graph[i].clone()).collect();
                names.sort();
                names
            })
            .collect();
        circular_groups.sort_by(|a, b| b.len().cmp(&a.len()));

        // 3. Orphan code files (excluding obvious entry points, tests, docs, configs)
        let mut orphan_files: Vec<String> = self
            .files
            .values()
            .filter(|f| !matches!(f.language.as_str(), "markdown" | "toml"))
            .filter(|f| !has_incoming.contains(&f.path))
            .filter(|f| {
                let base = f.path.rsplit('/').next().unwrap_or(&f.path);
                !matches!(
                    base,
                    "main.rs" | "lib.rs" | "mod.rs" | "build.rs" | "index.html"
                ) && !f.path.starts_with("tests/")
                    && !f.path.starts_with("examples/")
                    && !f.path.contains("/tests/")
            })
            .map(|f| f.path.clone())
            .collect();
        orphan_files.sort();

        HealthReport {
            bloated_files,
            circular_groups,
            orphan_files,
        }
    }
}

fn bfs_neighbors<'a>(
    cg: &'a CodeGraph,
    start: NodeIndex,
    dir: Direction,
    max_depth: usize,
) -> Vec<(usize, EdgeKind, &'a Symbol)> {
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
            }) {
                if label_of.get(p) != Some(&best_lbl) {
                    label_of.insert(p.clone(), best_lbl);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    label_of
}

fn infer_community_label(files: &[String]) -> String {
    if files.len() == 1 {
        return files[0].clone();
    }
    let mut dir_counts: HashMap<&str, usize> = HashMap::new();
    for f in files {
        let dir = f.rsplit_once('/').map(|(d, _)| d).unwrap_or("root");
        *dir_counts.entry(dir).or_insert(0) += 1;
    }
    dir_counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(d, _)| d.to_string())
        .unwrap_or_else(|| "cluster".to_string())
}
