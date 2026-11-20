//! Workflow tools: run_checks, changes_since, review_changes and project_playbook.

use super::*;

impl TrackerEngine {
    /// 15. Runs the project's own test/lint/build/format commands (detected from project
    ///     files) and returns a compact summary that lists every recognized failure and
    ///     never reports an unexplained failure as success.
    pub fn run_checks(
        &self,
        kind: &str,
        extra_args: Option<&str>,
        timeout_secs: u64,
        max_tokens: usize,
    ) -> Result<QueryOutput> {
        use crate::checks::{CheckKind, detect_commands, render, run_command, summarize};
        let start = Instant::now();
        let kinds: Vec<CheckKind> = if kind.trim().eq_ignore_ascii_case("all") {
            vec![CheckKind::Format, CheckKind::Lint, CheckKind::Test]
        } else {
            vec![CheckKind::parse(kind).ok_or_else(|| {
                anyhow::anyhow!("unknown kind `{kind}`: use test, lint, build, format or all")
            })?]
        };
        let extra: Vec<String> = extra_args
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let all = detect_commands(&self.repo_root);
        let selected: Vec<_> = all.iter().filter(|c| kinds.contains(&c.kind)).collect();
        if !extra.is_empty() && selected.len() != 1 {
            anyhow::bail!(
                "extra_args needs exactly one matching command, but kind `{kind}` matches {}: {}",
                selected.len(),
                selected
                    .iter()
                    .map(|c| format!("`{}`", c.display(&[])))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let timeout = std::time::Duration::from_secs(timeout_secs.clamp(1, 3600));

        let mut summary_lines = Vec::new();
        let mut sections = Vec::new();
        let mut raw_tokens = 0usize;
        for k in &kinds {
            let cmds: Vec<_> = selected.iter().filter(|c| c.kind == *k).collect();
            if cmds.is_empty() {
                summary_lines.push(format!(
                    "- {}: NOT DETECTED (checked Cargo.toml, package.json scripts, go.mod, \
                     pytest/ruff config); run it yourself if the project has one",
                    k.as_str()
                ));
                continue;
            }
            for cmd in cmds {
                match run_command(&self.repo_root, cmd, &extra, timeout) {
                    Ok(run) => {
                        raw_tokens += count_tokens(&run.output);
                        let text = render(cmd, &run, &summarize(&run.output));
                        let result = text
                            .lines()
                            .find_map(|l| l.strip_prefix("RESULT: "))
                            .unwrap_or("?")
                            .to_string();
                        summary_lines.push(format!("- {} `{}`: {result}", k.as_str(), run.command));
                        sections.push(text);
                    }
                    Err(e) => summary_lines.push(format!(
                        "- {} `{}`: COULD NOT RUN: {e:#}",
                        k.as_str(),
                        cmd.display(&extra)
                    )),
                }
            }
        }

        let mut out = format!("# RUN_CHECKS ({kind})\n{}\n", summary_lines.join("\n"));
        for s in sections {
            out.push('\n');
            out.push_str(&s);
        }
        let db = Database::open_for_repo(&self.repo_root)?;
        self.finalize(
            &db,
            "run_checks",
            kind,
            out,
            if max_tokens == 0 { 1500 } else { max_tokens },
            raw_tokens,
            0,
            start,
        )
    }

    /// 16. What changed (working tree incl. staged/unstaged/untracked) since a git ref, mapped
    ///     onto symbols, optionally limited to paths or to hunks touching given symbols.
    pub fn changes_since(
        &self,
        git_ref: &str,
        paths: &[String],
        symbols: &[String],
        max_tokens: usize,
    ) -> Result<QueryOutput> {
        let start = Instant::now();
        let (db, cg) = self.load_fresh()?;
        let report = crate::changes::changes_since(&self.repo_root, &cg, git_ref, paths, symbols)?;
        self.finalize(
            &db,
            "changes_since",
            git_ref,
            report.text,
            if max_tokens == 0 { 2000 } else { max_tokens },
            report.baseline_tokens,
            report.files,
            start,
        )
    }

    /// Local review of the branch and uncommitted work against `base` (default branch when
    /// empty). Findings dismissed earlier with `remember(target=<id>, kind=dismissed)` are
    /// not shown again; they are listed by id and reason so nothing disappears silently.
    pub fn review_changes(&self, base: &str, max_tokens: usize) -> Result<QueryOutput> {
        let start = Instant::now();
        let base_ref = if base.trim().is_empty() {
            crate::review::default_base(&self.repo_root)?
        } else {
            base.trim().to_string()
        };
        let mut cs = crate::review::review(&self.repo_root, &base_ref)?;
        let (mut db, cg) = self.load_fresh()?;
        let notes = db.list_notes(None)?;
        let mut dismissed: Vec<(String, String)> = Vec::new();
        // A dismissal holds only while the code it was made about is unchanged; after that
        // the finding is raised again, saying it was dismissed before
        cs.findings.retain_mut(|f| {
            match notes
                .iter()
                .find(|n| n.kind == "dismissed" && n.target.trim().eq_ignore_ascii_case(&f.id))
            {
                Some(n) if self.note_still_current(&cg, n) => {
                    dismissed.push((f.id.clone(), n.reason.clone()));
                    false
                }
                Some(n) => {
                    f.evidence.push(format!(
                        "dismissed earlier (note #{}: {}), but its code changed since: re-check",
                        n.id, n.reason
                    ));
                    true
                }
                None => true,
            }
        });
        let mut text = crate::review::render_markdown(&cs);
        let mut extra = String::new();
        if !dismissed.is_empty() {
            extra.push_str(&format!(
                "Dismissed earlier, not shown again ({}):\n",
                dismissed.len()
            ));
            for (id, reason) in &dismissed {
                extra.push_str(&format!("- `{id}`: {reason}\n"));
            }
        }
        let raised: Vec<(String, String)> = cs
            .findings
            .iter()
            .map(|f| {
                let msg: String = f.message.chars().take(140).collect();
                let cut = if msg.len() < f.message.len() {
                    "…"
                } else {
                    ""
                };
                (f.id.clone(), format!("{msg}{cut} ({})", f.location))
            })
            .collect();
        let previous = db.swap_review_findings(&base_ref, &raised)?;
        extra.push_str(&review_delta(
            previous.as_deref(),
            &raised,
            &dismissed,
            &base_ref,
        ));
        if !cs.findings.is_empty() {
            extra.push_str(
                "After checking a finding that is not a problem: \
                 remember(target=\"<id>\", kind=\"dismissed\", reason=\"...\").\n",
            );
        }
        // Keep this next to the findings, ahead of the long symbol list a budget cuts first
        match text.find("\n## Changed symbols") {
            Some(at) => text.insert_str(at, &format!("\n{extra}")),
            None => text.push_str(&extra),
        }
        let baseline: usize = cs
            .files
            .iter()
            .filter_map(|f| cg.files.get(&f.path))
            .map(|f| f.token_count)
            .sum();
        self.finalize(
            &db,
            "review_changes",
            &base_ref,
            text,
            if max_tokens == 0 { 2500 } else { max_tokens },
            baseline,
            cs.files.len(),
            start,
        )
    }

    /// 17. Build/test/lint/format commands, CI steps, toolchain versions, conventions and
    ///     agent instruction files, each marked detected (declared by the project) or guessed.
    pub fn project_playbook(&self, max_tokens: usize) -> Result<QueryOutput> {
        let start = Instant::now();
        let pb = crate::playbook::build_playbook(&self.repo_root);
        let text = crate::playbook::render(&pb);
        let db = Database::open_for_repo(&self.repo_root)?;
        // Baseline: the files an agent would otherwise open to learn the same things
        let baseline: usize = pb
            .commands
            .iter()
            .chain(&pb.ci_steps)
            .chain(&pb.toolchain)
            .chain(&pb.conventions)
            .map(|i| i.source.split_whitespace().next().unwrap_or("").to_string())
            .collect::<HashSet<String>>()
            .iter()
            .filter_map(|f| std::fs::read_to_string(self.repo_root.join(f)).ok())
            .map(|t| count_tokens(&t))
            .sum();
        self.finalize(
            &db,
            "project_playbook",
            "playbook",
            text,
            if max_tokens == 0 { 1200 } else { max_tokens },
            baseline,
            0,
            start,
        )
    }
}
/// "Since the previous review": findings resolved (raised before, gone now and not merely
/// dismissed), new and unchanged, by stable id.
fn review_delta(
    previous: Option<&[(String, String)]>,
    raised: &[(String, String)],
    dismissed: &[(String, String)],
    base_ref: &str,
) -> String {
    let Some(previous) = previous else {
        return format!(
            "Since the previous review: none recorded against `{base_ref}`; the next \
             review_changes will list resolved / new / unchanged findings.\n"
        );
    };
    let before: HashSet<&str> = previous.iter().map(|(id, _)| id.as_str()).collect();
    let now: HashSet<&str> = raised.iter().map(|(id, _)| id.as_str()).collect();
    let hidden: HashSet<&str> = dismissed.iter().map(|(id, _)| id.as_str()).collect();
    let resolved: Vec<&(String, String)> = previous
        .iter()
        .filter(|(id, _)| !now.contains(id.as_str()) && !hidden.contains(id.as_str()))
        .collect();
    let new: Vec<&str> = raised
        .iter()
        .map(|(id, _)| id.as_str())
        .filter(|id| !before.contains(id))
        .collect();
    let unchanged = raised.len() - new.len();
    let mut s = format!(
        "Since the previous review against `{base_ref}`: {} resolved, {} new, {unchanged} \
         unchanged.\n",
        resolved.len(),
        new.len()
    );
    const SHOWN: usize = 10;
    for (id, summary) in resolved.iter().take(SHOWN) {
        s.push_str(&format!("- resolved `{id}`: was {summary}\n"));
    }
    s.push_str(&more_note(
        resolved.len(),
        SHOWN.min(resolved.len()),
        "resolved",
    ));
    for id in new.iter().take(SHOWN) {
        s.push_str(&format!("- new `{id}`\n"));
    }
    s.push_str(&more_note(new.len(), SHOWN.min(new.len()), "new"));
    s
}
