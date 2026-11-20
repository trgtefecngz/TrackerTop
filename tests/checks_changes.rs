//! run_checks, changes_since and the project playbook.

mod common;

use common::*;
use serde_json::json;
use tempfile::TempDir;
use trackertop::context::TrackerEngine;
use trackertop::mcp::handle_jsonrpc_message;

#[test]
fn run_checks_summarizes_a_real_cargo_test_run() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"rc_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn passes() { assert_eq!(super::add(1, 2), 3); }\n    #[test]\n    fn fails() { assert_eq!(super::add(2, 2), 5); }\n}\n",
    )
    .unwrap();
    let engine = TrackerEngine::new(root);

    let out = engine.run_checks("test", None, 600, 4_000).unwrap().text;
    assert!(out.contains("RESULT: FAILED (exit 101)"), "{out}");
    assert!(out.contains("1 passed, 1 failed"), "{out}");
    assert!(out.contains("- tests::fails"), "{out}");
    assert!(out.contains("lib.rs:8:"), "{out}");
    assert!(!out.contains("- tests::passes"), "{out}");

    let filtered = engine
        .run_checks("test", Some("passes"), 600, 4_000)
        .unwrap()
        .text;
    assert!(filtered.contains("RESULT: PASSED"), "{filtered}");
    assert!(filtered.contains("`cargo test passes`"), "{filtered}");

    // Ambiguous extra args and unknown kinds are errors, not empty results.
    assert!(engine.run_checks("all", Some("x"), 60, 1_000).is_err());
    let resp = handle_jsonrpc_message(
        &engine,
        &json!({"jsonrpc":"2.0","id":9,"method":"tools/call",
                "params":{"name":"run_checks","arguments":{"kind":"bogus"}}}),
    )
    .unwrap();
    assert_eq!(resp["result"]["isError"], json!(true), "{resp}");
}

#[test]
fn run_checks_says_when_no_command_is_detected() {
    let tmp = TempDir::new().unwrap();
    let engine = TrackerEngine::new(tmp.path());
    let out = engine.run_checks("test", None, 60, 1_000).unwrap().text;
    assert!(out.contains("test: NOT DETECTED"), "{out}");
}

#[test]
fn changes_since_maps_working_tree_diffs_to_symbols() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha() -> i32 {\n    1\n}\n\npub fn beta() -> i32 {\n    2\n}\n\npub fn gamma() -> i32 {\n    3\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("src/keep.rs"), "pub fn keep() {}\n").unwrap();
    git_in(root, &["init", "-q"]);
    git_in(root, &["add", "."]);
    git_in(root, &["commit", "-q", "-m", "init"]);

    // beta edited, gamma deleted, delta added; one untracked file
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha() -> i32 {\n    1\n}\n\npub fn beta() -> i32 {\n    20 + 2\n}\n\npub fn delta() -> i32 {\n    4\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("src/fresh.rs"), "pub fn fresh() {}\n").unwrap();
    let engine = TrackerEngine::new(root);

    let out = engine.changes_since("HEAD", &[], &[], 4_000).unwrap().text;
    assert!(out.contains("## src/lib.rs (modified"), "{out}");
    assert!(out.contains("Touched: beta [fn] L5-7"), "{out}");
    assert!(out.contains("Added symbols: delta"), "{out}");
    assert!(out.contains("Removed symbols: gamma"), "{out}");
    assert!(out.contains("+    20 + 2"), "{out}");
    assert!(out.contains("-    2"), "{out}");
    assert!(
        out.contains("## Untracked new files (1)\n- src/fresh.rs"),
        "{out}"
    );
    assert!(!out.contains("keep.rs"), "{out}");
    assert!(!out.contains(".trackertop"), "{out}");

    let beta = engine
        .changes_since("HEAD", &[], &["beta".to_string()], 4_000)
        .unwrap()
        .text;
    assert!(beta.contains("+    20 + 2"), "{beta}");
    assert!(!beta.contains("delta"), "only hunks touching beta: {beta}");
    let alpha = engine
        .changes_since("HEAD", &[], &["alpha".to_string()], 4_000)
        .unwrap()
        .text;
    assert!(
        alpha.contains("No changes touch the requested symbols"),
        "{alpha}"
    );
    let gone = engine
        .changes_since("HEAD", &[], &["gamma".to_string()], 4_000)
        .unwrap()
        .text;
    assert!(gone.contains("SYMBOL `gamma` NOT FOUND IN INDEX"), "{gone}");

    let scoped = engine
        .changes_since("HEAD", &["src/keep.rs".to_string()], &[], 4_000)
        .unwrap()
        .text;
    assert!(
        scoped.contains("No changes since that commit in the requested scope"),
        "{scoped}"
    );

    let bad = engine
        .changes_since("no-such-ref", &[], &[], 1_000)
        .unwrap_err();
    assert!(
        format!("{bad:#}").contains("does not name a commit"),
        "{bad:#}"
    );
    let plain = TempDir::new().unwrap();
    let err = TrackerEngine::new(plain.path())
        .changes_since("HEAD", &[], &[], 1_000)
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("not a git repository"),
        "{err:#}"
    );
}

#[test]
fn project_playbook_is_served_over_mcp() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("package.json"),
        r#"{"scripts":{"test":"vitest run"},"engines":{"node":">=20"}}"#,
    )
    .unwrap();
    std::fs::write(tmp.path().join("CLAUDE.md"), "rules\n").unwrap();
    let engine = TrackerEngine::new(tmp.path());
    let resp = handle_jsonrpc_message(
        &engine,
        &json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
                "params":{"name":"project_playbook","arguments":{}}}),
    )
    .unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("READ FIRST (instructions for agents): CLAUDE.md"),
        "{text}"
    );
    assert!(
        text.contains("test: `npm run test` [detected from package.json scripts.test]"),
        "{text}"
    );
    assert!(
        text.contains("Node >=20 [detected from package.json engines.node]"),
        "{text}"
    );
}

/// A project indexed from a subdirectory of a git repository (a monorepo package) must see
/// its own changes with project-relative paths, and nothing from sibling packages.
#[test]
fn changes_since_works_when_the_project_is_a_git_subdirectory() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let app = repo.join("packages/app");
    std::fs::create_dir_all(app.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("packages/other")).unwrap();
    std::fs::write(
        app.join("src/lib.rs"),
        "pub fn alpha() -> i32 {\n    1\n}\n\npub fn gamma() -> i32 {\n    3\n}\n",
    )
    .unwrap();
    std::fs::write(repo.join("packages/other/x.rs"), "pub fn x() {}\n").unwrap();
    git_in(repo, &["init", "-q"]);
    git_in(repo, &["add", "."]);
    git_in(repo, &["commit", "-q", "-m", "init"]);

    std::fs::write(
        app.join("src/lib.rs"),
        "pub fn alpha() -> i32 {\n    10\n}\n\npub fn delta() -> i32 {\n    4\n}\n",
    )
    .unwrap();
    std::fs::write(repo.join("packages/other/x.rs"), "pub fn x() { }\n").unwrap();
    let engine = TrackerEngine::new(&app);

    let out = engine.changes_since("HEAD", &[], &[], 4_000).unwrap().text;
    assert!(out.contains("## src/lib.rs (modified"), "{out}");
    assert!(out.contains("Touched: alpha [fn] L1-3"), "{out}");
    assert!(out.contains("Added symbols: delta"), "{out}");
    assert!(out.contains("Removed symbols: gamma"), "{out}");
    assert!(!out.contains("other/x.rs"), "{out}");
}
