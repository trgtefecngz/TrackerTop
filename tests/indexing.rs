//! Incremental indexing: caching, rehashing, interrupted and concurrent runs, extractor revisions.

mod common;

use common::*;
use tempfile::TempDir;
use trackertop::context::TrackerEngine;
use trackertop::indexer::index_repository;

/// The MCP engine caches the built graph between calls; it must still see edits made
/// in this process AND re-indexes performed by another process (GUI / second MCP server).
#[test]
fn graph_cache_never_serves_stale_symbols() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn alpha_fn() -> u8 { 1 }\n").unwrap();

    let engine = TrackerEngine::new(root);
    let found = |e: &TrackerEngine, name: &str| {
        !e.read_symbol(name, false, 0)
            .unwrap()
            .text
            .contains("NOT FOUND IN INDEX")
    };

    assert!(found(&engine, "alpha_fn"));
    assert!(!found(&engine, "beta_fn"));
    // Served from cache: still correct
    assert!(found(&engine, "alpha_fn"));

    // Edit in place (different size, so detection does not depend on mtime resolution)
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha_fn() -> u8 { 1 }\npub fn beta_fn() -> u8 { 2 }\n",
    )
    .unwrap();
    assert!(found(&engine, "beta_fn"));

    // Another engine (stands in for another process) re-indexes first
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha_fn() -> u8 { 1 }\npub fn beta_fn() -> u8 { 2 }\npub fn gamma_fn() -> u8 { 3 }\n",
    )
    .unwrap();
    let other = TrackerEngine::new(root);
    assert!(found(&other, "gamma_fn"));
    // `engine` sees no file changes itself, but the generation moved: must reload
    assert!(found(&engine, "gamma_fn"));
}

/// A file whose timestamp changes but whose content does not (git checkout, no-op save)
/// must be re-hashed once and then skipped again, without bumping the graph generation.
#[test]
fn touched_but_unchanged_files_are_not_rehashed_every_run() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let file = root.join("src/lib.rs");
    std::fs::write(&file, "pub fn stable_fn() -> u8 { 1 }\n").unwrap();

    let first = index_repository(root, false).unwrap();
    assert_eq!(first.total_files, 1);

    // Touch to a different, settled (past) timestamp without changing the content
    let touched_at = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(touched_at)
        .unwrap();

    let touched = index_repository(root, false).unwrap();
    assert_eq!(touched.parsed_files, 0);
    assert_eq!(
        touched.generation, first.generation,
        "content unchanged: no graph rebuild"
    );

    let again = index_repository(root, false).unwrap();
    assert_eq!(
        again.skipped_files, 1,
        "refreshed mtime lets the fast path skip the file"
    );
}

/// A run that wrote file rows but died before rebuilding edges must not lose those edges
/// forever: the next (otherwise no-op) run notices and repairs them.
#[test]
fn interrupted_edge_rebuild_is_repaired_on_next_run() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write_call_graph_repo(root, 3);
    let full = index_repository(root, false).unwrap();
    assert!(full.total_edges > 0);

    // Simulate the crash window: file rows advanced, edges wiped and never rebuilt.
    // White-box on purpose; the row-count asserts make a schema change fail loudly here
    // instead of silently turning this into a no-op test.
    let conn = open_existing_index(root);
    let bumped = conn
        .execute(
            "UPDATE meta SET value = value + 1 WHERE key = 'files_generation'",
            [],
        )
        .unwrap();
    assert_eq!(
        bumped, 1,
        "meta.files_generation row missing: schema changed?"
    );
    let wiped = conn.execute("DELETE FROM edges", []).unwrap();
    assert_eq!(
        wiped, full.total_edges,
        "edges table out of sync with stats"
    );
    drop(conn);

    let repaired = index_repository(root, false).unwrap();
    assert_eq!(repaired.parsed_files, 0);
    assert_eq!(repaired.total_edges, full.total_edges);
}

/// End-to-end convergence check: several indexers racing while files change must end on the
/// same edge set as a clean full rebuild.
///
/// Scheduling decides which interleavings actually happen, so this test alone does not prove
/// the stale-snapshot guard; `db::tests::stale_edge_snapshot_cannot_overwrite_newer_state`
/// (deterministic) and `interrupted_edge_rebuild_is_repaired_on_next_run` do.
#[test]
fn concurrent_indexers_never_leave_stale_edges() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    write_call_graph_repo(&root, 1);
    index_repository(&root, false).unwrap();

    let errors = std::sync::Mutex::new(Vec::<String>::new());
    std::thread::scope(|s| {
        for _ in 0..4 {
            let root = root.clone();
            let errors = &errors;
            s.spawn(move || {
                for _ in 0..6 {
                    if let Err(e) = index_repository(&root, false) {
                        errors.lock().unwrap().push(format!("{e:#}"));
                    }
                }
            });
        }
        for n in 2..8 {
            write_call_graph_repo(&root, n);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });

    // The only acceptable failure is the documented retryable "index kept changing" error.
    for e in errors.into_inner().unwrap() {
        assert!(
            e.contains("kept changing"),
            "unexpected indexing error: {e}"
        );
    }

    let settled = index_repository(&root, false).unwrap();
    let settled_edges = edge_set(&root);
    let clean = index_repository(&root, true).unwrap();
    assert_eq!(settled.total_symbols, clean.total_symbols);
    // Compare the actual edges, not just their count: a wrong set with the same size must fail.
    assert_eq!(settled_edges, edge_set(&root));
    assert_eq!(settled_edges.len(), clean.total_edges);
}

/// "Racy" edit: two same-size versions of a file written within the same second share the
/// same (second-resolution) mtime and size. The second version must still be indexed.
#[test]
fn same_second_same_size_edit_is_not_missed() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let file = root.join("src/lib.rs");
    // A stamp that is never "in the past" during this test, so both writes are racy no matter
    // how slow the machine is (a stamp of `now` would depend on the first index finishing
    // within a second).
    let stamp = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    let write_with_stamp = |body: &str| {
        std::fs::write(&file, body).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
    };

    write_with_stamp("pub fn alpha_v1() -> u8 { 1 }\n");
    index_repository(root, false).unwrap();
    // Same length, same mtime: invisible to a naive mtime+size fast path
    write_with_stamp("pub fn omega_v2() -> u8 { 2 }\n");
    let second = index_repository(root, false).unwrap();

    assert_eq!(second.parsed_files, 1, "racy edit was skipped");
    let engine = TrackerEngine::new(root);
    assert!(
        !engine
            .read_symbol("omega_v2", false, 0)
            .unwrap()
            .text
            .contains("NOT FOUND IN INDEX")
    );
}

/// Once a file's mtime is safely in the past, the mtime+size fast path skips it again
/// (racy handling must not turn into re-hashing every file on every run).
#[test]
fn settled_files_take_the_fast_path_again() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let file = root.join("src/lib.rs");
    std::fs::write(&file, "pub fn settled_fn() -> u8 { 1 }\n").unwrap();
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(past)
        .unwrap();

    index_repository(root, false).unwrap();
    let again = index_repository(root, false).unwrap();
    assert_eq!(again.skipped_files, 1);
}

/// An index written by another extractor revision is re-parsed in full: unchanged files
/// must not keep the references an older extractor produced (stale edges, all tools).
#[test]
fn index_from_another_extractor_revision_is_reparsed_in_full() {
    use trackertop::db::Database;
    use trackertop::indexer::{EXTRACTOR_REVISION, index_repository};
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "pub fn a() -> i32 { b() }\n").unwrap();
    std::fs::write(root.join("src/b.rs"), "pub fn b() -> i32 { 1 }\n").unwrap();
    let first = index_repository(root, false).unwrap();
    assert_eq!(first.parsed_files, 2);
    let db = Database::open_for_repo(root).unwrap();
    assert_eq!(db.extractor_revision().unwrap(), EXTRACTOR_REVISION);
    assert_eq!(index_repository(root, false).unwrap().parsed_files, 0);

    // Simulate an index from an older TrackerTop, with a file deleted meanwhile
    db.set_extractor_revision(EXTRACTOR_REVISION - 1).unwrap();
    std::fs::remove_file(root.join("src/b.rs")).unwrap();
    let upgraded = index_repository(root, false).unwrap();
    assert_eq!(upgraded.parsed_files, 1, "the unchanged file is re-parsed");
    assert_eq!(upgraded.deleted_files, 1, "deletions are still detected");
    assert_eq!(db.extractor_revision().unwrap(), EXTRACTOR_REVISION);
    assert_eq!(index_repository(root, false).unwrap().parsed_files, 0);
}

/// A full re-parse drops the rows of a file it could not re-parse (here: no longer valid
/// UTF-8), instead of keeping the old extractor's symbols and marking the index current.
#[test]
fn full_reparse_drops_files_it_could_not_reparse() {
    use trackertop::db::Database;
    use trackertop::indexer::{EXTRACTOR_REVISION, index_repository};
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "pub fn a() -> i32 { b() }\n").unwrap();
    std::fs::write(root.join("src/b.rs"), "pub fn b() -> i32 { 1 }\n").unwrap();
    index_repository(root, false).unwrap();
    let db = Database::open_for_repo(root).unwrap();
    db.set_extractor_revision(EXTRACTOR_REVISION - 1).unwrap();
    std::fs::write(root.join("src/b.rs"), [0xff, 0xfe, b'f', b'n']).unwrap();
    index_repository(root, false).unwrap();
    let files = db.load_file_map().unwrap();
    assert!(files.contains_key("src/a.rs"));
    assert!(
        !files.contains_key("src/b.rs"),
        "rows from the old extractor survived a full re-parse"
    );
}

/// The desktop status bar shows "Indexed N ago" and "Indexing… done/total files": both
/// come from the run itself, so a finished run must record its time and report every
/// changed file as parsed, and an unchanged re-run still counts as a fresh check.
#[test]
fn index_runs_record_their_time_and_progress() {
    use trackertop::db::Database;
    use trackertop::indexer::{IndexProgress, IndexStage, index_repository_with_progress};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    for i in 0..3 {
        std::fs::write(
            root.join(format!("src/m{i}.rs")),
            format!("pub fn f{i}() {{}}\n"),
        )
        .unwrap();
    }

    let progress = IndexProgress::default();
    assert_eq!(progress.stage(), IndexStage::Scanning);
    let before = chrono::Utc::now().timestamp();
    index_repository_with_progress(root, false, &progress).unwrap();
    let after = chrono::Utc::now().timestamp();
    // Parsing finished; the run ended in the linking stage
    assert_eq!(progress.stage(), IndexStage::Linking);

    let db = Database::open_for_repo(root).unwrap();
    let first = db.indexed_at().unwrap().expect("indexed_at recorded");
    assert!(
        (before..=after).contains(&first),
        "{first} not in {before}..={after}"
    );

    // Unchanged re-run: nothing to parse, but the check time moves forward
    let again = IndexProgress::default();
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    index_repository_with_progress(root, false, &again).unwrap();
    assert!(db.indexed_at().unwrap().unwrap() > first);
}

#[test]
fn parsing_progress_counts_every_changed_file() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use trackertop::indexer::{IndexProgress, IndexStage, index_repository_with_progress};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    for i in 0..40 {
        std::fs::write(
            root.join(format!("src/m{i}.rs")),
            format!("pub fn f{i}() {{}}\n"),
        )
        .unwrap();
    }
    let progress = Arc::new(IndexProgress::default());
    let stop = Arc::new(AtomicBool::new(false));
    // Sample the progress from another thread while the run goes, like the UI does
    let sampler = {
        let (p, stop) = (progress.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                if let IndexStage::Parsing { done, total } = p.stage() {
                    seen.push((done, total));
                }
                std::thread::yield_now();
            }
            seen
        })
    };
    index_repository_with_progress(&root, false, &progress).unwrap();
    stop.store(true, Ordering::Relaxed);
    let seen = sampler.join().unwrap();
    assert!(
        seen.iter().all(|&(d, t)| t == 40 && d <= t),
        "every sample is within 0..=40 of 40: {seen:?}"
    );
    assert_eq!(progress.files(), (40, 40), "every changed file was counted");
}

/// The app shows a symbol count in its header and in its status bar (from the project
/// snapshot): both must count the same thing, code symbols without file rows and `impl`
/// blocks, or the same screen shows two different numbers.
#[test]
fn project_snapshot_counts_code_symbols_only() {
    use trackertop::registry::{ProjectEntry, project_snapshot};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub struct S;\nimpl S {\n    pub fn m(&self) {}\n}\npub fn f() {}\n",
    )
    .unwrap();
    index_repository(root, false).unwrap();

    let snap = project_snapshot(ProjectEntry {
        path: root.to_string_lossy().to_string(),
        name: "p".to_string(),
        added_at: String::new(),
        last_seen: String::new(),
    });
    assert!(snap.indexed);
    assert_eq!(snap.total_files, 1);
    // S, S::m, f — not the file row, not the impl block
    assert_eq!(snap.total_symbols, 3);
}
