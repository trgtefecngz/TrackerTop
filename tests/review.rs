//! review_changes: changed symbols and every review rule.

mod common;

use common::*;
use tempfile::TempDir;
use trackertop::context::TrackerEngine;

/// `review` step 1: every kind of change maps to the right symbols on the right side,
/// including file renames, deletions, untracked files, and a project that is a
/// subdirectory of its git repository.
#[test]
fn review_lists_changed_symbols_for_every_kind_of_change() {
    use trackertop::review::{Change, review};
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let root = repo.join("pkg");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(repo.join("sibling.rs"), "pub fn sibling() {}\n").unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn keep() -> i32 {\n    1\n}\n\npub fn edit() -> i32 {\n    2\n}\n\npub fn drop_me() -> i32 {\n    3\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/old_name.rs"),
        "pub fn moved() -> i32 {\n    4\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("src/gone.rs"), "pub fn gone() {}\n").unwrap();
    git_in(repo, &["init", "-q", "-b", "main"]);
    git_in(repo, &["add", "."]);
    git_in(repo, &["commit", "-q", "-m", "base"]);
    git_in(repo, &["checkout", "-q", "-b", "work"]);

    // Committed on the branch: edit() changes, drop_me() goes, fresh() arrives
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn keep() -> i32 {\n    1\n}\n\npub fn edit() -> i32 {\n    20\n}\n\npub fn fresh() -> i32 {\n    5\n}\n",
    )
    .unwrap();
    git_in(repo, &["mv", "pkg/src/old_name.rs", "pkg/src/new_name.rs"]);
    git_in(repo, &["rm", "-q", "pkg/src/gone.rs"]);
    git_in(repo, &["commit", "-q", "-am", "work"]);
    // Uncommitted: an untracked file, and a change outside the project
    std::fs::write(root.join("src/extra.rs"), "pub fn extra() {}\n").unwrap();
    std::fs::write(repo.join("sibling.rs"), "pub fn sibling() { }\n").unwrap();
    // The base branch moves on after the fork: the review must use the merge base
    git_in(repo, &["checkout", "-q", "main"]);
    std::fs::write(root.join("src/later.rs"), "pub fn later() {}\n").unwrap();
    git_in(repo, &["add", "pkg/src/later.rs"]);
    git_in(
        repo,
        &[
            "commit",
            "-q",
            "-m",
            "main moves on",
            "--",
            "pkg/src/later.rs",
        ],
    );
    git_in(repo, &["checkout", "-q", "work"]);

    let cs = review(&root, "main").unwrap();
    let got: Vec<(String, Change)> = cs
        .symbols
        .iter()
        .map(|s| (s.qualified_name.clone(), s.change))
        .collect();
    let has = |name: &str, change: Change| got.iter().any(|(n, c)| n == name && *c == change);
    assert!(has("edit", Change::Modified), "{got:?}");
    assert!(has("fresh", Change::Added), "{got:?}");
    assert!(has("drop_me", Change::Removed), "{got:?}");
    assert!(has("gone", Change::Removed), "{got:?}");
    assert!(has("extra", Change::Added), "{got:?}");
    // Unchanged symbols, moved-but-unchanged code and other packages are not listed
    for n in ["keep", "moved", "sibling", "later"] {
        assert!(!got.iter().any(|(g, _)| g == n), "{n}: {got:?}");
    }
    let renamed = cs.files.iter().find(|f| f.status == "renamed").unwrap();
    assert_eq!(
        (renamed.old_path.as_deref(), renamed.path.as_str()),
        (Some("src/old_name.rs"), "src/new_name.rs")
    );
    let removed = cs
        .symbols
        .iter()
        .find(|s| s.qualified_name == "drop_me")
        .unwrap();
    assert_eq!(removed.base.as_ref().unwrap().start_line, 9);
    assert!(removed.head.is_none());

    // The user's branch, index and working tree are untouched by the base checkout
    let status = std::process::Command::new("git")
        .current_dir(repo)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    let status = String::from_utf8_lossy(&status.stdout);
    assert_eq!(status.lines().count(), 2, "{status}");
    let wt = std::process::Command::new("git")
        .current_dir(repo)
        .args(["worktree", "list"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&wt.stdout).lines().count(), 1);

    // A second run reuses the cached snapshot and gives the same answer
    let again = review(&root, "main").unwrap();
    assert_eq!(again.symbols, cs.symbols);
}

/// Review rules on a fixture: `broken-ref` only when a surviving caller still calls the
/// removed symbol (verified against the working tree), `impact` for a widely used
/// function, `untested` for new code no test reaches. Stable ids.
#[test]
fn review_rules_flag_broken_refs_impact_and_untested_code() {
    use trackertop::review::review;
    use trackertop::review::rules::Severity;
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let mut lib = String::from(
        "pub struct Store;\nimpl Store {\n    pub fn open() -> Store { Store }\n}\n\
         pub fn old_api() -> i32 { 1 }\npub fn renamed_away() -> i32 { 2 }\n\
         pub fn hub(x: i32) -> i32 {\n    x + 1\n}\n",
    );
    for k in 0..10 {
        lib.push_str(&format!("pub fn user_{k}() -> i32 {{ hub({k}) }}\n"));
    }
    lib.push_str(
        "pub fn keeps_calling() -> i32 {\n    old_api() + 1\n}\n\
         pub fn opens() {\n    let _s = Store::open();\n}\n\
         pub fn was_updated() -> i32 {\n    renamed_away()\n}\n\
         #[test]\nfn hub_works() {\n    assert_eq!(hub(1), 2);\n}\n",
    );
    std::fs::write(root.join("src/lib.rs"), &lib).unwrap();
    git_in(root, &["init", "-q", "-b", "main"]);
    git_in(root, &["add", "."]);
    git_in(root, &["commit", "-q", "-m", "base"]);
    git_in(root, &["checkout", "-q", "-b", "work"]);

    let new_lib = lib
        .replace("pub fn old_api() -> i32 { 1 }\n", "")
        .replace("    pub fn open() -> Store { Store }\n", "")
        .replace(
            "pub fn renamed_away() -> i32 { 2 }\n",
            "pub fn new_name() -> i32 { 2 }\n",
        )
        .replace("    renamed_away()\n", "    new_name()\n")
        .replace("    x + 1\n", "    x + 2\n")
        + "pub fn brand_new() -> i32 {\n    7\n}\n";
    std::fs::write(root.join("src/lib.rs"), new_lib).unwrap();

    let cs = review(root, "main").unwrap();
    let ids: Vec<&str> = cs.findings.iter().map(|f| f.id.as_str()).collect();
    let get = |id: &str| cs.findings.iter().find(|f| f.id == id);

    let broken = get("broken-ref:src/lib.rs:old_api").unwrap_or_else(|| panic!("{ids:?}"));
    assert_eq!(broken.severity, Severity::High);
    assert!(
        broken.evidence[0].starts_with("src/lib.rs:")
            && broken.evidence[0].contains("old_api() + 1"),
        "{:?}",
        broken.evidence
    );
    let open = get("broken-ref:src/lib.rs:Store::open").unwrap_or_else(|| panic!("{ids:?}"));
    assert_eq!(open.severity, Severity::High);
    // Removed, but its only caller was updated: not a broken reference
    assert!(
        get("broken-ref:src/lib.rs:renamed_away").is_none(),
        "{ids:?}"
    );

    let hub = get("impact:src/lib.rs:hub").unwrap_or_else(|| panic!("{ids:?}"));
    assert!(hub.message.contains("10 direct"), "{}", hub.message);
    assert!(
        hub.evidence.iter().any(|e| e.contains("hub_works")),
        "{:?}",
        hub.evidence
    );

    assert!(get("untested:src/lib.rs:brand_new").is_some(), "{ids:?}");
    // hub is reached by a test through `assert_eq!(hub(1), 2)`
    assert!(get("untested:src/lib.rs:hub").is_none(), "{ids:?}");

    // Stable: a second run yields the same ids
    let again = review(root, "main").unwrap();
    assert_eq!(
        again.findings.iter().map(|f| &f.id).collect::<Vec<_>>(),
        cs.findings.iter().map(|f| &f.id).collect::<Vec<_>>()
    );
}

/// `review_changes` (the agent's MCP entry point): findings first; a finding dismissed with
/// `remember(kind=dismissed)` is not raised again but is still listed with its reason.
#[test]
fn review_changes_hides_dismissed_findings_and_says_so() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn old_api() -> i32 { 1 }\npub fn caller() -> i32 {\n    old_api()\n}\n",
    )
    .unwrap();
    git_in(root, &["init", "-q", "-b", "main"]);
    git_in(root, &["add", "."]);
    git_in(root, &["commit", "-q", "-m", "base"]);
    git_in(root, &["checkout", "-q", "-b", "work"]);
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn caller() -> i32 {\n    old_api()\n}\n",
    )
    .unwrap();
    let engine = TrackerEngine::new(root);

    let out = engine.review_changes("", 4_000).unwrap().text;
    let id = "broken-ref:src/lib.rs:old_api";
    assert!(out.contains(&format!("id: `{id}`")), "{out}");
    assert!(out.contains("src/lib.rs:2 in caller"), "{out}");
    let findings_at = out.find("## Findings").unwrap();
    let symbols_at = out.find("## Changed symbols").unwrap();
    assert!(findings_at < symbols_at, "{out}");

    engine
        .remember_kind(
            id,
            "old_api is generated by a build script",
            "dismissed",
            "checked: build.rs defines it",
        )
        .unwrap();
    let out = engine.review_changes("main", 4_000).unwrap().text;
    assert!(!out.contains(&format!("id: `{id}`")), "{out}");
    assert!(
        out.contains("Dismissed earlier, not shown again (1)"),
        "{out}"
    );
    assert!(out.contains("checked: build.rs defines it"), "{out}");
}

/// `new-cycle`: an import cycle the change creates (JS/TS) is reported with the new
/// dependency as evidence; one that already existed at the base is not.
#[test]
fn review_reports_import_cycles_the_change_creates() {
    use trackertop::review::review;
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/a.ts"),
        "import { b } from \"./b\";\nexport function a(): number {\n  return b() + 1;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/b.ts"),
        "export function b(): number {\n  return 1;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/c.ts"),
        "import { d } from \"./d\";\nexport function c(): number {\n  return d();\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/d.ts"),
        "import { c } from \"./c\";\nexport function d(): number {\n  return c();\n}\n",
    )
    .unwrap();
    git_in(root, &["init", "-q", "-b", "main"]);
    git_in(root, &["add", "."]);
    git_in(root, &["commit", "-q", "-m", "base"]);
    git_in(root, &["checkout", "-q", "-b", "work"]);
    std::fs::write(
        root.join("src/b.ts"),
        "import { a } from \"./a\";\nexport function b(): number {\n  return a() - 1;\n}\n",
    )
    .unwrap();
    // c <-> d already form a cycle at the base; touching c must not report it
    std::fs::write(
        root.join("src/c.ts"),
        "import { d } from \"./d\";\nexport function c(): number {\n  return d() + 0;\n}\n",
    )
    .unwrap();

    let cs = review(root, "main").unwrap();
    let cycles: Vec<_> = cs
        .findings
        .iter()
        .filter(|f| f.rule == "new-cycle")
        .collect();
    assert_eq!(cycles.len(), 1, "{:#?}", cs.findings);
    assert_eq!(cycles[0].id, "new-cycle:src/a.ts:2-files");
    assert!(
        cycles[0].message.contains("src/a.ts, src/b.ts"),
        "{}",
        cycles[0].message
    );
    assert!(
        cycles[0].evidence.iter().any(|e| e.starts_with("src/b.ts")),
        "{:?}",
        cycles[0].evidence
    );
}

/// `cochange-miss` reads the base's history relative to the project root (here a
/// monorepo package), and only the base's history: the branch never votes for itself.
#[test]
fn review_flags_files_that_habitually_change_together() {
    use trackertop::review::review;
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let pkg = repo.join("pkg");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    git_in(repo, &["init", "-q", "-b", "main"]);
    for i in 0..5 {
        std::fs::write(
            pkg.join("src/a.rs"),
            format!("pub fn a() -> i32 {{ {i} }}\n"),
        )
        .unwrap();
        std::fs::write(
            pkg.join("src/b.rs"),
            format!("pub fn b() -> i32 {{ {i} }}\n"),
        )
        .unwrap();
        // Outside the package: never part of its history
        std::fs::write(repo.join("other.txt"), format!("{i}\n")).unwrap();
        git_in(repo, &["add", "."]);
        git_in(repo, &["commit", "-q", "-m", &format!("step {i}")]);
    }
    git_in(repo, &["checkout", "-q", "-b", "work"]);
    std::fs::write(pkg.join("src/a.rs"), "pub fn a() -> i32 { 42 }\n").unwrap();

    let cs = review(&pkg, "main").unwrap();
    let hit = cs
        .findings
        .iter()
        .find(|f| f.rule == "cochange-miss")
        .unwrap_or_else(|| panic!("{:?}", cs.findings));
    assert_eq!(hit.id, "cochange-miss:src/b.rs");
    assert!(hit.message.contains("5 of src/a.rs's 5"), "{}", hit.message);
    assert!(hit.evidence[0].contains("step 4"), "{:?}", hit.evidence);

    // Updating the partner too clears the finding
    std::fs::write(pkg.join("src/b.rs"), "pub fn b() -> i32 { 42 }\n").unwrap();
    let cs = review(&pkg, "main").unwrap();
    assert!(
        !cs.findings.iter().any(|f| f.rule == "cochange-miss"),
        "{:?}",
        cs.findings
    );
}

/// `orphan`: a function whose last caller the change removed. Code that was already
/// dead, and a function still named somewhere (passed as a value), are not reported.
#[test]
fn review_flags_functions_the_change_left_without_callers() {
    use trackertop::review::review;
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let base = "fn helper_alpha() -> i32 { 1 }\n\
                fn mapper_beta(x: i32) -> i32 { x }\n\
                fn already_dead() -> i32 { 3 }\n\
                pub fn api_gamma() -> i32 { 4 }\n\
                pub fn run() -> i32 {\n    helper_alpha() + mapper_beta(2) + api_gamma()\n}\n";
    std::fs::write(root.join("src/lib.rs"), base).unwrap();
    git_in(root, &["init", "-q", "-b", "main"]);
    git_in(root, &["add", "."]);
    git_in(root, &["commit", "-q", "-m", "base"]);
    git_in(root, &["checkout", "-q", "-b", "work"]);
    let head = base.replace(
        "    helper_alpha() + mapper_beta(2) + api_gamma()\n",
        "    [2].iter().map(|x| mapper_beta(*x)).sum::<i32>() + [1].len() as i32\n",
    );
    let head = head.replace("mapper_beta(*x)).sum", "*x).map(mapper_beta).sum");
    std::fs::write(root.join("src/lib.rs"), head).unwrap();

    let cs = review(root, "main").unwrap();
    let orphans: Vec<&str> = cs
        .findings
        .iter()
        .filter(|f| f.rule == "orphan")
        .map(|f| f.id.as_str())
        .collect();
    assert_eq!(
        orphans,
        vec![
            "orphan:src/lib.rs:api_gamma",
            "orphan:src/lib.rs:helper_alpha"
        ],
        "{:?}",
        cs.findings
    );
    let alpha = cs
        .findings
        .iter()
        .find(|f| f.id == "orphan:src/lib.rs:helper_alpha")
        .unwrap();
    assert_eq!(alpha.location, "src/lib.rs:L1-1");
    assert!(
        alpha.evidence[0].contains("src/lib.rs:5 `run` (modified)"),
        "{:?}",
        alpha.evidence
    );
    assert!(!alpha.message.contains("public"), "{}", alpha.message);
    let gamma = cs
        .findings
        .iter()
        .find(|f| f.id == "orphan:src/lib.rs:api_gamma")
        .unwrap();
    assert!(gamma.message.contains("public"), "{}", gamma.message);
}

/// `review_changes` reports resolved / new / unchanged findings against the previous
/// review of the same base; a dismissed finding is not counted as resolved.
#[test]
fn review_changes_reports_what_changed_since_the_previous_review() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn old_api() -> i32 { 1 }\npub fn other_api() -> i32 { 2 }\n\
         pub fn caller() -> i32 {\n    old_api()\n}\npub fn second() -> i32 {\n    other_api()\n}\n",
    )
    .unwrap();
    git_in(root, &["init", "-q", "-b", "main"]);
    git_in(root, &["add", "."]);
    git_in(root, &["commit", "-q", "-m", "base"]);
    git_in(root, &["checkout", "-q", "-b", "work"]);
    let both_removed = "pub fn caller() -> i32 {\n    old_api()\n}\n\
                        pub fn second() -> i32 {\n    other_api()\n}\n";
    std::fs::write(root.join("src/lib.rs"), both_removed).unwrap();
    let engine = TrackerEngine::new(root);
    let a = "broken-ref:src/lib.rs:old_api";
    let b = "broken-ref:src/lib.rs:other_api";

    let out = engine.review_changes("main", 4_000).unwrap().text;
    assert!(out.contains("none recorded against `main`"), "{out}");
    let out = engine.review_changes("main", 4_000).unwrap().text;
    assert!(out.contains("0 resolved, 0 new, 2 unchanged"), "{out}");

    // Fix one caller: its finding is resolved, with what it was
    std::fs::write(
        root.join("src/lib.rs"),
        both_removed.replace("    old_api()\n", "    3\n"),
    )
    .unwrap();
    let out = engine.review_changes("main", 4_000).unwrap().text;
    assert!(
        out.contains("1 resolved, 1 new, 1 unchanged")
            && out.contains("- new `untested:src/lib.rs:caller`"),
        "{out}"
    );
    assert!(out.contains(&format!("- resolved `{a}`: was ")), "{out}");

    // Dismissing the other one is not "resolved"
    engine
        .remember_kind(b, "generated", "dismissed", "checked: generated")
        .unwrap();
    let out = engine.review_changes("main", 4_000).unwrap().text;
    assert!(out.contains("0 resolved, 0 new, 1 unchanged"), "{out}");

    // Reintroducing the first broken call makes it new again; the file changed, so the
    // dismissal of the other one no longer holds and it is raised again too
    std::fs::write(root.join("src/lib.rs"), both_removed).unwrap();
    let out = engine.review_changes("main", 4_000).unwrap().text;
    assert!(out.contains("1 resolved, 2 new, 0 unchanged"), "{out}");
    assert!(out.contains(&format!("- new `{a}`")), "{out}");
}

/// `orphan` stays quiet when a function may still be reached in ways the graph does not
/// see: named in a non-indexed file (YAML), registered by an attribute, or a trait method
/// now dispatched through an operator.
#[test]
fn orphan_ignores_functions_reached_outside_the_call_graph() {
    use trackertop::review::review;
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let base = "pub struct P(pub i32);\n\
                impl std::ops::Add for P {\n    type Output = P;\n    fn add(self, o: P) -> P {\n        P(self.0 + o.0)\n    }\n}\n\
                fn job_handler_x() -> i32 { 1 }\n\
                #[my_framework::route]\nfn route_handler_y() -> i32 { 2 }\n\
                #[my_framework::command(\n    rename_all = \"x\",\n)]\nfn multi_attr_w() -> i32 { 3 }\n\
                pub fn run() -> i32 {\n    let p = P(1).add(P(2));\n    p.0 + job_handler_x() + route_handler_y() + multi_attr_w()\n}\n";
    std::fs::write(root.join("src/lib.rs"), base).unwrap();
    std::fs::write(root.join("jobs.yml"), "jobs:\n  - job_handler_x\n").unwrap();
    git_in(root, &["init", "-q", "-b", "main"]);
    git_in(root, &["add", "."]);
    git_in(root, &["commit", "-q", "-m", "base"]);
    git_in(root, &["checkout", "-q", "-b", "work"]);
    let head = base.replace(
        "    let p = P(1).add(P(2));\n    p.0 + job_handler_x() + route_handler_y() + multi_attr_w()\n",
        "    let p = P(1) + P(2);\n    p.0\n",
    );
    std::fs::write(root.join("src/lib.rs"), head).unwrap();

    let cs = review(root, "main").unwrap();
    let orphans: Vec<&str> = cs
        .findings
        .iter()
        .filter(|f| f.rule == "orphan")
        .map(|f| f.id.as_str())
        .collect();
    assert!(orphans.is_empty(), "{orphans:?}");
}
