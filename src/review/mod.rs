//! `trackertop review`: a local, graph-grounded review of the working tree against a base.
//!
//! Step 1 (this module): find exactly which symbols a change adds, removes or modifies.
//! The head side is the current index of the working tree; the base side is the project
//! as of the merge base, checked out and indexed separately (see `snapshot`). Comparing
//! both graphs, instead of guessing from diff text, keeps renames, deletions and moved
//! files correct, and gives later rules (broken references, new cycles, untested code)
//! both graphs to work with.

pub mod snapshot;

use crate::changes::{FileDiff, FileStatus, diff_since, git, touched};
use crate::db::Database;
use crate::graph::CodeGraph;
use crate::indexer::index_repository;
use crate::model::{Symbol, SymbolKind};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Added,
    Removed,
    Modified,
}

impl Change {
    pub fn as_str(self) -> &'static str {
        match self {
            Change::Added => "added",
            Change::Removed => "removed",
            Change::Modified => "modified",
        }
    }
}

/// A symbol span on one side of the change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Span {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangedSymbol {
    pub qualified_name: String,
    pub kind: String,
    pub change: Change,
    /// Where it is now (absent for removed symbols).
    pub head: Option<Span>,
    /// Where it was at the base (absent for added symbols).
    pub base: Option<Span>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileChange {
    pub path: String,
    pub old_path: Option<String>,
    /// added, deleted, modified, renamed or untracked
    pub status: String,
    pub added_lines: usize,
    pub removed_lines: usize,
    /// No extractor for this file type on either side: its symbols are unknown.
    pub not_indexed: bool,
    pub binary: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangeSet {
    pub base_ref: String,
    pub base_commit: String,
    pub files: Vec<FileChange>,
    pub symbols: Vec<ChangedSymbol>,
}

impl ChangeSet {
    pub fn count(&self, change: Change) -> usize {
        self.symbols.iter().filter(|s| s.change == change).count()
    }
}

/// Symbols of `path` in `cg`, without the file-level pseudo symbol.
fn file_symbols<'a>(cg: &'a CodeGraph, path: &str) -> Vec<&'a Symbol> {
    cg.symbols_by_file
        .get(path)
        .map(|ids| {
            ids.iter()
                .filter_map(|id| cg.symbols.get(id))
                .filter(|s| s.kind != SymbolKind::File)
                .collect()
        })
        .unwrap_or_default()
}

fn span(s: &Symbol) -> Span {
    Span {
        path: s.file_path.clone(),
        start_line: s.start_line,
        end_line: s.end_line,
    }
}

/// Symbols keyed by (qualified name, kind); several symbols can share a key (two tests with
/// the same name, overloads), so each key keeps all of them in line order.
type Keyed<'a> = BTreeMap<(String, String), Vec<&'a Symbol>>;

fn keyed<'a>(syms: &[&'a Symbol]) -> Keyed<'a> {
    let mut m: Keyed = BTreeMap::new();
    for s in syms {
        m.entry((s.qualified_name.clone(), s.kind.as_str().to_string()))
            .or_default()
            .push(s);
    }
    for v in m.values_mut() {
        v.sort_by_key(|s| s.start_line);
    }
    m
}

/// Compares one file's symbols at the base and head. `head_hit` / `base_hit` are the
/// symbols whose span a diff hunk touches on that side.
fn compare_file(
    head_syms: &[&Symbol],
    base_syms: &[&Symbol],
    head_hit: &BTreeSet<String>,
    base_hit: &BTreeSet<String>,
    out: &mut Vec<ChangedSymbol>,
) {
    let head = keyed(head_syms);
    let base = keyed(base_syms);
    let keys: BTreeSet<&(String, String)> = head.keys().chain(base.keys()).collect();
    for key in keys {
        let h = head.get(key).map(Vec::as_slice).unwrap_or_default();
        let b = base.get(key).map(Vec::as_slice).unwrap_or_default();
        // Pair occurrences in line order; the surplus on either side is added or removed
        for i in 0..h.len().max(b.len()) {
            let (hs, bs) = (h.get(i).copied(), b.get(i).copied());
            let change = match (hs, bs) {
                (Some(_), None) => Change::Added,
                (None, Some(_)) => Change::Removed,
                (Some(hs), Some(bs)) => {
                    if head_hit.contains(&hs.id) || base_hit.contains(&bs.id) {
                        Change::Modified
                    } else {
                        continue;
                    }
                }
                (None, None) => continue,
            };
            out.push(ChangedSymbol {
                qualified_name: key.0.clone(),
                kind: key.1.clone(),
                change,
                head: hs.map(span),
                base: bs.map(span),
            });
        }
    }
}

/// Maps the diff between `base` and the working tree onto symbols of both graphs.
pub fn collect_changes(
    diffs: &[FileDiff],
    untracked: &[String],
    head: &CodeGraph,
    base: &CodeGraph,
    base_ref: &str,
    base_commit: &str,
) -> ChangeSet {
    let mut files = Vec::new();
    let mut symbols = Vec::new();

    for d in diffs {
        let base_path = d.old_path.as_deref().unwrap_or(&d.path);
        let head_syms = if d.status == FileStatus::Deleted {
            Vec::new()
        } else {
            file_symbols(head, &d.path)
        };
        let base_syms = if d.status == FileStatus::Added {
            Vec::new()
        } else {
            file_symbols(base, base_path)
        };
        let not_indexed =
            !d.binary && !head.files.contains_key(&d.path) && !base.files.contains_key(base_path);
        let mut head_hit = BTreeSet::new();
        let mut base_hit = BTreeSet::new();
        for h in &d.hunks {
            if h.new_count > 0 {
                let (a, b) = h.new_range();
                head_hit.extend(touched(&head_syms, a, b).into_iter().map(|s| s.id.clone()));
            }
            if let Some((a, b)) = h.old_range() {
                base_hit.extend(touched(&base_syms, a, b).into_iter().map(|s| s.id.clone()));
            }
        }
        compare_file(&head_syms, &base_syms, &head_hit, &base_hit, &mut symbols);
        files.push(FileChange {
            path: d.path.clone(),
            old_path: d.old_path.clone(),
            status: match d.status {
                FileStatus::Added => "added",
                FileStatus::Deleted => "deleted",
                FileStatus::Modified => "modified",
                FileStatus::Renamed => "renamed",
            }
            .to_string(),
            added_lines: d.added_lines(),
            removed_lines: d.removed_lines(),
            not_indexed,
            binary: d.binary,
        });
    }

    for path in untracked {
        let head_syms = file_symbols(head, path);
        compare_file(
            &head_syms,
            &[],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut symbols,
        );
        files.push(FileChange {
            path: path.clone(),
            old_path: None,
            status: "untracked".to_string(),
            added_lines: 0,
            removed_lines: 0,
            not_indexed: !head.files.contains_key(path),
            binary: false,
        });
    }

    symbols.sort_by(|a, b| {
        let p = |s: &ChangedSymbol| {
            let sp = s.head.as_ref().or(s.base.as_ref());
            sp.map(|x| (x.path.clone(), x.start_line))
                .unwrap_or_default()
        };
        p(a).cmp(&p(b)).then(a.change.cmp(&b.change))
    });
    ChangeSet {
        base_ref: base_ref.to_string(),
        base_commit: base_commit.to_string(),
        files,
        symbols,
    }
}

/// The branch a review compares against when none is given: `origin/HEAD`'s target, else
/// `main`, else `master`.
pub fn default_base(repo_root: &Path) -> Result<String> {
    if let Ok(s) = git(
        repo_root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) && !s.trim().is_empty()
    {
        return Ok(s.trim().to_string());
    }
    for cand in ["main", "master"] {
        if snapshot::resolve_commit(repo_root, cand).is_ok() {
            return Ok(cand.to_string());
        }
    }
    anyhow::bail!("no base branch found (tried origin/HEAD, main, master): pass --base")
}

/// Reviews the working tree (committed, staged, unstaged and untracked work) against the
/// merge base of `base_ref` and `HEAD`.
pub fn review(repo_root: &Path, base_ref: &str) -> Result<ChangeSet> {
    let base_tip = snapshot::resolve_commit(repo_root, base_ref)?;
    let base_commit = git(repo_root, &["merge-base", &base_tip, "HEAD"])
        .map(|s| s.trim().to_string())
        .with_context(|| format!("`{base_ref}` and HEAD have no common history"))?;

    index_repository(repo_root, false)?;
    let head = CodeGraph::load(&Database::open_for_repo(repo_root)?)?;
    let (_, base) = snapshot::base_graph(repo_root, &base_commit)?;
    let (diffs, untracked) = diff_since(repo_root, &base_commit, &[])?;
    Ok(collect_changes(
        &diffs,
        &untracked,
        &head,
        &base,
        base_ref,
        &base_commit,
    ))
}

fn fmt_span(s: &Span) -> String {
    format!("{}:L{}-{}", s.path, s.start_line, s.end_line)
}

/// Markdown report: scope, then every changed symbol with `file:line` on both sides.
pub fn render_markdown(cs: &ChangeSet) -> String {
    let short: String = cs.base_commit.chars().take(10).collect();
    let mut out = format!(
        "# REVIEW: working tree vs `{}` (merge base {short})\n",
        cs.base_ref
    );
    let mut by_status: BTreeMap<&str, usize> = BTreeMap::new();
    for f in &cs.files {
        *by_status.entry(f.status.as_str()).or_default() += 1;
    }
    out.push_str(&format!(
        "Files: {} changed ({})\n",
        cs.files.len(),
        by_status
            .iter()
            .map(|(k, v)| format!("{v} {k}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.push_str(&format!(
        "Symbols: {} modified, {} added, {} removed\n",
        cs.count(Change::Modified),
        cs.count(Change::Added),
        cs.count(Change::Removed)
    ));

    if !cs.symbols.is_empty() {
        out.push_str("\n## Changed symbols\n");
        for s in &cs.symbols {
            let loc = match (s.change, &s.head, &s.base) {
                (Change::Removed, _, Some(b)) => format!("was {}", fmt_span(b)),
                (_, Some(h), Some(b)) if h.path != b.path || h.start_line != b.start_line => {
                    format!("{} (was {})", fmt_span(h), fmt_span(b))
                }
                (_, Some(h), _) => fmt_span(h),
                _ => String::new(),
            };
            out.push_str(&format!(
                "- [{}] {} [{}] {loc}\n",
                s.change.as_str(),
                s.qualified_name,
                s.kind
            ));
        }
    }

    let renamed: Vec<&FileChange> = cs.files.iter().filter(|f| f.status == "renamed").collect();
    if !renamed.is_empty() {
        out.push_str("\n## Renamed files\n");
        for f in renamed {
            out.push_str(&format!(
                "- {} -> {}\n",
                f.old_path.as_deref().unwrap_or("?"),
                f.path
            ));
        }
    }
    let blind: Vec<&FileChange> = cs
        .files
        .iter()
        .filter(|f| f.not_indexed || f.binary)
        .collect();
    out.push_str(&format!(
        "\nCOVERAGE: {} changed files mapped onto symbols; {} not mapped{}\n",
        cs.files.len() - blind.len(),
        blind.len(),
        if blind.is_empty() {
            String::new()
        } else {
            format!(
                " (no extractor or binary: {})",
                blind
                    .iter()
                    .map(|f| f.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    ));
    out
}
