//! Cross-session continuity: resume and the session-start briefing.

use super::*;

impl TrackerEngine {
    /// Everything `resume` reports, read from the index DB, git and the progress file.
    fn resume_data(&self, db: &Database, cg: &CodeGraph) -> Result<crate::journal::ResumeData> {
        use crate::journal::{ChangedFile, NoteView, OpenFinding, ResumeData, WorkTree};
        let (mut journal, mut journal_total) = db.recent_journal(2_000)?;
        // Agent calls from before the journal existed (telemetry has no session or
        // outcome; they are grouped by day)
        if journal.len() < 2_000 {
            let before = journal
                .last()
                .map(|e| e.timestamp.clone())
                .unwrap_or_default();
            let (older, older_total) = db.telemetry_as_journal(&before, 2_000 - journal.len())?;
            journal_total += older_total;
            journal.extend(older);
        }
        let notes = db
            .list_notes(None)?
            .into_iter()
            .map(|n| NoteView {
                outdated: self.note_outdated(cg, &n),
                note: n,
            })
            .collect();
        let findings = db
            .review_findings()?
            .into_iter()
            .map(|(base_ref, id, summary, reviewed_at)| OpenFinding {
                base_ref,
                id,
                summary,
                reviewed_at,
            })
            .collect();
        let worktree = (|| -> Result<WorkTree> {
            // Each git process costs ~0.1 s on Windows: run them side by side so the
            // SessionStart briefing stays fast
            let root = &self.repo_root;
            let (branch, diff) = std::thread::scope(|s| {
                let branch = s.spawn(|| {
                    crate::changes::git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
                        .map(|b| b.trim().to_string())
                });
                let diff = crate::changes::diff_since(root, "HEAD", &[]);
                (branch.join(), diff)
            });
            let branch = branch.map_err(|_| anyhow::anyhow!("git thread panicked"))??;
            let (diffs, untracked) = diff?;
            let mut files: Vec<ChangedFile> = diffs
                .iter()
                .map(|d| {
                    let syms: Vec<&Symbol> = cg
                        .symbols_by_file
                        .get(&d.path)
                        .map(|ids| {
                            ids.iter()
                                .filter_map(|id| cg.symbols.get(id))
                                .filter(|s| s.kind != SymbolKind::File)
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut symbols: Vec<(String, usize)> = Vec::new();
                    for h in &d.hunks {
                        let (a, b) = h.new_range();
                        for s in crate::changes::touched(&syms, a, b) {
                            let item = (s.qualified_name.clone(), s.start_line);
                            if !symbols.contains(&item) {
                                symbols.push(item);
                            }
                        }
                    }
                    ChangedFile {
                        path: d.path.clone(),
                        status: match d.status {
                            crate::changes::FileStatus::Added => 'A',
                            crate::changes::FileStatus::Deleted => 'D',
                            crate::changes::FileStatus::Modified => 'M',
                            crate::changes::FileStatus::Renamed => 'R',
                        },
                        added: d.added_lines(),
                        removed: d.removed_lines(),
                        symbols,
                    }
                })
                .collect();
            files.extend(untracked.into_iter().map(|path| ChangedFile {
                path,
                status: '?',
                added: 0,
                removed: 0,
                symbols: Vec::new(),
            }));
            Ok(WorkTree { branch, files })
        })()
        .map_err(|e| format!("{e:#}"));
        let mut progress = crate::journal::progress_sections(&self.repo_root);
        if let Some(file) = progress.first().map(|s| s.file.clone())
            && let Some(git) = self.progress_git(&file)
        {
            crate::journal::mark_outdated_progress(&mut progress, &git);
        }
        Ok(ResumeData {
            journal,
            journal_total,
            notes,
            findings,
            progress,
            worktree,
        })
    }

    /// What git says happened since the progress file `file` last changed (`None`: not a
    /// git repository, or git failed). Offline and local only: `gh` is not asked, so the
    /// SessionStart briefing stays fast and works without a network.
    fn progress_git(&self, file: &str) -> Option<crate::journal::ProgressGit> {
        use crate::changes::git;
        let root = &self.repo_root;
        let mut out = crate::journal::ProgressGit::default();
        let status = git(root, &["status", "--porcelain", "--", file]).ok()?;
        let last = git(root, &["log", "-1", "--format=%H", "--", file]).ok()?;
        let last = last.trim();
        if !status.trim().is_empty() || last.is_empty() {
            out.file_dirty = true;
            return Some(out);
        }
        let range = format!("{last}..HEAD");
        let subjects = git(root, &["log", "--format=%s", &range]).ok()?;
        let subjects: Vec<&str> = subjects.lines().collect();
        out.commits_since = subjects.len();
        out.newest_since = subjects.first().map(|s| s.to_string());
        for s in &subjects {
            for n in crate::journal::merged_pr_numbers(s) {
                if !out.merged_since.iter().any(|(m, _)| *m == n) {
                    out.merged_since.push((n, s.to_string()));
                }
            }
        }
        // The default branch: the remote's HEAD when known, else a local main / master
        let default = git(
            root,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        )
        .ok()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .or_else(|| {
            ["main", "master"]
                .into_iter()
                .find(|b| {
                    git(
                        root,
                        &[
                            "rev-parse",
                            "--verify",
                            "--quiet",
                            &format!("refs/heads/{b}"),
                        ],
                    )
                    .is_ok()
                })
                .map(str::to_string)
        });
        if let Some(branch) = default
            && git(root, &["merge-base", "--is-ancestor", last, &branch]).is_ok()
        {
            out.on_default_branch = Some(
                branch
                    .strip_prefix("origin/")
                    .unwrap_or(&branch)
                    .to_string(),
            );
        }
        Some(out)
    }

    /// 19. Where work stands, for a new session or one after `/compact`: recent sessions,
    ///     what was tried and failed or is blocked, open review findings, decisions (flagged
    ///     when possibly outdated), the progress file's Next / pending-decision sections and
    ///     uncommitted work. Capped lists say how much they hide.
    pub fn resume(&self, max_tokens: usize) -> Result<QueryOutput> {
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;
        let data = self.resume_data(&db, &cg)?;
        let text = crate::journal::render_resume(&data, false);
        // Baseline: what an agent reads to rebuild this by hand: the changed files, the
        // whole progress file and the full `recall` list
        let changed: usize = data
            .worktree
            .as_ref()
            .map(|w| {
                w.files
                    .iter()
                    .filter_map(|f| cg.files.get(&f.path))
                    .map(|f| f.token_count)
                    .sum()
            })
            .unwrap_or(0);
        let progress_file: usize = data
            .progress
            .first()
            .and_then(|s| std::fs::read_to_string(self.repo_root.join(&s.file)).ok())
            .map(|t| count_tokens(&t))
            .unwrap_or(0);
        let notes: usize = data
            .notes
            .iter()
            .map(|v| count_tokens(&v.note.display_line()))
            .sum();
        let baseline = changed + progress_file + notes;
        self.finalize(
            &db,
            "resume",
            "",
            text,
            if max_tokens == 0 { 3000 } else { max_tokens },
            baseline,
            0,
            start,
        )
    }

    /// The short SessionStart version of [`Self::resume`]. Reads an existing index only:
    /// returns `Ok(None)` when the project has no `.trackertop/index.db` (never creates one,
    /// never re-indexes, so it stays fast), and records no telemetry.
    pub fn briefing(&self, max_tokens: usize) -> Result<Option<String>> {
        let db_path = self.repo_root.join(".trackertop").join("index.db");
        if !db_path.is_file() {
            return Ok(None);
        }
        let db = Database::open_path(&db_path)?;
        let cg = CodeGraph::load(&db)?;
        let data = self.resume_data(&db, &cg)?;
        let text = crate::journal::render_resume(&data, true);
        Ok(Some(truncate_to_tokens(&text, max_tokens).0))
    }
}
