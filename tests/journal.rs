//! Cross-session continuity: the work journal, resume and the session-start briefing.

mod common;

use common::*;
use serde_json::json;
use tempfile::TempDir;
use trackertop::context::TrackerEngine;
use trackertop::mcp::handle_jsonrpc_message;

/// One MCP `tools/call`, returning the text of the answer.
fn call(engine: &TrackerEngine, id: u64, tool: &str, args: serde_json::Value) -> String {
    let resp = handle_jsonrpc_message(
        engine,
        &json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                "params":{"name":tool,"arguments":args}}),
    )
    .unwrap();
    resp.pointer("/result/content/0/text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("no text in {resp}"))
        .to_string()
}

/// Cross-session continuity, end to end: two agent sessions work through MCP (calls are
/// journaled, explicit progress / tried / blocked entries are written, a review raises
/// findings, a decision's code changes), then a third session's `resume` and the
/// SessionStart briefing report all of it with dates and file:line.
#[test]
fn resume_reports_what_earlier_sessions_did() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn old_api() -> i32 { 1 }\npub fn caller() -> i32 {\n    old_api()\n}\npub fn parse() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("docs/V0.9_PROGRESS.md"),
        "# v0.9\n## Run log\n- old stuff\n## Next\n1. wire the parser into `src/lib.rs::parse`\n2. ship\n## Kararını bekleyenler\n- merge PR #9?\n",
    )
    .unwrap();
    git_in(root, &["init", "-q", "-b", "main"]);
    git_in(root, &["add", "."]);
    git_in(root, &["commit", "-q", "-m", "base"]);
    git_in(root, &["checkout", "-q", "-b", "work"]);

    // Session 1: reads code, records a decision on `parse`, a failed approach and blockers
    let s1 = TrackerEngine::new(root);
    s1.set_session("20261120T090000Z#111");
    handle_jsonrpc_message(
        &s1,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"clientInfo":{"name":"claude-code","version":"1"}}}),
    )
    .unwrap();
    call(
        &s1,
        2,
        "read_symbol",
        json!({"symbol": "parse", "max_tokens": 500}),
    );
    call(&s1, 3, "read_symbol", json!({"symbol": "parse"}));
    call(
        &s1,
        4,
        "read_symbol",
        json!({"symbol": "no_such_symbol_zz"}),
    );
    call(
        &s1,
        5,
        "remember",
        json!({"target": "parse", "note": "parse stays pure", "kind": "decision", "reason": "callers cache it"}),
    );
    call(
        &s1,
        6,
        "remember",
        json!({"note": "regex-based parser", "kind": "tried", "reason": "too slow on big files"}),
    );
    call(
        &s1,
        7,
        "remember",
        json!({"target": "src/lib.rs", "note": "parser rewrite", "kind": "blocked", "reason": "waiting for grammar crate release"}),
    );
    call(
        &s1,
        8,
        "remember",
        json!({"note": "CI quota", "kind": "blocked", "reason": "runner minutes used up"}),
    );
    let rejected = call(&s1, 9, "remember", json!({"note": "x", "kind": "tried"}));
    assert!(rejected.contains("needs a `reason`"), "{rejected}");

    // Session 2: removes old_api (a broken ref), reviews, closes the CI blocker by id, and
    // changes `parse` so session 1's decision on it is now possibly outdated
    let s2 = TrackerEngine::new(root);
    s2.set_session("20261120T110000Z#222");
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn caller() -> i32 {\n    old_api()\n}\npub fn parse() -> i32 {\n    3\n}\n",
    )
    .unwrap();
    let review = call(&s2, 1, "review_changes", json!({"base": "main"}));
    assert!(review.contains("broken-ref:src/lib.rs:old_api"), "{review}");
    let ci = trackertop::db::Database::open_for_repo(root)
        .unwrap()
        .list_notes(Some("CI quota"))
        .unwrap();
    let ci_id = ci[0].id;
    call(
        &s2,
        2,
        "remember",
        json!({"note": format!("CI back, closes #{ci_id}"), "kind": "progress"}),
    );

    // Session 3 starts fresh
    let s3 = TrackerEngine::new(root);
    s3.set_session("20261120T130000Z#333");
    let out = call(&s3, 1, "resume", json!({}));

    // Uncommitted work with touched symbols and lines
    assert!(out.contains("Uncommitted work on `work`"), "{out}");
    assert!(out.contains("M src/lib.rs"), "{out}");
    assert!(out.contains("parse :4"), "{out}");
    // Progress file sections with file:line; other sections are not lifted
    assert!(out.contains("## Next (docs/V0.9_PROGRESS.md:4)"), "{out}");
    assert!(out.contains("1. wire the parser"), "{out}");
    assert!(
        out.contains("## Kararını bekleyenler (docs/V0.9_PROGRESS.md:7)"),
        "{out}"
    );
    assert!(!out.contains("old stuff"), "{out}");
    // Blocked: the CI blocker was closed by id, the parser blocker is open
    assert!(out.contains("## Blocked (1 open)"), "{out}");
    assert!(
        out.contains("parser rewrite (why: waiting for grammar crate release)"),
        "{out}"
    );
    assert!(!out.contains("runner minutes used up"), "{out}");
    // Tried and failed
    assert!(
        out.contains("regex-based parser (why: too slow on big files)"),
        "{out}"
    );
    // Open review finding with file:line
    assert!(out.contains("`broken-ref:src/lib.rs:old_api`"), "{out}");
    assert!(out.contains("(was src/lib.rs:L1-1)"), "{out}");
    // The decision is flagged because `parse` changed after it was written
    assert!(out.contains("Decisions (1, 1 POSSIBLY OUTDATED)"), "{out}");
    assert!(out.contains("parse stays pure"), "{out}");
    // Progress log
    assert!(out.contains(&format!("CI back, closes #{ci_id}")), "{out}");
    // Sessions, newest first, with the client, the focus, results and the failed lookup
    let s2_at = out.find("review_changes×1").expect(&out);
    let s1_at = out.find("read_symbol×3").expect(&out);
    assert!(s2_at < s1_at, "newest session first: {out}");
    assert!(out.contains("(mcp:claude-code)"), "{out}");
    assert!(out.contains("focus: parse"), "{out}");
    assert!(out.contains("last review_changes"), "{out}");
    // Every item is dated
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    assert!(out.matches(&today).count() >= 6, "{out}");

    // The SessionStart briefing: short, found from the stdin cwd, has the essentials
    let input = json!({"cwd": root.join("src").to_string_lossy(), "source": "compact"}).to_string();
    let brief = trackertop::hook::hook_output(&input, None, std::time::Duration::from_secs(20));
    assert!(brief.starts_with(trackertop::hook::HOOK_HINT), "{brief}");
    assert!(brief.contains("Context was just compacted"), "{brief}");
    assert!(brief.contains("1. wire the parser"), "{brief}");
    assert!(brief.contains("parser rewrite"), "{brief}");
    assert!(brief.contains("POSSIBLY OUTDATED"), "{brief}");
    let toks = trackertop::tokens::count_tokens(&brief);
    assert!(toks <= 600, "briefing is {toks} tokens:\n{brief}");
}

/// The journal never creates an index where there is none, records MCP calls with their
/// session, and its rows become timeline items for the desktop app.
#[test]
fn journal_rows_feed_the_timeline() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    let e = TrackerEngine::new(root);
    e.journal_call("read_symbol", "symbol=a", "ok").unwrap();
    assert!(
        !root.join(".trackertop").exists(),
        "journal must not create an index"
    );
    e.set_session("s1");
    call(&e, 1, "read_symbol", json!({"symbol": "a"}));
    call(&e, 2, "smart_grep", json!({"pattern": "fn"}));
    let db = trackertop::db::Database::open_for_repo(root).unwrap();
    let (rows, total) = db.recent_journal(10).unwrap();
    assert_eq!(total, 2);
    assert_eq!(rows[0].tool, "smart_grep");
    assert_eq!(rows[1].query, "symbol=a");
    assert_eq!(rows[1].session, "s1");
    let t = trackertop::journal::timeline(&rows, &[], 10);
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].title, "2 calls: read_symbol×1, smart_grep×1");
}

/// A progress file can outlive the work it describes: its `## Next` still says "awaiting
/// merge" after the PR was merged. `resume` marks such a section POSSIBLY OUTDATED, with
/// the git facts behind it (a mentioned PR found merged in git history, a merge awaited by
/// a file that is already on the default branch, commits made after the file last
/// changed). A current file is not marked.
#[test]
fn resume_marks_progress_sections_git_has_overtaken() {
    let progress = "# v0.1\n## Next\n1. Awaiting merge of PR #12.\n2. Then tag the release.\n\
                    ## Kararını bekleyenler\n- Should PR #12 be squashed?\n";
    let marked = |out: &str, heading: &str| -> bool {
        out.lines()
            .any(|l| l.starts_with(&format!("## {heading} (")) && l.contains("POSSIBLY OUTDATED"))
    };

    // Stale: PR #12 merged on main and more commits followed
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("docs/V0.1_PROGRESS.md"), progress).unwrap();
    std::fs::write(root.join("a.txt"), "0").unwrap();
    git_in(root, &["init", "-q", "-b", "main"]);
    git_in(root, &["add", "."]);
    git_in(root, &["commit", "-q", "-m", "progress"]);
    for (i, msg) in ["Parser rewrite (#12)", "fix a", "fix b", "release: 0.1.0"]
        .iter()
        .enumerate()
    {
        std::fs::write(root.join("a.txt"), (i + 1).to_string()).unwrap();
        git_in(root, &["commit", "-q", "-am", msg]);
    }
    let out = TrackerEngine::new(root).resume(6_000).unwrap().text;
    assert!(marked(&out, "Next"), "{out}");
    assert!(marked(&out, "Kararını bekleyenler"), "{out}");
    assert!(
        out.contains("PR #12 is already merged (`Parser rewrite (#12)`)"),
        "{out}"
    );
    assert!(
        out.contains("4 commits since docs/V0.1_PROGRESS.md last changed"),
        "{out}"
    );

    // Awaits a merge, but the file's last change is already on main (no PR number)
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(
        root.join("docs/V0.1_PROGRESS.md"),
        "## Next\n1. All done; remaining before merge: nothing.\n",
    )
    .unwrap();
    git_in(root, &["init", "-q", "-b", "main"]);
    git_in(root, &["add", "."]);
    git_in(root, &["commit", "-q", "-m", "progress"]);
    let out = TrackerEngine::new(root).resume(6_000).unwrap().text;
    assert!(marked(&out, "Next"), "{out}");
    assert!(out.contains("already on `main`"), "{out}");

    // Current: written on a work branch, nothing merged, no later commits
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("a.txt"), "0").unwrap();
    git_in(root, &["init", "-q", "-b", "main"]);
    git_in(root, &["add", "."]);
    git_in(root, &["commit", "-q", "-m", "base"]);
    git_in(root, &["checkout", "-q", "-b", "work"]);
    std::fs::write(root.join("docs/V0.1_PROGRESS.md"), progress).unwrap();
    git_in(root, &["add", "."]);
    git_in(root, &["commit", "-q", "-m", "progress"]);
    let out = TrackerEngine::new(root).resume(6_000).unwrap().text;
    assert!(out.contains("## Next (docs/V0.1_PROGRESS.md:2)"), "{out}");
    assert!(!out.contains("POSSIBLY OUTDATED"), "{out}");
}
