//! Base-side snapshots for `trackertop review`: the project as it was at a commit, checked
//! out into `.trackertop/review-base/<commit>/` and indexed there.
//!
//! The checkout uses a private git index file and `checkout-index`, so it registers no
//! worktree, runs no hooks and never touches the user's branch, index or working tree.
//! A snapshot is published by renaming a fully written temp directory, so an interrupted
//! run never leaves a half-checked-out base that a later run would trust.

use crate::changes::git;
use crate::db::Database;
use crate::graph::CodeGraph;
use crate::indexer::index_snapshot;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// How many base snapshots are kept per project (oldest are removed first).
const KEEP_SNAPSHOTS: usize = 3;

/// Full commit id that `rev` names, or an error if it names no commit.
pub fn resolve_commit(repo_root: &Path, rev: &str) -> Result<String> {
    let r = rev.trim();
    if r.is_empty() || r.starts_with('-') {
        bail!("`{rev}` is not a valid git ref");
    }
    git(repo_root, &["rev-parse", "--is-inside-work-tree"])
        .context("not a git repository: review needs git history")?;
    git(
        repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{r}^{{commit}}"),
        ],
    )
    .map(|s| s.trim().to_string())
    .with_context(|| format!("`{r}` does not name a commit in this repository"))
}

/// Where the project root sits inside the repository (`""` at the top level,
/// `packages/app/` in a monorepo package).
fn project_prefix(repo_root: &Path) -> Result<String> {
    Ok(git(repo_root, &["rev-parse", "--show-prefix"])?
        .trim()
        .to_string())
}

/// The project as of `commit`, indexed. Returns the snapshot's project root and its graph.
pub fn base_graph(repo_root: &Path, commit: &str) -> Result<(PathBuf, CodeGraph)> {
    let cache = repo_root.join(".trackertop").join("review-base");
    std::fs::create_dir_all(&cache)?;
    let short: String = commit.chars().take(12).collect();
    let dir = cache.join(&short);
    if !dir.is_dir() {
        checkout_snapshot(repo_root, commit, &cache, &dir)?;
    }
    prune_old_snapshots(&cache, &dir);
    index_snapshot(&dir).with_context(|| format!("indexing the base snapshot {short}"))?;
    let db = Database::open_for_repo(&dir)?;
    let graph = CodeGraph::load(&db)?;
    Ok((dir, graph))
}

fn checkout_snapshot(repo_root: &Path, commit: &str, cache: &Path, dir: &Path) -> Result<()> {
    let prefix = project_prefix(repo_root)?;
    let tree = if prefix.is_empty() {
        format!("{commit}^{{tree}}")
    } else {
        format!("{commit}:{}", prefix.trim_end_matches('/'))
    };
    let tmp = cache.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        commit.chars().take(12).collect::<String>()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let index_file = tmp.join(".git-index");
    // Run from the repository top level: in a subdirectory, `checkout-index --all` only
    // writes entries under that subdirectory's prefix, and the private index holds the
    // project's subtree at its root.
    let toplevel = PathBuf::from(git(repo_root, &["rev-parse", "--show-toplevel"])?.trim());
    let result = (|| -> Result<()> {
        let mut prefix_arg = tmp.to_string_lossy().replace('\\', "/");
        if !prefix_arg.ends_with('/') {
            prefix_arg.push('/');
        }
        run_git_with_index(&toplevel, &index_file, &["read-tree", &tree])?;
        run_git_with_index(
            &toplevel,
            &index_file,
            &[
                "checkout-index",
                "--all",
                "--force",
                &format!("--prefix={prefix_arg}"),
            ],
        )?;
        std::fs::remove_file(&index_file)?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e.context(format!("checking out the base commit {commit}")));
    }
    if let Err(e) = std::fs::rename(&tmp, dir) {
        let _ = std::fs::remove_dir_all(&tmp);
        // Another review published the same snapshot first: use it
        if dir.is_dir() {
            return Ok(());
        }
        return Err(e).context("publishing the base snapshot");
    }
    Ok(())
}

fn run_git_with_index(repo_root: &Path, index_file: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(repo_root)
        .env("GIT_INDEX_FILE", index_file)
        .args(["-c", "core.quotepath=false"])
        .args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd
        .output()
        .context("could not run `git` (is it installed and on PATH?)")?;
    if !out.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Keeps the newest snapshots (and always `current`); removes the rest and stale temp dirs.
fn prune_old_snapshots(cache: &Path, current: &Path) {
    let Ok(entries) = std::fs::read_dir(cache) else {
        return;
    };
    let mut snaps: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let modified = e
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if name.starts_with(".tmp-") {
            // Left by an interrupted run more than an hour ago
            if modified.elapsed().is_ok_and(|age| age.as_secs() > 3600) {
                let _ = std::fs::remove_dir_all(&path);
            }
            continue;
        }
        if path.is_dir() && path != current {
            snaps.push((modified, path));
        }
    }
    snaps.sort_by_key(|s| std::cmp::Reverse(s.0));
    for (_, path) in snaps.into_iter().skip(KEEP_SNAPSHOTS.saturating_sub(1)) {
        let _ = std::fs::remove_dir_all(path);
    }
}
