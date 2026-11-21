//! "Matryoshka" nested graph: project → communities → files → symbols, one level at a time.
//!
//! Each level shows only the children of the current scope. Edges are aggregated from the
//! symbol-level dependency edges (`Contains` excluded), so an edge between two communities
//! carries the number of underlying symbol edges between them. Nothing is silently dropped:
//! every view reports how many edges leave its scope (`external_edges`), how many internal
//! edges are not drawn (`hidden_edges`), and how many children were cut by the node cap
//! (`truncated`).
//!
//! Group ids are stable across runs and re-indexes: a community is identified by the
//! lexicographically smallest file path it contains (`c:<path>`), not by a numeric label
//! that shifts whenever files are added. A file is `f:<path>`, a symbol `s:<symbol id>`.

use crate::graph::{CodeGraph, infer_community_label};
use crate::model::{Edge, EdgeKind, FileRecord, Symbol, SymbolKind};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, HashMap};

/// Maximum number of child nodes returned for one level. Larger scopes are truncated and the
/// view says by how much.
pub const MAX_LEVEL_NODES: usize = 400;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NestedLevel {
    Project,
    /// Stable community id (`c:<anchor path>`).
    Community(String),
    /// File path.
    File(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NestedKind {
    Community,
    File,
    Symbol,
}

#[derive(Debug, Clone)]
pub struct NestedNode {
    /// Stable id: `c:<anchor path>`, `f:<path>` or `s:<symbol id>`.
    pub id: String,
    pub label: String,
    pub kind: NestedKind,
    /// Communities: files inside; files: symbols inside; symbols: 0.
    pub child_count: usize,
    pub tokens: usize,
    /// `path:line` for files and symbols, so a claim can be verified.
    pub location: Option<String>,
    /// Language for files, symbol kind for symbols, empty for communities.
    pub detail: String,
    /// The level reached by drilling into this node (`None` for symbols).
    pub drill: Option<NestedLevel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NestedEdge {
    pub src: usize,
    pub tgt: usize,
    /// Number of underlying symbol-level edges aggregated into this one.
    pub weight: usize,
}

#[derive(Debug, Clone)]
pub struct NestedView {
    pub level: NestedLevel,
    /// Path from the project root to this level: `(label, level)`.
    pub breadcrumbs: Vec<(String, NestedLevel)>,
    pub nodes: Vec<NestedNode>,
    pub edges: Vec<NestedEdge>,
    /// Symbol edges with exactly one endpoint inside this scope (not drawn at this level).
    pub external_edges: usize,
    /// `external_edges` by where they lead: other communities (community level) or other
    /// files (file level); largest first. The counts sum to `external_edges`.
    pub external: Vec<ExternalGroup>,
    /// Symbol edges inside this scope that are not drawn: both endpoints map to the same
    /// node, or an endpoint is a file-scope entry or was cut by the node cap.
    pub hidden_edges: usize,
    /// Children not shown because of [`MAX_LEVEL_NODES`].
    pub truncated: usize,
}

/// Edges leaving a scope toward one outside group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalGroup {
    /// Community label, or file path at file level; `(not indexed)` when the far end's file
    /// has no community.
    pub label: String,
    /// The level that opens the far end (`None` when it cannot be opened).
    pub level: Option<NestedLevel>,
    pub count: usize,
    /// Children of this view that carry these edges: `(node index, edges)`. Edges whose
    /// inside end is not a shown child (file-scope entry, cut by the cap) are only in `count`.
    pub from: Vec<(usize, usize)>,
}

enum Scope<'p> {
    Project,
    Community(usize),
    File(&'p String),
}

struct Community {
    id: String,
    label: String,
    files: Vec<String>,
}

/// Precomputed grouping used to answer [`NestedGraph::view`] queries.
pub struct NestedGraph<'a> {
    files: &'a HashMap<String, FileRecord>,
    symbols: &'a HashMap<String, Symbol>,
    edges: &'a [Edge],
    communities: Vec<Community>,
    community_of_file: HashMap<String, usize>,
    symbols_by_file: BTreeMap<String, Vec<&'a Symbol>>,
}

impl<'a> NestedGraph<'a> {
    pub fn new(cg: &'a CodeGraph) -> Self {
        Self::from_parts(&cg.files, &cg.symbols, &cg.edges, &cg.file_communities)
    }

    /// Builds the nesting from raw parts. `file_communities` maps a file path to any
    /// community label; files missing from it become their own community.
    pub fn from_parts(
        files: &'a HashMap<String, FileRecord>,
        symbols: &'a HashMap<String, Symbol>,
        edges: &'a [Edge],
        file_communities: &HashMap<String, usize>,
    ) -> Self {
        let mut raw: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for path in files.keys() {
            let key = match file_communities.get(path) {
                Some(l) => format!("l{l}"),
                None => format!("p{path}"),
            };
            raw.entry(key).or_default().push(path.clone());
        }
        let mut communities: Vec<Community> = raw
            .into_values()
            .map(|mut fs| {
                fs.sort();
                Community {
                    id: format!("c:{}", fs[0]),
                    label: infer_community_label(&fs),
                    files: fs,
                }
            })
            .collect();
        communities.sort_by(|a, b| a.id.cmp(&b.id));

        let mut community_of_file = HashMap::new();
        for (i, c) in communities.iter().enumerate() {
            for f in &c.files {
                community_of_file.insert(f.clone(), i);
            }
        }

        let mut symbols_by_file: BTreeMap<String, Vec<&Symbol>> = BTreeMap::new();
        for s in symbols.values() {
            symbols_by_file
                .entry(s.file_path.clone())
                .or_default()
                .push(s);
        }
        for list in symbols_by_file.values_mut() {
            list.sort_by(|a, b| a.start_line.cmp(&b.start_line).then(a.id.cmp(&b.id)));
        }

        Self {
            files,
            symbols,
            edges,
            communities,
            community_of_file,
            symbols_by_file,
        }
    }

    /// Stable id of the community holding `path`.
    pub fn community_id_of(&self, path: &str) -> Option<&str> {
        self.community_of_file
            .get(path)
            .map(|&i| self.communities[i].id.as_str())
    }

    /// Where an edge leaving `scope` lands: `(label, level that opens it)`.
    fn external_target(
        &self,
        scope: &Scope<'_>,
        outside: &Symbol,
    ) -> (String, Option<NestedLevel>) {
        match scope {
            Scope::File(_) => (
                outside.file_path.clone(),
                self.files
                    .contains_key(&outside.file_path)
                    .then(|| NestedLevel::File(outside.file_path.clone())),
            ),
            _ => match self.community_of_file.get(&outside.file_path) {
                Some(&i) => (
                    self.communities[i].label.clone(),
                    Some(NestedLevel::Community(self.communities[i].id.clone())),
                ),
                None => ("(not indexed)".to_string(), None),
            },
        }
    }

    fn community_index(&self, id: &str) -> Option<usize> {
        self.communities.iter().position(|c| c.id == id)
    }

    fn file_tokens(&self, path: &str) -> usize {
        self.files.get(path).map(|f| f.token_count).unwrap_or(0)
    }

    /// Maps an edge endpoint (symbol) to the id of the child it belongs to in `scope`, or
    /// `None` when the symbol is outside the scope. In a file scope every symbol of the file
    /// is inside; file-scope entries map to an id with no node, so their internal edges are
    /// counted as hidden instead of vanishing.
    fn scope_key(&self, scope: &Scope<'_>, s: &Symbol) -> Option<String> {
        match scope {
            Scope::Project => self
                .community_of_file
                .get(&s.file_path)
                .map(|&i| self.communities[i].id.clone()),
            Scope::Community(ci) => (self.community_of_file.get(&s.file_path) == Some(ci))
                .then(|| format!("f:{}", s.file_path)),
            Scope::File(path) => (s.file_path == **path).then(|| format!("s:{}", s.id)),
        }
    }

    /// Returns the children of `level` with aggregated edges. Unknown ids are an error, never
    /// an empty view.
    pub fn view(&self, level: &NestedLevel) -> Result<NestedView> {
        let mut breadcrumbs = vec![("Project".to_string(), NestedLevel::Project)];
        let mut children: Vec<NestedNode> = Vec::new();
        let scope = match level {
            NestedLevel::Project => {
                for c in &self.communities {
                    children.push(NestedNode {
                        id: c.id.clone(),
                        label: c.label.clone(),
                        kind: NestedKind::Community,
                        child_count: c.files.len(),
                        tokens: c.files.iter().map(|f| self.file_tokens(f)).sum(),
                        location: None,
                        detail: String::new(),
                        drill: Some(NestedLevel::Community(c.id.clone())),
                    });
                }
                Scope::Project
            }
            NestedLevel::Community(cid) => {
                let Some(ci) = self.community_index(cid) else {
                    bail!(
                        "unknown community id `{cid}` (the index may have changed; go back to Project)"
                    );
                };
                let c = &self.communities[ci];
                breadcrumbs.push((c.label.clone(), level.clone()));
                for f in &c.files {
                    children.push(NestedNode {
                        id: format!("f:{f}"),
                        label: f.rsplit('/').next().unwrap_or(f).to_string(),
                        kind: NestedKind::File,
                        child_count: self
                            .symbols_by_file
                            .get(f)
                            .map(|v| v.iter().filter(|s| s.kind != SymbolKind::File).count())
                            .unwrap_or(0),
                        tokens: self.file_tokens(f),
                        location: Some(format!("{f}:1")),
                        detail: self
                            .files
                            .get(f)
                            .map(|r| r.language.clone())
                            .unwrap_or_default(),
                        drill: Some(NestedLevel::File(f.clone())),
                    });
                }
                Scope::Community(ci)
            }
            NestedLevel::File(path) => {
                if !self.files.contains_key(path) {
                    bail!("file `{path}` is not indexed");
                }
                if let Some(&ci) = self.community_of_file.get(path) {
                    let c = &self.communities[ci];
                    breadcrumbs.push((c.label.clone(), NestedLevel::Community(c.id.clone())));
                }
                breadcrumbs.push((
                    path.rsplit('/').next().unwrap_or(path).to_string(),
                    level.clone(),
                ));
                for s in self.symbols_by_file.get(path).into_iter().flatten() {
                    if s.kind == SymbolKind::File {
                        continue;
                    }
                    children.push(NestedNode {
                        id: format!("s:{}", s.id),
                        label: s.qualified_name.clone(),
                        kind: NestedKind::Symbol,
                        child_count: 0,
                        tokens: s.token_count,
                        location: Some(format!("{}:{}", s.file_path, s.start_line)),
                        detail: s.kind.as_str().to_string(),
                        drill: None,
                    });
                }
                Scope::File(path)
            }
        };

        // Cap the node count, keeping the heaviest children (tokens, then id).
        let mut truncated = 0;
        if children.len() > MAX_LEVEL_NODES {
            let mut order: Vec<usize> = (0..children.len()).collect();
            order.sort_by(|&a, &b| {
                children[b]
                    .tokens
                    .cmp(&children[a].tokens)
                    .then_with(|| children[a].id.cmp(&children[b].id))
            });
            let mut keep = vec![false; children.len()];
            for &i in order.iter().take(MAX_LEVEL_NODES) {
                keep[i] = true;
            }
            truncated = children.len() - MAX_LEVEL_NODES;
            let mut k = keep.into_iter();
            children.retain(|_| k.next().unwrap_or(false));
        }

        let index: HashMap<&str, usize> = children
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();

        let mut agg: BTreeMap<(usize, usize), usize> = BTreeMap::new();
        let mut external_edges = 0;
        let mut external: BTreeMap<String, ExternalGroup> = BTreeMap::new();
        let mut hidden_edges = 0;
        for e in self.edges {
            if e.kind == EdgeKind::Contains {
                continue;
            }
            let (Some(s), Some(t)) = (
                self.symbols.get(&e.source_id),
                self.symbols.get(&e.target_id),
            ) else {
                continue;
            };
            match (self.scope_key(&scope, s), self.scope_key(&scope, t)) {
                (None, None) => {}
                (Some(inside), None) | (None, Some(inside)) => {
                    external_edges += 1;
                    let outside = if self.scope_key(&scope, s).is_some() {
                        t
                    } else {
                        s
                    };
                    let (label, lvl) = self.external_target(&scope, outside);
                    let g = external.entry(label).or_insert_with(|| ExternalGroup {
                        label: String::new(),
                        level: lvl,
                        count: 0,
                        from: Vec::new(),
                    });
                    g.count += 1;
                    if let Some(&ci) = index.get(inside.as_str()) {
                        match g.from.iter_mut().find(|(i, _)| *i == ci) {
                            Some((_, n)) => *n += 1,
                            None => g.from.push((ci, 1)),
                        }
                    }
                }
                (Some(a), Some(b)) => match (index.get(a.as_str()), index.get(b.as_str())) {
                    (Some(&ia), Some(&ib)) if ia != ib => *agg.entry((ia, ib)).or_insert(0) += 1,
                    _ => hidden_edges += 1,
                },
            }
        }

        Ok(NestedView {
            level: level.clone(),
            breadcrumbs,
            nodes: children,
            edges: agg
                .into_iter()
                .map(|((src, tgt), weight)| NestedEdge { src, tgt, weight })
                .collect(),
            external_edges,
            external: {
                let mut groups: Vec<ExternalGroup> = external
                    .into_iter()
                    .map(|(label, mut g)| {
                        g.label = label;
                        g.from.sort();
                        g
                    })
                    .collect();
                groups.sort_by(|a, b| b.count.cmp(&a.count).then(a.label.cmp(&b.label)));
                groups
            },
            hidden_edges,
            truncated,
        })
    }

    /// One-line coverage statement for a view, shown next to the graph.
    pub fn coverage_line(view: &NestedView) -> String {
        let total_children = view.nodes.len() + view.truncated;
        let mut s = format!(
            "{} of {} children shown, {} aggregated edges",
            view.nodes.len(),
            total_children,
            view.edges.len()
        );
        if view.truncated > 0 {
            s.push_str(&format!(
                " (cap {MAX_LEVEL_NODES}: {} smallest omitted)",
                view.truncated
            ));
        }
        if view.external_edges > 0 {
            s.push_str(&format!(", {} edges leave this scope", view.external_edges));
        }
        if view.hidden_edges > 0 {
            s.push_str(&format!(
                ", {} internal edges not drawn (inside one node or file-scope)",
                view.hidden_edges
            ));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, tokens: usize) -> (String, FileRecord) {
        (
            path.to_string(),
            FileRecord {
                path: path.to_string(),
                language: "rust".to_string(),
                hash: String::new(),
                mtime_secs: 0,
                size_bytes: 0,
                line_count: 10,
                token_count: tokens,
            },
        )
    }

    fn sym(file: &str, name: &str, line: usize, kind: SymbolKind) -> (String, Symbol) {
        let id = format!("{file}::{name}#{line}");
        (
            id.clone(),
            Symbol {
                id,
                file_path: file.to_string(),
                name: name.to_string(),
                qualified_name: name.to_string(),
                kind,
                signature: String::new(),
                start_line: line,
                end_line: line,
                token_count: 5,
                visibility: String::new(),
                doc_summary: String::new(),
                parent_id: None,
            },
        )
    }

    fn edge(a: &str, b: &str, kind: EdgeKind) -> Edge {
        Edge {
            source_id: a.to_string(),
            target_id: b.to_string(),
            kind,
            weight: 1.0,
            confidence: Default::default(),
        }
    }

    struct Fixture {
        files: HashMap<String, FileRecord>,
        symbols: HashMap<String, Symbol>,
        edges: Vec<Edge>,
        comms: HashMap<String, usize>,
    }

    /// Two communities: {src/a.rs, src/b.rs} and {lib/c.rs}.
    fn fixture() -> Fixture {
        let files: HashMap<_, _> = [
            file("src/a.rs", 100),
            file("src/b.rs", 50),
            file("lib/c.rs", 70),
        ]
        .into_iter()
        .collect();
        let symbols: HashMap<_, _> = [
            sym("src/a.rs", "a.rs", 1, SymbolKind::File),
            sym("src/a.rs", "fa", 2, SymbolKind::Function),
            sym("src/a.rs", "ga", 5, SymbolKind::Function),
            sym("src/b.rs", "fb", 3, SymbolKind::Function),
            sym("lib/c.rs", "fc", 4, SymbolKind::Function),
        ]
        .into_iter()
        .collect();
        let edges = vec![
            edge("src/a.rs::fa#2", "src/b.rs::fb#3", EdgeKind::Calls),
            edge("src/a.rs::ga#5", "src/b.rs::fb#3", EdgeKind::Calls),
            edge("src/a.rs::fa#2", "src/a.rs::ga#5", EdgeKind::Calls),
            edge("src/b.rs::fb#3", "lib/c.rs::fc#4", EdgeKind::Calls),
            edge("src/a.rs::a.rs#1", "lib/c.rs::fc#4", EdgeKind::Imports),
            edge("src/a.rs::a.rs#1", "src/a.rs::fa#2", EdgeKind::Contains),
        ];
        let comms: HashMap<_, _> = [
            ("src/a.rs".to_string(), 7),
            ("src/b.rs".to_string(), 7),
            ("lib/c.rs".to_string(), 3),
        ]
        .into_iter()
        .collect();
        Fixture {
            files,
            symbols,
            edges,
            comms,
        }
    }

    fn graph(f: &Fixture) -> NestedGraph<'_> {
        NestedGraph::from_parts(&f.files, &f.symbols, &f.edges, &f.comms)
    }

    fn edge_by_label(v: &NestedView, a: &str, b: &str) -> Option<usize> {
        v.edges
            .iter()
            .find(|e| v.nodes[e.src].label == a && v.nodes[e.tgt].label == b)
            .map(|e| e.weight)
    }

    fn accounted(v: &NestedView) -> usize {
        v.edges.iter().map(|e| e.weight).sum::<usize>() + v.hidden_edges + v.external_edges
    }

    #[test]
    fn project_level_aggregates_cross_community_edges() {
        let f = fixture();
        let v = graph(&f).view(&NestedLevel::Project).unwrap();
        let ids: Vec<&str> = v.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["c:lib/c.rs", "c:src/a.rs"]);
        // fb -> fc and the a.rs file-scope import -> fc: 2 edges from src to lib.
        assert_eq!(edge_by_label(&v, "src", "lib/c.rs"), Some(2));
        assert_eq!(v.edges.len(), 1);
        // 3 edges stay inside the src community (not drawn at this level); Contains ignored.
        assert_eq!(v.hidden_edges, 3);
        assert_eq!(v.external_edges, 0);
        assert_eq!(v.nodes[1].child_count, 2);
        assert_eq!(v.nodes[1].tokens, 150);
    }

    #[test]
    fn community_level_shows_files_and_counts_leaving_edges() {
        let f = fixture();
        let v = graph(&f)
            .view(&NestedLevel::Community("c:src/a.rs".into()))
            .unwrap();
        assert_eq!(v.nodes.len(), 2);
        assert_eq!(edge_by_label(&v, "a.rs", "b.rs"), Some(2));
        assert_eq!(
            v.external_edges, 2,
            "fb->fc and the import leave the community"
        );
        assert_eq!(v.hidden_edges, 1, "fa->ga stays inside a.rs");
        // Both leaving edges go to the lib community: the import from a.rs (child 0) and
        // fb->fc from b.rs (child 1).
        assert_eq!(v.external.len(), 1);
        assert_eq!(v.external[0].count, 2);
        assert_eq!(
            v.external[0].level,
            Some(NestedLevel::Community("c:lib/c.rs".into()))
        );
        assert_eq!(v.external[0].from, vec![(0, 1), (1, 1)]);
        let g = graph(&f);
        assert_eq!(g.community_id_of("src/b.rs"), Some("c:src/a.rs"));
        assert_eq!(g.community_id_of("nope.rs"), None);
        let fv = g.view(&NestedLevel::File("src/b.rs".into())).unwrap();
        let sum: usize = fv.external.iter().map(|e| e.count).sum();
        assert_eq!(sum, fv.external_edges);
        assert!(
            fv.external
                .iter()
                .any(|e| e.level == Some(NestedLevel::File("lib/c.rs".into())))
        );
        assert_eq!(v.breadcrumbs.len(), 2);
        assert_eq!(v.nodes[0].location.as_deref(), Some("src/a.rs:1"));
        assert_eq!(
            v.nodes[0].child_count, 2,
            "the file-scope entry is not counted"
        );
    }

    #[test]
    fn file_level_shows_symbols_with_locations() {
        let f = fixture();
        let v = graph(&f)
            .view(&NestedLevel::File("src/a.rs".into()))
            .unwrap();
        let labels: Vec<&str> = v.nodes.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["fa", "ga"],
            "the file-scope entry is not a node"
        );
        assert_eq!(edge_by_label(&v, "fa", "ga"), Some(1));
        assert_eq!(v.external_edges, 3);
        assert_eq!(v.hidden_edges, 0);
        assert_eq!(v.nodes[1].location.as_deref(), Some("src/a.rs:5"));
        assert_eq!(v.breadcrumbs.len(), 3);
        assert!(v.nodes.iter().all(|n| n.drill.is_none()));
    }

    #[test]
    fn unknown_scopes_are_errors_not_empty_views() {
        let f = fixture();
        let g = graph(&f);
        assert!(g.view(&NestedLevel::Community("c:nope".into())).is_err());
        assert!(g.view(&NestedLevel::File("nope.rs".into())).is_err());
    }

    #[test]
    fn group_ids_are_stable_when_numeric_labels_change() {
        let f = fixture();
        let mut relabeled = f.comms.clone();
        for v in relabeled.values_mut() {
            *v += 1000;
        }
        let a = graph(&f).view(&NestedLevel::Project).unwrap();
        let g2 = NestedGraph::from_parts(&f.files, &f.symbols, &f.edges, &relabeled);
        let b = g2.view(&NestedLevel::Project).unwrap();
        let ids = |v: &NestedView| v.nodes.iter().map(|n| n.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&a), ids(&b));
    }

    #[test]
    fn every_symbol_edge_is_accounted_for() {
        let f = fixture();
        let g = graph(&f);
        let non_contains = f
            .edges
            .iter()
            .filter(|e| e.kind != EdgeKind::Contains)
            .count();
        // The project level sees every edge exactly once.
        assert_eq!(
            accounted(&g.view(&NestedLevel::Project).unwrap()),
            non_contains
        );
        // Each edge touching a scope is drawn, hidden or external, never lost:
        // src community touches all 5, lib touches 2.
        let src = g
            .view(&NestedLevel::Community("c:src/a.rs".into()))
            .unwrap();
        assert_eq!(accounted(&src), 5);
        let lib = g
            .view(&NestedLevel::Community("c:lib/c.rs".into()))
            .unwrap();
        assert_eq!(accounted(&lib), 2);
    }

    #[test]
    fn node_cap_truncates_and_says_so() {
        let mut f = fixture();
        for i in 0..(MAX_LEVEL_NODES + 5) {
            let (k, s) = sym("lib/c.rs", &format!("x{i}"), 10 + i, SymbolKind::Function);
            f.symbols.insert(k, s);
        }
        let v = graph(&f)
            .view(&NestedLevel::File("lib/c.rs".into()))
            .unwrap();
        assert_eq!(v.nodes.len(), MAX_LEVEL_NODES);
        assert_eq!(v.truncated, 6);
        let line = NestedGraph::coverage_line(&v);
        assert!(line.contains("6 smallest omitted"), "{line}");
        assert!(line.contains(&format!("{MAX_LEVEL_NODES} of {}", MAX_LEVEL_NODES + 6)));
    }

    #[test]
    fn community_label_is_deterministic_on_ties() {
        let files = vec!["b/x.rs".to_string(), "a/y.rs".to_string()];
        for _ in 0..20 {
            assert_eq!(infer_community_label(&files), "a");
        }
    }
}
