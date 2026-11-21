//! `changes_since`: what changed in the working tree (staged + unstaged + untracked) since a
//! git ref, mapped onto symbols, so an agent re-reads only the diffs instead of whole files.
//!
//! For every changed file the report lists the touched symbols (current spans), symbols
//! added or removed since the ref (the old file is parsed with the same extractor), and the
//! changed lines themselves. Lists that are cut say so.

use crate::graph::CodeGraph;
use crate::model::{Symbol, SymbolKind};
use crate::parser::default_registry;
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// Max diff lines printed per file before the rest is summarized.
const LINES_PER_FILE: usize = 40;

pub(crate) fn git(repo_root: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(crate::fsutil::process_path(repo_root))
        .args(["-c", "core.quotepath=false"])
        .args(args);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd
        .output()
        .context("could not run `git` (is it installed and on PATH?)")?;
    if !output.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    /// `+line` / `-line` (context lines are not requested).
    pub lines: Vec<String>,
}

impl Hunk {
    /// Inclusive new-side line range this hunk touches. A pure deletion touches the line
    /// it was removed before (or after, at the end of the file).
    pub fn new_range(&self) -> (usize, usize) {
        if self.new_count == 0 {
            let at = self.new_start.max(1);
            (at, at + 1)
        } else {
            (self.new_start, self.new_start + self.new_count - 1)
        }
    }

    /// Inclusive old-side line range, or `None` for a pure insertion (nothing removed).
    pub fn old_range(&self) -> Option<(usize, usize)> {
        (self.old_count > 0).then(|| (self.old_start, self.old_start + self.old_count - 1))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    pub fn added_lines(&self) -> usize {
        self.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.starts_with('+'))
            .count()
    }

    pub fn removed_lines(&self) -> usize {
        self.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.starts_with('-'))
            .count()
    }
}

/// `12` or `12,3` → (start, count); a missing count means 1.
fn parse_range(s: &str) -> Option<(usize, usize)> {
    let (a, b) = match s.split_once(',') {
        Some((a, b)) => (a, b.parse().ok()?),
        None => (s, 1),
    };
    Some((a.parse().ok()?, b))
}

/// Parses `git diff --unified=0` output.
pub fn parse_unified_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    // Lines still expected in the current hunk (old side, new side). While they are
    // positive every `-`/`+` line is content, even one that looks like a `---` header.
    let mut remaining = (0usize, 0usize);
    for line in text.lines() {
        if remaining != (0, 0) {
            if let Some(h) = files.last_mut().and_then(|f| f.hunks.last_mut()) {
                if line.starts_with('-') && remaining.0 > 0 {
                    remaining.0 -= 1;
                    h.lines.push(line.to_string());
                    continue;
                }
                if line.starts_with('+') && remaining.1 > 0 {
                    remaining.1 -= 1;
                    h.lines.push(line.to_string());
                    continue;
                }
                if line.starts_with('\\') {
                    continue; // "\ No newline at end of file"
                }
            }
            remaining = (0, 0); // malformed or truncated hunk: resync on headers
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // Paths are re-read from ---/+++ and rename headers; this one is a fallback
            let path = rest
                .rsplit_once(" b/")
                .map(|(_, b)| b.to_string())
                .unwrap_or_default();
            files.push(FileDiff {
                path,
                old_path: None,
                status: FileStatus::Modified,
                binary: false,
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if line.starts_with("new file mode") {
            file.status = FileStatus::Added;
        } else if line.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
        } else if let Some(from) = line.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = Some(from.to_string());
        } else if let Some(to) = line.strip_prefix("rename to ") {
            file.path = to.to_string();
        } else if line.starts_with("Binary files ") {
            file.binary = true;
        } else if let Some(p) = line.strip_prefix("--- a/") {
            if file.status == FileStatus::Deleted {
                file.path = p.to_string();
            }
        } else if let Some(p) = line.strip_prefix("+++ b/") {
            file.path = p.to_string();
        } else if let Some(rest) = line.strip_prefix("@@ -") {
            // @@ -old[,n] +new[,n] @@ optional section
            let mut parts = rest.split_whitespace();
            let old = parts.next().and_then(parse_range);
            let new = parts
                .next()
                .and_then(|p| p.strip_prefix('+'))
                .and_then(parse_range);
            if let (Some((os, oc)), Some((ns, nc))) = (old, new) {
                file.hunks.push(Hunk {
                    old_start: os,
                    old_count: oc,
                    new_start: ns,
                    new_count: nc,
                    lines: Vec::new(),
                });
                remaining = (oc, nc);
            }
        }
    }
    files
}

/// Resolves `git_ref` to a short commit id, failing loudly for anything that is not one.
pub fn resolve_ref(repo_root: &Path, git_ref: &str) -> Result<String> {
    let r = git_ref.trim();
    if r.is_empty() || r.starts_with('-') {
        bail!("`{git_ref}` is not a valid git ref");
    }
    git(repo_root, &["rev-parse", "--is-inside-work-tree"])
        .context("not a git repository: changes_since needs git history")?;
    let spec = format!("{r}^{{commit}}");
    git(
        repo_root,
        &["rev-parse", "--verify", "--quiet", "--short", &spec],
    )
    .map(|s| s.trim().to_string())
    .with_context(|| format!("`{r}` does not name a commit in this repository"))
}

/// Working tree (staged + unstaged) vs `commit`, plus untracked files, limited to `paths`.
pub fn diff_since(
    repo_root: &Path,
    commit: &str,
    paths: &[String],
) -> Result<(Vec<FileDiff>, Vec<String>)> {
    let mut args = vec![
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--unified=0",
        "--find-renames",
        // Paths relative to the project root, and nothing outside it, even when the project
        // is a subdirectory of the git repository (a monorepo package)
        "--relative",
        commit,
        "--",
    ];
    let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    args.extend(&path_refs);
    let mut untracked_args = vec!["ls-files", "--others", "--exclude-standard", "--"];
    untracked_args.extend(&path_refs);
    // The two git processes are independent: run them side by side
    let (diff, untracked) = std::thread::scope(|s| {
        let untracked = s.spawn(|| git(repo_root, &untracked_args));
        let diff = git(repo_root, &args);
        (diff, untracked.join())
    });
    let diff = parse_unified_diff(&diff?);
    let untracked = untracked
        .map_err(|_| anyhow::anyhow!("git thread panicked"))??
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    Ok((diff, untracked))
}

fn old_symbol_names(repo_root: &Path, commit: &str, path: &str) -> Option<BTreeSet<String>> {
    let ext = default_registry().extractor_for_path(Path::new(path))?;
    // `./` makes the path relative to the project root, not the repository top level
    let old = git(repo_root, &["show", &format!("{commit}:./{path}")]).ok()?;
    let parsed = ext.parse(path, &old).ok()?;
    Some(
        parsed
            .symbols
            .into_iter()
            .filter(|s| s.kind != SymbolKind::File)
            .map(|s| s.qualified_name)
            .collect(),
    )
}

/// Innermost symbols of `syms` that intersect `[a, b]`.
pub(crate) fn touched<'a>(syms: &[&'a Symbol], a: usize, b: usize) -> Vec<&'a Symbol> {
    let hit: Vec<&Symbol> = syms
        .iter()
        .copied()
        .filter(|s| s.start_line <= b && a <= s.end_line)
        .collect();
    hit.iter()
        .copied()
        .filter(|s| {
            !hit.iter().any(|o| {
                o.id != s.id
                    && s.start_line <= o.start_line
                    && o.end_line <= s.end_line
                    && (o.start_line, o.end_line) != (s.start_line, s.end_line)
            })
        })
        .collect()
}

/// Rendered report and the token baseline (current size of the changed files).
pub struct ChangesReport {
    pub text: String,
    pub baseline_tokens: usize,
    pub files: usize,
}

/// Builds the report. `symbols` restricts it to hunks that touch those symbols.
pub fn changes_since(
    repo_root: &Path,
    cg: &CodeGraph,
    git_ref: &str,
    paths: &[String],
    symbols: &[String],
) -> Result<ChangesReport> {
    let commit = resolve_ref(repo_root, git_ref)?;

    // Symbol filter: resolve to (file, span) now; unresolved names are reported, not dropped.
    let mut focus: Vec<&Symbol> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();
    for q in symbols {
        match cg.resolve_symbol(q) {
            Some(s) => focus.push(s),
            None => unresolved.push(q.clone()),
        }
    }
    let mut scope: Vec<String> = paths.to_vec();
    scope.extend(focus.iter().map(|s| s.file_path.clone()));
    if !symbols.is_empty() && focus.is_empty() {
        scope.clear();
    }
    let (diffs, untracked) = if !symbols.is_empty() && focus.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        diff_since(repo_root, &commit, &scope)?
    };

    let mut out = format!(
        "# CHANGES SINCE `{git_ref}` ({commit}): working tree incl. staged/unstaged edits vs that commit\n"
    );
    for q in &unresolved {
        out.push_str(&format!(
            "SYMBOL `{q}` NOT FOUND IN INDEX (it may have been deleted or renamed; check \
             `Removed symbols` below or call without `symbols`).\n"
        ));
    }
    let mut baseline = 0usize;
    let mut shown_files = 0usize;
    for d in &diffs {
        let file_syms: Vec<&Symbol> = cg
            .symbols_by_file
            .get(&d.path)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| cg.symbols.get(id))
                    .filter(|s| s.kind != SymbolKind::File)
                    .collect()
            })
            .unwrap_or_default();
        let focus_here: Vec<&Symbol> = focus
            .iter()
            .copied()
            .filter(|s| s.file_path == d.path)
            .collect();
        let hunks: Vec<&Hunk> = d
            .hunks
            .iter()
            .filter(|h| {
                focus.is_empty() || {
                    let (a, b) = h.new_range();
                    focus_here
                        .iter()
                        .any(|s| s.start_line <= b && a <= s.end_line)
                }
            })
            .collect();
        if !focus.is_empty() && hunks.is_empty() {
            continue;
        }
        shown_files += 1;
        if let Some(f) = cg.files.get(&d.path) {
            baseline += f.token_count;
        }
        let status = match d.status {
            FileStatus::Added => "added".to_string(),
            FileStatus::Deleted => "deleted".to_string(),
            FileStatus::Modified => "modified".to_string(),
            FileStatus::Renamed => format!("renamed from {}", d.old_path.as_deref().unwrap_or("?")),
        };
        out.push_str(&format!(
            "\n## {} ({status}, +{} -{})\n",
            d.path,
            d.added_lines(),
            d.removed_lines()
        ));
        if d.binary {
            out.push_str("Binary file: no line diff.\n");
            continue;
        }
        if !cg.files.contains_key(&d.path) && d.status != FileStatus::Deleted {
            out.push_str("(file not indexed: no symbol mapping)\n");
        }

        let mut touched_syms: Vec<&Symbol> = Vec::new();
        for h in &hunks {
            let (a, b) = h.new_range();
            for s in touched(&file_syms, a, b) {
                if !touched_syms.iter().any(|t| t.id == s.id) {
                    touched_syms.push(s);
                }
            }
        }
        touched_syms.sort_by_key(|s| s.start_line);
        if !touched_syms.is_empty() {
            out.push_str("Touched: ");
            out.push_str(
                &touched_syms
                    .iter()
                    .map(|s| {
                        format!(
                            "{} [{}] L{}-{}",
                            s.qualified_name,
                            s.kind.as_str(),
                            s.start_line,
                            s.end_line
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            out.push('\n');
        }
        if focus.is_empty() && d.status != FileStatus::Added {
            let old_path = d.old_path.as_deref().unwrap_or(&d.path);
            if let Some(old) = old_symbol_names(repo_root, &commit, old_path) {
                let new: BTreeSet<String> =
                    file_syms.iter().map(|s| s.qualified_name.clone()).collect();
                let added: Vec<&String> = new.difference(&old).collect();
                let removed: Vec<&String> = old.difference(&new).collect();
                if !added.is_empty() {
                    out.push_str(&format!(
                        "Added symbols: {}\n",
                        added
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if !removed.is_empty() {
                    out.push_str(&format!(
                        "Removed symbols: {}\n",
                        removed
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        }
        if d.status == FileStatus::Added || d.status == FileStatus::Deleted {
            let total: usize = hunks.iter().map(|h| h.lines.len()).sum();
            out.push_str(&format!(
                "(whole file {}; {total} lines not repeated here, read it if needed)\n",
                if d.status == FileStatus::Added {
                    "is new"
                } else {
                    "was deleted"
                }
            ));
            continue;
        }

        out.push_str("```diff\n");
        let mut printed = 0usize;
        let total: usize = hunks.iter().map(|h| h.lines.len()).sum();
        for h in &hunks {
            if printed >= LINES_PER_FILE {
                break;
            }
            let (a, b) = h.new_range();
            let owner = touched(&file_syms, a, b)
                .first()
                .map(|s| format!(" in {}", s.qualified_name))
                .unwrap_or_default();
            out.push_str(&format!(
                "@@ -{},{} +{},{} @@{owner}\n",
                h.old_start, h.old_count, h.new_start, h.new_count
            ));
            for l in &h.lines {
                if printed >= LINES_PER_FILE {
                    break;
                }
                out.push_str(l);
                out.push('\n');
                printed += 1;
            }
        }
        out.push_str("```\n");
        if total > printed {
            out.push_str(&format!(
                "(showing {printed} of {total} changed lines in this file; read the touched \
                 symbols for the rest)\n"
            ));
        }
    }

    if !untracked.is_empty() {
        out.push_str(&format!("\n## Untracked new files ({})\n", untracked.len()));
        for u in &untracked {
            let syms = cg
                .symbols_by_file
                .get(u)
                .map_or(0, |v| v.len().saturating_sub(1));
            out.push_str(&format!("- {u} ({syms} symbols)\n"));
        }
    }
    if shown_files == 0 && untracked.is_empty() {
        out.push_str(if symbols.is_empty() {
            "\nNo changes since that commit in the requested scope.\n"
        } else {
            "\nNo changes touch the requested symbols since that commit.\n"
        });
    }
    out.push_str(&format!(
        "\nCOVERAGE: {shown_files} changed files{}{}; untracked files included, ignored files \
         excluded. Spans are from the current index.\n",
        if paths.is_empty() {
            String::new()
        } else {
            format!(" under {}", paths.join(", "))
        },
        if focus.is_empty() {
            String::new()
        } else {
            " (only hunks touching the requested symbols)".to_string()
        }
    ));
    Ok(ChangesReport {
        text: out,
        baseline_tokens: baseline,
        files: shown_files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -3 +3 @@ fn one() {
-    1
+    2
@@ -10,2 +9,0 @@
-gone1
-gone2
@@ -20,0 +19,3 @@
+n1
+n2
+n3
diff --git a/old name.rs b/new name.rs
similarity index 90%
rename from old name.rs
rename to new name.rs
--- a/old name.rs
+++ b/new name.rs
@@ -1 +1 @@
-x
+y
diff --git a/del.ts b/del.ts
deleted file mode 100644
index 333..000
--- a/del.ts
+++ /dev/null
@@ -1,2 +0,0 @@
-a
-b
diff --git a/img.png b/img.png
new file mode 100644
index 000..444
Binary files /dev/null and b/img.png differ
";

    #[test]
    fn parses_hunks_renames_deletions_and_binaries() {
        let files = parse_unified_diff(DIFF);
        assert_eq!(files.len(), 4);
        let a = &files[0];
        assert_eq!(a.path, "src/a.rs");
        assert_eq!(a.status, FileStatus::Modified);
        assert_eq!(a.hunks.len(), 3);
        assert_eq!(
            (
                a.hunks[0].old_start,
                a.hunks[0].old_count,
                a.hunks[0].new_start,
                a.hunks[0].new_count
            ),
            (3, 1, 3, 1)
        );
        assert_eq!(a.hunks[1].new_range(), (9, 10), "pure deletion");
        assert_eq!(a.hunks[2].new_range(), (19, 21));
        assert_eq!((a.added_lines(), a.removed_lines()), (4, 3));

        let r = &files[1];
        assert_eq!(r.status, FileStatus::Renamed);
        assert_eq!(r.path, "new name.rs");
        assert_eq!(r.old_path.as_deref(), Some("old name.rs"));

        let d = &files[2];
        assert_eq!(
            (d.status.clone(), d.path.as_str()),
            (FileStatus::Deleted, "del.ts")
        );
        assert_eq!(d.removed_lines(), 2);

        let b = &files[3];
        assert!(b.binary);
        assert_eq!(
            (b.status.clone(), b.path.as_str()),
            (FileStatus::Added, "img.png")
        );
    }

    #[test]
    fn diff_lines_that_look_like_headers_stay_content() {
        // A removed line whose text starts with "-- " must not be read as a header
        let text = "diff --git a/x.sql b/x.sql\n--- a/x.sql\n+++ b/x.sql\n@@ -1 +1 @@\n--- comment\n+++ comment\n";
        let f = &parse_unified_diff(text)[0];
        assert_eq!(f.path, "x.sql");
        assert_eq!(f.hunks[0].lines, vec!["--- comment", "+++ comment"]);
        // Even a content line shaped exactly like a header stays content
        let text = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n--- a/evil\n+++ b/evil\n";
        let f = &parse_unified_diff(text)[0];
        assert_eq!(f.path, "x");
        assert_eq!(f.hunks[0].lines.len(), 2);
    }

    #[test]
    fn refs_starting_with_a_dash_are_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = resolve_ref(tmp.path(), "--output=x").unwrap_err();
        assert!(format!("{err:#}").contains("not a valid git ref"));
    }
}
