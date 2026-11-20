use crate::model::{Edge, EdgeKind};
use crate::parser::file_symbol_id;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

fn run_git(repo_root: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_root).args(args);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Extracts weighted `CO_CHANGES` edges between files that frequently change together
/// in recent git history (last 80 commits). 100% offline; silently returns empty if not a git repo.
pub fn extract_cochange_edges(repo_root: &Path, indexed_files: &HashSet<String>) -> Vec<Edge> {
    if !repo_root.join(".git").exists() {
        return Vec::new();
    }
    let Some(log) = run_git(
        repo_root,
        &[
            "log",
            "-n",
            "80",
            "--name-only",
            "--pretty=format:---COMMIT---",
        ],
    ) else {
        return Vec::new();
    };

    let mut pair_counts: HashMap<(String, String), usize> = HashMap::new();
    let mut current_files: Vec<String> = Vec::new();

    let flush_commit = |files: &mut Vec<String>, counts: &mut HashMap<(String, String), usize>| {
        // Ignore massive refactor/format commits (> 20 files) to avoid false co-change noise
        if files.len() >= 2 && files.len() <= 20 {
            files.sort();
            files.dedup();
            for i in 0..files.len() {
                for j in (i + 1)..files.len() {
                    *counts
                        .entry((files[i].clone(), files[j].clone()))
                        .or_insert(0) += 1;
                }
            }
        }
        files.clear();
    };

    for line in log.lines() {
        let trimmed = line.trim();
        if trimmed == "---COMMIT---" {
            flush_commit(&mut current_files, &mut pair_counts);
        } else if !trimmed.is_empty() {
            let norm = trimmed.replace('\\', "/");
            if indexed_files.contains(&norm) {
                current_files.push(norm);
            }
        }
    }
    flush_commit(&mut current_files, &mut pair_counts);

    let mut edges = Vec::new();
    for ((a, b), count) in pair_counts {
        if count >= 2 {
            let weight = (0.5 + (count as f32 * 0.25)).min(2.5);
            let fa = file_symbol_id(&a);
            let fb = file_symbol_id(&b);
            edges.push(Edge {
                source_id: fa.clone(),
                target_id: fb.clone(),
                kind: EdgeKind::CoChanges,
                weight,
            });
            edges.push(Edge {
                source_id: fb,
                target_id: fa,
                kind: EdgeKind::CoChanges,
                weight,
            });
        }
    }
    edges
}

/// Returns files currently modified in working tree / staged area, or files from the latest commit if clean.
pub fn changed_files_for_diff(repo_root: &Path) -> (String, Vec<String>) {
    if !repo_root.join(".git").exists() {
        return ("not a git repository".to_string(), Vec::new());
    }

    let mut changed = HashSet::new();
    if let Some(status) = run_git(repo_root, &["status", "--porcelain"]) {
        for line in status.lines() {
            if line.len() >= 4 {
                let raw = line[3..].trim();
                let path = raw.rsplit(" -> ").next().unwrap_or(raw).trim_matches('"');
                if !path.is_empty() {
                    changed.insert(path.replace('\\', "/"));
                }
            }
        }
    }

    if !changed.is_empty() {
        let mut v: Vec<String> = changed.into_iter().collect();
        v.sort();
        return ("working tree (uncommitted)".to_string(), v);
    }

    // Fallback to latest commit so `diff_impact` is still informative on a clean repo
    if let Some(last) = run_git(
        repo_root,
        &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
    ) {
        for line in last.lines() {
            let norm = line.trim().replace('\\', "/");
            if !norm.is_empty() {
                changed.insert(norm);
            }
        }
    }
    let mut v: Vec<String> = changed.into_iter().collect();
    v.sort();
    ("HEAD (latest commit)".to_string(), v)
}
