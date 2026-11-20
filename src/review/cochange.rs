//! `cochange-miss`: a file that, in the project's own history, almost always changes
//! together with a file this change touches, but was left alone this time.
//!
//! The history is read from git at the merge base, so the branch under review never
//! votes for itself. Only the habit is reported, never a claim that the file is wrong:
//! the finding is low severity and quotes the commits it rests on.

use super::ChangeSet;
use super::rules::{Finding, Severity};
use crate::changes::git;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Non-merge commits read from the base's history.
pub const HISTORY_COMMITS: usize = 300;
/// Commits touching more files than this are bulk edits (formatting, renames, version
/// bumps): they say nothing about which files belong together.
/// Release commits of this repo touch 12–20 files and coupled everything with everything,
/// so the bar is lower than git's usual "bulk" notion.
const MAX_FILES_PER_COMMIT: usize = 12;
/// A changed file needs this much history before its habits are trusted at all...
const MIN_HISTORY: usize = 5;
/// ...a partner must have changed with it in at least this many commits...
const MIN_TOGETHER: usize = 4;
/// ...and in at least this share (percent) of all commits that touched the file.
const MIN_SHARE_PCT: usize = 60;

/// One commit of the base's history: `short-hash subject` and the files it touched,
/// relative to the project root.
pub struct Commit {
    pub label: String,
    pub files: BTreeSet<String>,
}

/// Recent history at `base_commit`, restricted to the project (a monorepo package only
/// sees its own files).
pub fn load_history(root: &Path, base_commit: &str) -> Result<Vec<Commit>> {
    let n = HISTORY_COMMITS.to_string();
    let log = git(
        root,
        &[
            "log",
            "-n",
            &n,
            "--no-merges",
            "--name-only",
            "--relative",
            "--format=%x01%h %s",
            base_commit,
            "--",
            ".",
        ],
    )?;
    let mut out: Vec<Commit> = Vec::new();
    for line in log.lines() {
        if let Some(label) = line.strip_prefix('\u{1}') {
            let label = label.trim();
            let label = match label.char_indices().nth(72) {
                Some((i, _)) => format!("{}…", &label[..i]),
                None => label.to_string(),
            };
            out.push(Commit {
                label,
                files: BTreeSet::new(),
            });
        } else if let Some(c) = out.last_mut() {
            let p = line.trim();
            if !p.is_empty() {
                c.files.insert(p.replace('\\', "/"));
            }
        }
    }
    Ok(out)
}

/// Why a partner was flagged: (changed file, commits together, its commits, sample labels).
type Hit = (String, usize, usize, Vec<String>);

/// `cochange-miss`: for each file the change touches, partners that changed with it in
/// ≥ `MIN_TOGETHER` commits and ≥ `MIN_SHARE_PCT`% of its commits, absent from this change
/// and still present in the working tree. One finding per partner.
pub fn cochange_miss(cs: &ChangeSet, history: &[Commit], head_root: &Path, out: &mut Vec<Finding>) {
    let changed: BTreeSet<&str> = cs
        .files
        .iter()
        .flat_map(|f| std::iter::once(f.path.as_str()).chain(f.old_path.as_deref()))
        .collect();
    let usable: Vec<&Commit> = history
        .iter()
        .filter(|c| c.files.len() >= 2 && c.files.len() <= MAX_FILES_PER_COMMIT)
        .collect();

    // partner -> [(changed file, together, total, commits as evidence)]
    let mut hits: BTreeMap<String, Vec<Hit>> = BTreeMap::new();
    for f in &cs.files {
        // History is keyed by the base name of a renamed file.
        let key = f.old_path.as_deref().unwrap_or(&f.path);
        let mine: Vec<&&Commit> = usable.iter().filter(|c| c.files.contains(key)).collect();
        let total = mine.len();
        if total < MIN_HISTORY {
            continue;
        }
        let mut together: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for c in &mine {
            for p in &c.files {
                if p != key {
                    together
                        .entry(p.as_str())
                        .or_default()
                        .push(c.label.as_str());
                }
            }
        }
        for (partner, commits) in together {
            let k = commits.len();
            if k < MIN_TOGETHER || k * 100 < total * MIN_SHARE_PCT {
                continue;
            }
            if changed.contains(partner) || !head_root.join(partner).is_file() {
                continue;
            }
            hits.entry(partner.to_string()).or_default().push((
                f.path.clone(),
                k,
                total,
                commits.iter().take(3).map(|s| s.to_string()).collect(),
            ));
        }
    }

    for (partner, mut why) in hits {
        why.sort_by(|a, b| {
            (b.1 * 1000 / b.2)
                .cmp(&(a.1 * 1000 / a.2))
                .then(a.0.cmp(&b.0))
        });
        let (top, k, total, _) = &why[0];
        let more = if why.len() > 1 {
            format!(" (and {} other changed file(s))", why.len() - 1)
        } else {
            String::new()
        };
        let mut evidence = Vec::new();
        for (file, k, total, commits) in why.iter().take(3) {
            evidence.push(format!(
                "{file}: changed with {partner} in {k} of its {total} commits, e.g. {}",
                commits.join("; ")
            ));
        }
        out.push(Finding {
            id: format!("cochange-miss:{partner}"),
            rule: "cochange-miss",
            severity: Severity::Low,
            symbol: String::new(),
            location: partner.clone(),
            message: format!(
                "{partner} changed together with {top} in {k} of {top}'s {total} recent \
                 commits{more}, but not in this change: check whether it needs the same \
                 update (history habit, not proof; last {HISTORY_COMMITS} commits at the base)"
            ),
            evidence,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::FileChange;

    fn commit(label: &str, files: &[&str]) -> Commit {
        Commit {
            label: label.into(),
            files: files.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn change(path: &str) -> FileChange {
        FileChange {
            path: path.into(),
            old_path: None,
            status: "modified".into(),
            added_lines: 1,
            removed_lines: 0,
            not_indexed: false,
            binary: false,
        }
    }

    #[test]
    fn flags_habitual_partner_and_ignores_weak_or_bulk_ones() {
        let dir = tempfile::TempDir::new().unwrap();
        for f in ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs"] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        let bulk: Vec<String> = (0..25).map(|i| format!("x{i}.rs")).collect();
        let mut bulk_refs: Vec<&str> = bulk.iter().map(|s| s.as_str()).collect();
        bulk_refs.extend(["a.rs", "c.rs"]);
        let history = vec![
            commit("1 one", &["a.rs", "b.rs"]),
            commit("2 two", &["a.rs", "b.rs", "c.rs"]),
            commit("3 three", &["a.rs", "b.rs", "c.rs"]),
            commit("4 four", &["a.rs", "b.rs", "d.rs"]),
            commit("5 five", &["a.rs", "c.rs"]),
            commit("6 six", &["a.rs", "d.rs"]),
            // e.rs has only 4 commits: always with f.rs, but too little history to trust
            commit("e1", &["e.rs", "f.rs"]),
            commit("e2", &["e.rs", "f.rs"]),
            commit("e3", &["e.rs", "f.rs"]),
            commit("e4", &["e.rs", "f.rs"]),
            // bulk commits never count, even repeated
            commit("6 bulk", &bulk_refs),
            commit("7 bulk", &bulk_refs),
            commit("8 bulk", &bulk_refs),
        ];
        let cs = ChangeSet {
            base_ref: "main".into(),
            base_commit: "0".into(),
            files: vec![change("a.rs"), change("e.rs")],
            symbols: vec![],
            findings: vec![],
        };
        let mut out = Vec::new();
        cochange_miss(&cs, &history, dir.path(), &mut out);
        let ids: Vec<&str> = out.iter().map(|f| f.id.as_str()).collect();
        // b: 4 of 6 -> flagged; c: 3 of 6 -> not; d: 2 of 6 -> not; f: e.rs too young
        assert_eq!(ids, vec!["cochange-miss:b.rs"]);
        assert!(
            out[0].message.contains("4 of a.rs's 6"),
            "{}",
            out[0].message
        );
        assert!(
            out[0].evidence[0].contains("1 one"),
            "{:?}",
            out[0].evidence
        );

        // Changed together this time: no finding
        let mut cs2 = cs.clone();
        cs2.files.push(change("b.rs"));
        let mut out = Vec::new();
        cochange_miss(&cs2, &history, dir.path(), &mut out);
        assert!(out.is_empty());

        // Partner deleted from the working tree: no finding
        std::fs::remove_file(dir.path().join("b.rs")).unwrap();
        let mut out = Vec::new();
        cochange_miss(&cs, &history, dir.path(), &mut out);
        assert!(out.is_empty());
    }
}
