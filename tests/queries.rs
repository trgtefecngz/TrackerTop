//! Query tools: smart_grep, lookups, outlines, read_symbol, the nested graph and the health check.

mod common;

use common::*;
use tempfile::TempDir;
use trackertop::context::TrackerEngine;
use trackertop::indexer::index_repository;

#[test]
fn smart_grep_matches_a_plain_scan_and_states_its_coverage() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src/ui")).unwrap();
    std::fs::write(
        root.join("src/a.rs"),
        "// Needle in a comment\r\npub fn find_needle() -> &'static str {\r\n    \"NEEDLE (in a string)\"\r\n}\r\nfn other() { let needle = needle_twice(needle); }\r\n",
    )
    .unwrap();
    let many: String = (0..20)
        .map(|i| format!("export const needle{i} = () => {i};\n"))
        .collect();
    std::fs::write(root.join("src/b.ts"), many).unwrap();
    std::fs::write(
        root.join("src/ui/c.tsx"),
        "export function Banner() {\n  return <p>ÇÖĞÜŞ a.b (needle)</p>;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("NOTES.unindexed"),
        "needle in docs, not indexed\n",
    )
    .unwrap();

    index_repository(root, true).unwrap();
    let db = trackertop::db::Database::open_for_repo(root).unwrap();
    let graph = trackertop::graph::CodeGraph::load(&db).unwrap();
    let indexed: Vec<&String> = graph.files.keys().collect();

    for pattern in [
        "needle",
        "NEEDLE",
        "çöğüş",
        "a.b",
        "(",
        "static str",
        "zzz-absent",
    ] {
        let scan = trackertop::context::grep_scan(root, &indexed, pattern, None);
        let mut got: Vec<(String, usize)> = scan
            .files
            .iter()
            .flat_map(|f| f.lines.iter().map(move |(n, _)| (f.path.clone(), *n)))
            .collect();
        got.sort();
        assert_eq!(got, naive_grep(root, pattern), "pattern {pattern:?}");
        assert!(scan.unreadable.is_empty());
        assert_eq!(scan.files_searched, 3, "pattern {pattern:?}");
    }

    // Rendered output: caps are announced and totals match the ground truth.
    let engine = TrackerEngine::new(root);
    let out = engine.smart_grep("needle", None, 20_000).unwrap().text;
    let truth = naive_grep(root, "needle").len();
    assert!(
        out.contains(&format!("{truth} matching lines in 3 files")),
        "{out}"
    );
    assert!(
        out.contains("(showing 12 of 20 matches in this file; 8 more not shown)"),
        "{out}"
    );
    assert!(
        out.contains("Not searched: files outside the index"),
        "{out}"
    );
    let absent = engine.smart_grep("zzz-absent", None, 2_000).unwrap().text;
    assert!(absent.contains("No matches found in the searched files."));
    assert!(absent.contains("0 matching lines in 0 files"), "{absent}");
    let filtered = engine
        .smart_grep("needle", Some("src/ui"), 2_000)
        .unwrap()
        .text;
    assert!(
        filtered.contains("(2 more excluded by file_filter)"),
        "{filtered}"
    );
}

#[test]
fn lookups_distinguish_not_indexed_from_not_found() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn present() {}\n").unwrap();
    std::fs::write(root.join("data.unindexed"), "# readme\n").unwrap();
    index_repository(root, true).unwrap();
    let engine = TrackerEngine::new(root);

    let readme = engine.file_outline("data.unindexed", 800).unwrap().text;
    assert!(
        readme.contains("NOT INDEXED: `data.unindexed` exists"),
        "{readme}"
    );
    let missing = engine.file_outline("src/nope.rs", 800).unwrap().text;
    assert!(
        missing.contains("NOT FOUND: no file `src/nope.rs`"),
        "{missing}"
    );
    let sym = engine.read_symbol("absent_fn", true, 800).unwrap().text;
    assert!(sym.contains("NOT FOUND IN INDEX"), "{sym}");
    assert!(sym.contains("1 indexed files"), "{sym}");
    let batch = engine
        .read_symbols_batch(
            &["present".to_string(), "absent_fn".to_string()],
            false,
            2_000,
        )
        .unwrap()
        .text;
    assert!(batch.contains("pub fn present"), "{batch}");
    assert!(
        batch.contains("NOT FOUND IN INDEX: no symbol matches `absent_fn`"),
        "{batch}"
    );
}

#[test]
fn outlines_state_whether_spans_come_from_a_syntax_tree() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn precise() {}\n").unwrap();
    std::fs::write(root.join("src/a.ts"), "export function tsFn() {}\n").unwrap();
    std::fs::write(
        root.join("run.ps1"),
        "function Invoke-Thing {\n  Write-Host 1\n}\n",
    )
    .unwrap();
    index_repository(root, true).unwrap();
    let engine = TrackerEngine::new(root);
    for path in ["src/lib.rs", "src/a.ts"] {
        let out = engine.file_outline(path, 800).unwrap().text;
        assert!(out.contains("; tree-sitter)"), "{out}");
    }
    let ps = engine.file_outline("run.ps1", 800).unwrap().text;
    assert!(ps.contains("heuristic extractor"), "{ps}");
    let sym = engine.read_symbol("Invoke-Thing", false, 800).unwrap().text;
    assert!(sym.contains("Span: heuristic extractor"), "{sym}");
    let precise = engine.read_symbol("precise", false, 800).unwrap().text;
    assert!(!precise.contains("Span:"), "{precise}");
}

/// The matryoshka view must account for every symbol edge at every level of a real index:
/// each edge touching a scope is drawn (inside an aggregated edge), hidden or external,
/// checked against an independent count from the raw edge list.
#[test]
fn nested_graph_accounts_for_every_edge_on_a_real_index() {
    use std::collections::HashSet;
    use trackertop::db::Database;
    use trackertop::graph::CodeGraph;
    use trackertop::model::EdgeKind;
    use trackertop::nested::{NestedGraph, NestedLevel, NestedView};

    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write_call_graph_repo(root, 3);
    std::fs::create_dir_all(root.join("tools")).unwrap();
    std::fs::write(
        root.join("tools/cli.rs"),
        "pub fn main_tool() -> u8 { crate::root() + crate::b::leaf() }\n",
    )
    .unwrap();
    index_repository(root, false).unwrap();
    let db = Database::open_for_repo(root).unwrap();
    let cg = CodeGraph::load(&db).unwrap();
    let ng = NestedGraph::new(&cg);

    let resolved: Vec<(String, String)> = cg
        .edges
        .iter()
        .filter(|e| e.kind != EdgeKind::Contains)
        .filter_map(|e| {
            let s = cg.symbols.get(&e.source_id)?;
            let t = cg.symbols.get(&e.target_id)?;
            Some((s.file_path.clone(), t.file_path.clone()))
        })
        .collect();
    assert!(
        !resolved.is_empty(),
        "fixture must produce cross-symbol edges"
    );
    let accounted = |v: &NestedView| {
        v.edges.iter().map(|e| e.weight).sum::<usize>() + v.hidden_edges + v.external_edges
    };

    let project = ng.view(&NestedLevel::Project).unwrap();
    assert_eq!(accounted(&project), resolved.len());
    assert_eq!(project.truncated, 0);
    let total_files: usize = project.nodes.iter().map(|n| n.child_count).sum();
    assert_eq!(
        total_files,
        cg.files.len(),
        "every file sits in exactly one community"
    );

    for c in &project.nodes {
        let level = c.drill.clone().unwrap();
        let cv = ng.view(&level).unwrap();
        let files: HashSet<String> = cv
            .nodes
            .iter()
            .map(|n| n.id.trim_start_matches("f:").to_string())
            .collect();
        let touching = resolved
            .iter()
            .filter(|(a, b)| files.contains(a) || files.contains(b))
            .count();
        assert_eq!(accounted(&cv), touching, "community {}", c.id);
        for f in &cv.nodes {
            let fv = ng.view(f.drill.as_ref().unwrap()).unwrap();
            let path = f.id.trim_start_matches("f:");
            let touching = resolved
                .iter()
                .filter(|(a, b)| a == path || b == path)
                .count();
            assert_eq!(accounted(&fv), touching, "file {path}");
            for s in &fv.nodes {
                let loc = s.location.as_deref().unwrap();
                assert!(loc.starts_with(&format!("{path}:")), "{loc}");
            }
        }
    }
}

/// Symbol lookups never guess silently: `path::name` picks a symbol in a file, a name
/// shared by several symbols is reported as ambiguous with the alternatives, and a fuzzy
/// match is labelled as one.
#[test]
fn symbol_lookups_say_when_they_are_ambiguous_or_approximate() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "pub fn load() -> i32 {\n    1\n}\n").unwrap();
    std::fs::write(root.join("src/b.rs"), "pub fn load() -> i32 {\n    2\n}\n").unwrap();
    std::fs::write(
        root.join("src/c.rs"),
        "pub fn configuration_loader() -> i32 {\n    3\n}\n",
    )
    .unwrap();
    let engine = TrackerEngine::new(root);

    let out = engine
        .read_symbol("src/b.rs::load", false, 2_000)
        .unwrap()
        .text;
    assert!(out.contains("(src/b.rs:L1-3)"), "{out}");
    assert!(!out.contains("AMBIGUOUS"), "{out}");

    let out = engine.read_symbol("load", false, 2_000).unwrap().text;
    assert!(
        out.contains("AMBIGUOUS: 1 other symbol(s) are also named `load`"),
        "{out}"
    );

    let out = engine
        .read_symbol("configuration", false, 2_000)
        .unwrap()
        .text;
    assert!(out.contains("# SYMBOL configuration_loader"), "{out}");
    assert!(
        out.contains("NOTE: no symbol is named exactly `configuration`"),
        "{out}"
    );
}

/// `read_symbol("path:Lstart-end")` reads a line range (an impl tail, a test module's
/// helpers) and names the innermost symbol around it; a bad range fails loudly.
#[test]
fn read_symbol_reads_a_line_range() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub struct S;\nimpl S {\n    pub fn a(&self) -> i32 {\n        1\n    }\n\n    const TAIL: i32 = 7;\n}\n",
    )
    .unwrap();
    let engine = TrackerEngine::new(root);
    let out = engine
        .read_symbol("src/lib.rs:L6-8", false, 1_000)
        .unwrap()
        .text;
    assert!(out.contains("# LINES src/lib.rs:L6-8"), "{out}");
    assert!(out.contains("   7 |     const TAIL: i32 = 7;"), "{out}");
    assert!(!out.contains("pub fn a"), "{out}");
    assert!(out.contains("inside impl S"), "{out}");

    let out = engine
        .read_symbol("src/lib.rs:L7-99", false, 1_000)
        .unwrap()
        .text;
    assert!(out.contains("# LINES src/lib.rs:L7-8"), "{out}");
    assert!(out.contains("clamped: the file has 8 lines"), "{out}");

    let out = engine
        .read_symbol("src/lib.rs:L20-30", false, 1_000)
        .unwrap()
        .text;
    assert!(
        out.contains("OUT OF RANGE: src/lib.rs has 8 lines"),
        "{out}"
    );
    let out = engine
        .read_symbol("src/nope.rs:L1-3", false, 1_000)
        .unwrap()
        .text;
    assert!(out.contains("NOT INDEXED"), "{out}");
    // `path::name` with a numeric name is a symbol lookup, not a range
    let out = engine
        .read_symbol("src/lib.rs::7", false, 1_000)
        .unwrap()
        .text;
    assert!(
        !out.contains("# LINES") && !out.contains("`src/lib.rs:`"),
        "{out}"
    );
}

/// The health check reports only real issues: Rust module cycles and doc mentions are not
/// dependency cycles, scripts and entry points run directly are not orphans (they are
/// listed apart, with the reason), and only size outliers are oversized.
#[test]
fn health_check_reports_only_real_issues() {
    use trackertop::graph::CodeGraph;
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    for d in ["src", "scripts", "web"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    // A real import cycle (TypeScript)
    std::fs::write(
        root.join("web/c.ts"),
        "import { d } from \"./d\";\nexport function c(): number {\n  return d();\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("web/d.ts"),
        "import { c } from \"./c\";\nexport function d(): number {\n  return c();\n}\n",
    )
    .unwrap();
    // Rust modules that call each other: allowed
    std::fs::write(root.join("src/lib.rs"), "pub mod x;\npub mod y;\n").unwrap();
    std::fs::write(
        root.join("src/x.rs"),
        "pub fn ping(n: u32) -> u32 { if n == 0 { 0 } else { crate::y::pong(n - 1) } }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/y.rs"),
        "pub fn pong(n: u32) -> u32 { crate::x::ping(n) }\n",
    )
    .unwrap();
    // Docs mentioning both TS files in both directions are no dependency
    std::fs::write(
        root.join("README.md"),
        "# Demo\n\nSee `web/c.ts` and `web/d.ts`.\n",
    )
    .unwrap();
    // An installer run by the user and a file nothing refers to
    std::fs::write(
        root.join("scripts/install.ps1"),
        "function Install-Demo {\n  Write-Host 'hi'\n}\nInstall-Demo\n",
    )
    .unwrap();
    std::fs::write(root.join("web/lonely.py"), "def lonely():\n    return 1\n").unwrap();
    // Many ordinary files and one size outlier
    for i in 0..12 {
        let body: String = (0..20)
            .map(|j| format!("export function f{i}_{j}(a: number): number {{ return a + {j}; }}\n"))
            .collect();
        std::fs::write(root.join(format!("web/m{i}.ts")), body).unwrap();
    }
    let big: String = (0..900)
        .map(|j| {
            format!("export function big_{j}(alpha: number, beta: number): number {{ return alpha * {j} + beta; }}\n")
        })
        .collect();
    std::fs::write(root.join("web/big.ts"), big).unwrap();
    // Larger than the rest but not an outlier: a fixed 3,500-token threshold flagged it
    let mid: String = (0..250)
        .map(|j| {
            format!("export function mid_{j}(alpha: number, beta: number): number {{ return alpha * {j} + beta; }}\n")
        })
        .collect();
    std::fs::write(root.join("web/mid.ts"), mid).unwrap();
    index_repository(root, true).unwrap();

    let db = trackertop::db::Database::open_for_repo(root).unwrap();
    let hr = CodeGraph::load(&db).unwrap().health_report();
    assert_eq!(
        hr.circular_groups,
        vec![vec!["web/c.ts".to_string(), "web/d.ts".to_string()]],
        "{hr:#?}"
    );
    assert_eq!(hr.cycle_exempt_files, 3, "{hr:#?}");
    assert!(
        hr.orphan_files.contains(&"web/lonely.py".to_string()),
        "{hr:#?}"
    );
    assert!(
        !hr.orphan_files
            .iter()
            .any(|p| p.starts_with("scripts/") || p.starts_with("src/")),
        "{hr:#?}"
    );
    assert!(
        hr.orphan_excluded
            .iter()
            .any(|(p, why)| p == "scripts/install.ps1" && why.contains("script")),
        "{hr:#?}"
    );
    let oversized: Vec<&str> = hr.bloated_files.iter().map(|b| b.0.as_str()).collect();
    assert_eq!(oversized, vec!["web/big.ts"], "{hr:#?}");
    assert!(hr.bloat_threshold >= trackertop::graph::BLOAT_FLOOR_TOKENS);

    let engine = TrackerEngine::new(root);
    let text = engine.health_check(2_000).unwrap().text;
    for want in [
        "Coverage:",
        "3 Rust files left out",
        "`scripts/install.ps1` — script run directly",
        "`web/big.ts`",
    ] {
        assert!(text.contains(want), "missing {want:?}:\n{text}");
    }
}

/// Graph answers say how far to trust each edge: `call_graph` and `read_symbol` label every
/// caller and callee, and `impact_analysis` splits its direct and transitive counts into
/// exact and heuristic (a path is exact only when every edge on it is).
#[test]
fn graph_answers_label_edge_confidence() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "mod core;\nmod far;\n").unwrap();
    std::fs::write(
        root.join("src/core.rs"),
        "pub fn target() -> u8 { 1 }\n\
         pub fn near() -> u8 { target() }\n\
         pub fn near_top() -> u8 { near() }\n",
    )
    .unwrap();
    // `target` is defined in another file: a bare call to it is found by name only
    std::fs::write(
        root.join("src/far.rs"),
        "pub fn far() -> u8 { target() }\npub fn far_top() -> u8 { far() }\n",
    )
    .unwrap();
    index_repository(root, true).unwrap();
    let engine = TrackerEngine::new(root);

    let out = engine.call_graph("target", 2, 4_000).unwrap().text;
    assert!(
        out.contains("## Incoming / Callers: 4 (2 exact, 2 heuristic)"),
        "{out}"
    );
    assert!(out.contains("d1 [CALLS exact] near "), "{out}");
    assert!(out.contains("d1 [CALLS heuristic] far "), "{out}");
    // `far_top -> far` is exact, but the path to `target` goes through a guess
    assert!(out.contains("d2 [CALLS heuristic] far_top "), "{out}");
    assert!(out.contains("d2 [CALLS exact] near_top "), "{out}");

    let out = engine.impact_analysis("target", 2, 4_000).unwrap().text;
    assert!(
        out.contains("Direct: 2 (1 exact, 1 heuristic) | Transitive: 2 (1 exact, 1 heuristic)"),
        "{out}"
    );
    assert!(out.contains("- far [fn] heuristic (src/far.rs"), "{out}");
    assert!(out.contains("- near [fn] exact (src/core.rs"), "{out}");

    let out = engine.read_symbol("near", true, 4_000).unwrap().text;
    assert!(out.contains("- [CALLS exact] target "), "{out}");
    let out = engine.read_symbol("far", true, 4_000).unwrap().text;
    assert!(out.contains("- [CALLS heuristic] target "), "{out}");
}
