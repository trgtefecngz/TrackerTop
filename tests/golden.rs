//! Golden correctness corpus: small projects under `tests/golden/<language>/` with answers
//! written by hand in `expected.json` (who calls what, the impact of a change, untested
//! functions, orphan files). TrackerTop indexes each project and its answers are compared
//! with the expected ones:
//!
//! - an `exact` call edge that is not in the expected set fails the test (exact means
//!   "certain"; a wrong certain answer is a bug);
//! - precision and recall per language are printed as a table and must not drop below the
//!   numbers recorded in `tests/golden/baseline.json` (raise them there when they improve).
//!
//! `GOLDEN_DUMP=1 cargo test --test golden -- --nocapture` prints every symbol and edge
//! TrackerTop found, to help when writing or checking a fixture's answers.

mod common;

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use trackertop::db::Database;
use trackertop::graph::CodeGraph;
use trackertop::indexer::index_repository;
use trackertop::model::{Confidence, EdgeKind, SymbolKind};

/// The hand-written answers for one fixture. Symbols are `path::qualified_name`, paths
/// relative to the fixture root.
#[derive(Deserialize)]
struct Expected {
    language: String,
    /// Every call in the project to a symbol the project defines, as caller -> callees
    /// (calls into the standard library or other packages are not listed). A caller with
    /// no project calls is left out.
    calls: BTreeMap<String, Vec<String>>,
    /// Symbol -> every function, method or test that depends on it within 2 call hops.
    impact: BTreeMap<String, Vec<String>>,
    /// Functions and methods no test reaches (the `untested` review rule, with every
    /// file of the fixture new on a branch).
    untested: Vec<String>,
    /// Code files nothing in another file refers to (`health_check` orphans).
    orphan_files: Vec<String>,
}

#[derive(Deserialize, Clone, Copy, Default)]
struct Floor {
    call_precision: f64,
    call_recall: f64,
    answer_precision: f64,
    answer_recall: f64,
}

#[derive(Default, Clone, Copy)]
struct Score {
    expected: usize,
    found: usize,
    correct: usize,
}

impl Score {
    fn add(&mut self, expected: &BTreeSet<String>, found: &BTreeSet<String>) {
        self.expected += expected.len();
        self.found += found.len();
        self.correct += expected.intersection(found).count();
    }
    fn precision(&self) -> f64 {
        if self.found == 0 {
            1.0
        } else {
            self.correct as f64 / self.found as f64
        }
    }
    fn recall(&self) -> f64 {
        if self.expected == 0 {
            1.0
        } else {
            self.correct as f64 / self.expected as f64
        }
    }
}

struct Outcome {
    language: String,
    calls: Score,
    exact_found: usize,
    wrong_exact: Vec<String>,
    answers: Score,
    mismatches: Vec<String>,
}

fn golden_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// `path::qualified_name` of a symbol id (`path::qualified_name#line`).
fn key(id: &str) -> String {
    id.rsplit_once('#').map_or(id, |(k, _)| k).to_string()
}

fn set(items: &[String]) -> BTreeSet<String> {
    items.iter().cloned().collect()
}

fn diff(what: &str, expected: &BTreeSet<String>, found: &BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    for m in expected.difference(found) {
        out.push(format!("{what}: missing {m}"));
    }
    for w in found.difference(expected) {
        out.push(format!("{what}: wrong {w}"));
    }
    out
}

fn is_code_symbol(kind: SymbolKind) -> bool {
    !matches!(
        kind,
        SymbolKind::File | SymbolKind::Section | SymbolKind::Dependency
    )
}

fn run_fixture(dir: &Path) -> Outcome {
    let expected: Expected =
        serde_json::from_str(&std::fs::read_to_string(dir.join("expected.json")).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // The fixture arrives as new files on a branch, so `review` sees every function as
    // added (for the `untested` rule)
    common::git_in(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join(".gitkeep"), "").unwrap();
    common::git_in(root, &["add", "."]);
    common::git_in(root, &["commit", "-q", "-m", "base"]);
    common::git_in(root, &["checkout", "-q", "-b", "work"]);
    copy_dir(dir, root);
    std::fs::remove_file(root.join("expected.json")).unwrap();
    index_repository(root, true).unwrap();

    let db = Database::open_for_repo(root).unwrap();
    let cg = CodeGraph::load(&db).unwrap();
    let dump = std::env::var_os("GOLDEN_DUMP").is_some();
    if dump {
        let mut syms: Vec<_> = cg.symbols.values().collect();
        syms.sort_by(|a, b| a.id.cmp(&b.id));
        for s in syms {
            println!("SYM {} [{}]", s.id, s.kind.as_str());
        }
    }

    // 1. Call edges (CALLS and TESTS) between project symbols
    let mut found_calls: BTreeSet<String> = BTreeSet::new();
    let mut exact_calls: BTreeSet<String> = BTreeSet::new();
    let mut edges: Vec<_> = cg
        .edges
        .iter()
        .filter(|e| matches!(e.kind, EdgeKind::Calls | EdgeKind::Tests))
        .collect();
    edges.sort_by(|a, b| (&a.source_id, &a.target_id).cmp(&(&b.source_id, &b.target_id)));
    for e in edges {
        let pair = format!("{} -> {}", key(&e.source_id), key(&e.target_id));
        if dump {
            println!("EDGE {pair} [{} {}]", e.kind.as_str(), e.confidence.as_str());
        }
        if e.confidence == Confidence::Exact {
            exact_calls.insert(pair.clone());
        }
        found_calls.insert(pair);
    }
    let expected_calls: BTreeSet<String> = expected
        .calls
        .iter()
        .flat_map(|(from, tos)| tos.iter().map(move |t| format!("{from} -> {t}")))
        .collect();
    let mut calls = Score::default();
    calls.add(&expected_calls, &found_calls);
    let wrong_exact: Vec<String> = exact_calls.difference(&expected_calls).cloned().collect();
    let mut mismatches = diff("call", &expected_calls, &found_calls);

    // 2. Answers built on the edges: impact, untested, orphan files
    let mut answers = Score::default();
    for (target, deps) in &expected.impact {
        let sym = cg
            .symbols
            .values()
            .find(|s| key(&s.id) == *target)
            .unwrap_or_else(|| panic!("{}: no symbol {target}", dir.display()));
        let report = cg.impact_analysis(&sym.id, 2).unwrap();
        let found: BTreeSet<String> = report
            .direct_dependents
            .iter()
            .chain(&report.transitive_dependents)
            .chain(&report.related_tests)
            .filter(|s| is_code_symbol(s.kind))
            .map(|s| key(&s.id))
            .collect();
        let want = set(deps);
        answers.add(&want, &found);
        mismatches.extend(diff(&format!("impact of {target}"), &want, &found));
    }

    let cs = trackertop::review::review(root, "main").unwrap();
    let found_untested: BTreeSet<String> = cs
        .findings
        .iter()
        .filter(|f| f.rule == "untested")
        .map(|f| f.id.trim_start_matches("untested:").replacen(':', "::", 1))
        .collect();
    let want = set(&expected.untested);
    answers.add(&want, &found_untested);
    mismatches.extend(diff("untested", &want, &found_untested));

    let found_orphans: BTreeSet<String> = cg.health_report().orphan_files.into_iter().collect();
    let want = set(&expected.orphan_files);
    answers.add(&want, &found_orphans);
    mismatches.extend(diff("orphan file", &want, &found_orphans));

    Outcome {
        language: expected.language,
        calls,
        exact_found: exact_calls.len(),
        wrong_exact,
        answers,
        mismatches,
    }
}

fn pct(x: f64) -> String {
    format!("{:.1}%", x * 100.0)
}

#[test]
fn golden_corpus_answers_match_hand_checked_expectations() {
    let root = golden_root();
    let floors: BTreeMap<String, Floor> =
        serde_json::from_str(&std::fs::read_to_string(root.join("baseline.json")).unwrap())
            .unwrap();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("expected.json").is_file())
        .collect();
    dirs.sort();
    assert!(dirs.len() >= 4, "fixtures missing under {}", root.display());

    let outcomes: Vec<Outcome> = dirs.iter().map(|d| run_fixture(d)).collect();

    let mut table = String::from(
        "\n| Language | Call edges (expected / found / correct) | Call precision | Call recall \
         | Exact edges (wrong) | Answers precision | Answers recall |\n\
         |---|---|---|---|---|---|---|\n",
    );
    for o in &outcomes {
        table.push_str(&format!(
            "| {} | {} / {} / {} | {} | {} | {} ({}) | {} | {} |\n",
            o.language,
            o.calls.expected,
            o.calls.found,
            o.calls.correct,
            pct(o.calls.precision()),
            pct(o.calls.recall()),
            o.exact_found,
            o.wrong_exact.len(),
            pct(o.answers.precision()),
            pct(o.answers.recall()),
        ));
    }
    println!("{table}");
    for o in &outcomes {
        for m in &o.mismatches {
            println!("  [{}] {m}", o.language);
        }
    }

    let mut failures = Vec::new();
    for o in &outcomes {
        for w in &o.wrong_exact {
            failures.push(format!("[{}] wrong exact edge: {w}", o.language));
        }
        let floor = floors.get(&o.language).copied().unwrap_or_else(|| {
            failures.push(format!("[{}] no baseline in baseline.json", o.language));
            Floor::default()
        });
        // Rounded like the table, so a recorded number compares equal to itself
        let r = |x: f64| (x * 1000.0).round() / 1000.0;
        for (what, got, min) in [
            ("call precision", o.calls.precision(), floor.call_precision),
            ("call recall", o.calls.recall(), floor.call_recall),
            ("answer precision", o.answers.precision(), floor.answer_precision),
            ("answer recall", o.answers.recall(), floor.answer_recall),
        ] {
            if r(got) < r(min) {
                failures.push(format!(
                    "[{}] {what} dropped to {} (baseline {})",
                    o.language,
                    pct(got),
                    pct(min)
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{table}\n{}", failures.join("\n"));
}
