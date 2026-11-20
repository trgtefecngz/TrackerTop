//! Cross-session continuity: the work journal (every MCP tool call plus the agent's own
//! `progress` / `tried` / `blocked` entries) and the `resume` briefing built from it, so a
//! new session, or one after `/compact`, starts knowing where work stands.
//!
//! Everything here is pure (data in, text out) so it can be tested without git or a DB; the
//! engine (`TrackerEngine::resume`) gathers the inputs.

use crate::model::{JournalEntry, ProjectNote};
use chrono::{DateTime, Local};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Longest compacted query / outcome kept in a journal row.
const MAX_FIELD_CHARS: usize = 160;

/// Arguments that say how to answer, not what was asked: left out of the journal.
const NOISE_ARGS: &[&str] = &["max_tokens", "include_deps", "timeout_secs", "limit"];

/// Tools whose result is itself worth remembering (pass/fail, findings), not just "ok".
const RESULT_TOOLS: &[&str] = &["run_checks", "review_changes"];

fn clip(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `{"symbol": "Foo", "max_tokens": 900}` → `symbol=Foo`. Arrays are comma-joined; the
/// whole line is clipped (with `…`) so a long `remember` text cannot bloat the journal.
pub fn compact_query(args: &Value) -> String {
    let Some(obj) = args.as_object() else {
        return clip(&args.to_string(), MAX_FIELD_CHARS);
    };
    let mut parts = Vec::new();
    for (k, v) in obj {
        if NOISE_ARGS.contains(&k.as_str()) {
            continue;
        }
        let val = match v {
            Value::String(s) => s.clone(),
            Value::Array(a) => a
                .iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| x.to_string())
                })
                .collect::<Vec<_>>()
                .join(","),
            Value::Null => continue,
            other => other.to_string(),
        };
        parts.push(format!("{k}={val}"));
    }
    clip(&parts.join(" "), MAX_FIELD_CHARS)
}

/// How a call went, in one short line: `error: …` for failures, the RESULT / findings line
/// for checks and reviews, a NOT FOUND line when the lookup missed, else `ok`.
pub fn outcome_of(tool: &str, result: Result<&str, &str>) -> String {
    match result {
        Err(e) => format!(
            "error: {}",
            clip(e.lines().next().unwrap_or(""), MAX_FIELD_CHARS)
        ),
        Ok(text) => {
            if RESULT_TOOLS.contains(&tool)
                && let Some(line) = text.lines().find(|l| {
                    l.starts_with("RESULT:") || l.starts_with("## Findings") || l.contains("FAILED")
                })
            {
                return format!("ok: {}", clip(line.trim_start_matches("## "), 120));
            }
            if tool == "remember"
                && let Some(first) = text.lines().next()
            {
                return format!("ok: {}", clip(first, 120));
            }
            match text
                .lines()
                .find(|l| l.contains("NOT FOUND") || l.contains("NOT INDEXED"))
            {
                Some(l) => format!("ok: {}", clip(l, 120)),
                None => "ok".to_string(),
            }
        }
    }
}

/// `2026-11-20T19:44:33+00:00` → `2026-11-20 22:44` (local time). Unparseable text is
/// returned as is rather than dropped.
pub fn fmt_ts(ts: &str) -> String {
    DateTime::parse_from_rfc3339(ts)
        .map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|_| ts.to_string())
}

/// `HH:MM` of a timestamp (for the end of a same-day range).
fn fmt_time(ts: &str) -> String {
    DateTime::parse_from_rfc3339(ts)
        .map(|t| t.with_timezone(&Local).format("%H:%M").to_string())
        .unwrap_or_else(|_| ts.to_string())
}

/// Query keys that name what a call was about, most specific first.
const FOCUS_KEYS: &[&str] = &[
    "symbol", "symbols", "path", "paths", "from", "to", "pattern", "query", "task",
];

/// The value of `key=` in a compacted query (values may contain spaces; a value runs
/// until the next ` word=`).
fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    let tag = format!("{key}=");
    let start = if query.starts_with(&tag) {
        tag.len()
    } else {
        query.find(&format!(" {tag}"))? + 1 + tag.len()
    };
    let rest = &query[start..];
    let end = rest
        .match_indices(' ')
        .map(|(i, _)| i)
        .find(|&i| {
            let next = &rest[i + 1..];
            next.split_once('=').is_some_and(|(k, _)| {
                !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
        })
        .unwrap_or(rest.len());
    Some(rest[..end].trim())
}

/// The main thing a call was about (`symbol=Foo` → `Foo`), for "focus" lists. Memory
/// calls are left out: their content is reported as notes.
fn call_target(e: &JournalEntry) -> Option<String> {
    if matches!(e.tool.as_str(), "remember" | "recall" | "resume") {
        return None;
    }
    FOCUS_KEYS
        .iter()
        .find_map(|k| query_value(&e.query, k))
        .filter(|v| !v.is_empty() && v.chars().count() <= 60)
        .map(str::to_string)
}

/// One agent session's tool calls, summarized.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub session: String,
    pub source: String,
    pub first_ts: String,
    pub last_ts: String,
    pub calls: usize,
    /// `(tool, calls)`, most used first.
    pub tools: Vec<(String, usize)>,
    /// Most queried symbols / paths, most frequent first.
    pub focus: Vec<String>,
    /// Failed calls, newest first.
    pub errors: Vec<JournalEntry>,
    /// `run_checks` / `review_changes` results, newest first.
    pub results: Vec<JournalEntry>,
}

/// Groups journal rows (any order) by session; the most recently active session first.
/// Rows without a session id (should not happen for MCP calls) are grouped by day.
pub fn summarize_sessions(entries: &[JournalEntry]) -> Vec<SessionSummary> {
    let mut groups: HashMap<String, Vec<&JournalEntry>> = HashMap::new();
    for e in entries {
        let key = if e.session.is_empty() {
            format!("day {}", e.timestamp.get(..10).unwrap_or("?"))
        } else {
            e.session.clone()
        };
        groups.entry(key).or_default().push(e);
    }
    let mut out: Vec<SessionSummary> = groups
        .into_iter()
        .map(|(session, mut rows)| {
            rows.sort_by_key(|e| e.id);
            let mut tools: HashMap<&str, usize> = HashMap::new();
            let mut focus: HashMap<String, (usize, i64)> = HashMap::new();
            for e in &rows {
                *tools.entry(e.tool.as_str()).or_default() += 1;
                if let Some(t) = call_target(e) {
                    let slot = focus.entry(t).or_insert((0, 0));
                    slot.0 += 1;
                    slot.1 = slot.1.max(e.id);
                }
            }
            let mut tools: Vec<(String, usize)> =
                tools.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
            tools.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let mut focus: Vec<(String, (usize, i64))> = focus.into_iter().collect();
            focus.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let newest_first = |pred: &dyn Fn(&JournalEntry) -> bool| -> Vec<JournalEntry> {
                rows.iter()
                    .rev()
                    .filter(|e| pred(e))
                    .map(|e| (*e).clone())
                    .collect()
            };
            SessionSummary {
                source: rows.last().map(|e| e.source.clone()).unwrap_or_default(),
                first_ts: rows
                    .first()
                    .map(|e| e.timestamp.clone())
                    .unwrap_or_default(),
                last_ts: rows.last().map(|e| e.timestamp.clone()).unwrap_or_default(),
                calls: rows.len(),
                tools,
                focus: focus.into_iter().map(|(k, _)| k).collect(),
                errors: newest_first(&|e: &JournalEntry| e.is_error()),
                results: newest_first(&|e: &JournalEntry| {
                    !e.is_error() && RESULT_TOOLS.contains(&e.tool.as_str())
                }),
                session,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.last_ts
            .cmp(&a.last_ts)
            .then_with(|| a.session.cmp(&b.session))
    });
    out
}

/// Ids of `blocked` entries that a later `progress` entry closed: same target (other than
/// the catch-all `project`), or a mention of `#<id>` in its text or reason.
pub fn resolved_blocks(notes: &[ProjectNote]) -> HashSet<i64> {
    let mentions = |text: &str, id: i64| {
        let tag = format!("#{id}");
        text.match_indices(&tag).any(|(at, _)| {
            !text[at + tag.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        })
    };
    notes
        .iter()
        .filter(|b| b.kind == "blocked")
        .filter(|b| {
            notes.iter().any(|p| {
                p.kind == "progress"
                    && p.id > b.id
                    && ((b.target != "project" && p.target == b.target)
                        || mentions(&p.content, b.id)
                        || mentions(&p.reason, b.id))
            })
        })
        .map(|b| b.id)
        .collect()
}

/// A note plus, when the code it was written about changed, the warning to show with it.
#[derive(Debug, Clone)]
pub struct NoteView {
    pub note: ProjectNote,
    pub outdated: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OpenFinding {
    pub base_ref: String,
    pub id: String,
    /// Message plus `(file:line)`.
    pub summary: String,
    pub reviewed_at: String,
}

/// A `## Next` / `## Kararını bekleyenler` section of the progress file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressSection {
    pub file: String,
    pub heading: String,
    /// 1-based line of the heading.
    pub line: usize,
    /// Non-empty lines of the section body.
    pub lines: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ChangedFile {
    pub path: String,
    /// `M`, `A`, `D`, `R` or `?` (untracked).
    pub status: char,
    pub added: usize,
    pub removed: usize,
    /// Innermost symbols the change touches: `(qualified name, current start line)`.
    pub symbols: Vec<(String, usize)>,
}

#[derive(Debug, Clone)]
pub struct WorkTree {
    pub branch: String,
    pub files: Vec<ChangedFile>,
}

/// Everything `resume` reports, gathered by the engine.
#[derive(Debug, Clone)]
pub struct ResumeData {
    /// Newest first.
    pub journal: Vec<JournalEntry>,
    /// Rows in the journal table (may exceed `journal.len()`).
    pub journal_total: usize,
    /// All notes, newest first.
    pub notes: Vec<NoteView>,
    pub findings: Vec<OpenFinding>,
    pub progress: Vec<ProgressSection>,
    /// `Err` says why the working tree could not be read (not a git repo, …).
    pub worktree: Result<WorkTree, String>,
}

struct Caps {
    sessions: usize,
    blocked: usize,
    tried: usize,
    findings: usize,
    decisions: usize,
    progress_log: usize,
    section_lines: usize,
    files: usize,
    /// Longest progress-file line shown before it is clipped with `…`.
    line_chars: usize,
}

const FULL: Caps = Caps {
    sessions: 6,
    blocked: 10,
    tried: 10,
    findings: 12,
    decisions: 12,
    progress_log: 8,
    section_lines: 40,
    files: 20,
    line_chars: 400,
};

const BRIEF: Caps = Caps {
    sessions: 2,
    blocked: 3,
    tried: 3,
    findings: 3,
    decisions: 3,
    progress_log: 1,
    section_lines: 8,
    files: 4,
    line_chars: 140,
};

fn hidden_line(out: &mut String, hidden: usize, how: &str) {
    if hidden > 0 {
        out.push_str(&format!("  (+{hidden} more not shown; {how})\n"));
    }
}

fn note_item(v: &NoteView) -> String {
    let n = &v.note;
    let mut s = format!(
        "- {} #{} [{}] {}",
        fmt_ts(&n.created_at),
        n.id,
        n.target,
        n.content
    );
    if !n.reason.is_empty() {
        s.push_str(&format!(" (why: {})", n.reason));
    }
    // A progress / tried / blocked entry is history: its file changing afterwards is
    // expected, so only rules and decisions carry the outdated warning
    if let Some(w) = &v.outdated
        && !matches!(n.kind.as_str(), "progress" | "tried" | "blocked")
    {
        s.push_str(&format!(" [{w}]"));
    }
    s.push('\n');
    s
}

/// Open findings: the latest review per base, minus findings dismissed since (a dismissal
/// whose code changed does not count, matching `review_changes`).
fn open_findings(d: &ResumeData) -> Vec<&OpenFinding> {
    let dismissed: HashSet<String> = d
        .notes
        .iter()
        .filter(|v| v.note.kind == "dismissed" && v.outdated.is_none())
        .map(|v| v.note.target.trim().to_ascii_lowercase())
        .collect();
    d.findings
        .iter()
        .filter(|f| !dismissed.contains(&f.id.to_ascii_lowercase()))
        .collect()
}

/// The `resume` text. `brief` is the SessionStart version (a few hundred tokens); the full
/// one is the MCP tool. Lists that are capped say how many items are hidden and how to see
/// them.
pub fn render_resume(d: &ResumeData, brief: bool) -> String {
    let caps = if brief { &BRIEF } else { &FULL };
    let mut out = String::new();
    out.push_str(if brief {
        "## TrackerTop: where work stands (short; call the `resume` tool for the full picture)\n"
    } else {
        "# RESUME: where work stands\n"
    });

    // Uncommitted work
    match &d.worktree {
        Ok(w) if w.files.is_empty() => {
            out.push_str(&format!("Branch `{}`: no uncommitted changes.\n", w.branch));
        }
        Ok(w) => {
            out.push_str(&format!(
                "\n## Uncommitted work on `{}` ({} files)\n",
                w.branch,
                w.files.len()
            ));
            for f in w.files.iter().take(caps.files) {
                let mut line = format!("- {} {}", f.status, f.path);
                if f.status != '?' {
                    line.push_str(&format!(" (+{} -{})", f.added, f.removed));
                }
                if !f.symbols.is_empty() {
                    let syms: Vec<String> = f
                        .symbols
                        .iter()
                        .take(4)
                        .map(|(n, l)| format!("{n} :{l}"))
                        .collect();
                    line.push_str(&format!(": {}", syms.join(", ")));
                    if f.symbols.len() > 4 {
                        line.push_str(&format!(" +{} more", f.symbols.len() - 4));
                    }
                }
                out.push_str(&line);
                out.push('\n');
            }
            hidden_line(
                &mut out,
                w.files.len().saturating_sub(caps.files),
                "changes_since HEAD lists all",
            );
        }
        Err(e) => out.push_str(&format!("Uncommitted work: unknown ({e}).\n")),
    }

    // Progress file sections, `## Next` first (it is what to do now)
    let mut sections: Vec<&ProgressSection> = d.progress.iter().collect();
    sections.sort_by_key(|s| !wanted_next(&s.heading));
    for s in sections {
        out.push_str(&format!(
            "\n## {} ({}:{})\n",
            s.heading.trim_start_matches('#').trim(),
            s.file,
            s.line
        ));
        if s.lines.is_empty() {
            out.push_str("(empty)\n");
        }
        for l in s.lines.iter().take(caps.section_lines) {
            let indent: String = l.chars().take_while(|c| c.is_whitespace()).collect();
            out.push_str(&indent);
            out.push_str(&clip(l, caps.line_chars));
            out.push('\n');
        }
        hidden_line(
            &mut out,
            s.lines.len().saturating_sub(caps.section_lines),
            &format!("read {} from line {}", s.file, s.line),
        );
    }

    // Journal entries the agent wrote on purpose
    let resolved = resolved_blocks(&d.notes.iter().map(|v| v.note.clone()).collect::<Vec<_>>());
    let of_kind =
        |k: &str| -> Vec<&NoteView> { d.notes.iter().filter(|v| v.note.kind == k).collect() };
    let blocked: Vec<&NoteView> = of_kind("blocked")
        .into_iter()
        .filter(|v| !resolved.contains(&v.note.id))
        .collect();
    let closed = of_kind("blocked").len() - blocked.len();
    if !blocked.is_empty() {
        out.push_str(&format!("\n## Blocked ({} open)\n", blocked.len()));
        for v in blocked.iter().take(caps.blocked) {
            out.push_str(&note_item(v));
        }
        hidden_line(
            &mut out,
            blocked.len().saturating_sub(caps.blocked),
            "recall(\"blocked\")",
        );
    }
    if closed > 0 && !brief {
        out.push_str(&format!(
            "({closed} earlier blocked entries were closed by later progress; recall(\"blocked\") lists them)\n"
        ));
    }
    let tried = of_kind("tried");
    if !tried.is_empty() {
        out.push_str(&format!(
            "\n## Tried and failed ({}; do not repeat without a new reason)\n",
            tried.len()
        ));
        for v in tried.iter().take(caps.tried) {
            out.push_str(&note_item(v));
        }
        hidden_line(
            &mut out,
            tried.len().saturating_sub(caps.tried),
            "recall(\"tried\")",
        );
    }

    // Open review findings
    let findings = open_findings(d);
    if !findings.is_empty() {
        out.push_str(&format!(
            "\n## Open review findings ({}, from the last review_changes)\n",
            findings.len()
        ));
        for f in findings.iter().take(caps.findings) {
            out.push_str(&format!(
                "- {} vs `{}`: `{}` {}\n",
                fmt_ts(&f.reviewed_at),
                f.base_ref,
                f.id,
                f.summary
            ));
        }
        hidden_line(
            &mut out,
            findings.len().saturating_sub(caps.findings),
            "review_changes lists all",
        );
    }

    // Decisions: possibly outdated ones first, they need a re-check
    let mut decisions = of_kind("decision");
    decisions.sort_by(|a, b| match (a.outdated.is_some(), b.outdated.is_some()) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => b.note.id.cmp(&a.note.id),
    });
    if !decisions.is_empty() {
        let outdated = decisions.iter().filter(|v| v.outdated.is_some()).count();
        out.push_str(&format!(
            "\n## Decisions ({}, {} POSSIBLY OUTDATED)\n",
            decisions.len(),
            outdated
        ));
        for v in decisions.iter().take(caps.decisions) {
            out.push_str(&note_item(v));
        }
        hidden_line(
            &mut out,
            decisions.len().saturating_sub(caps.decisions),
            "recall(\"decision\")",
        );
    }

    let progress = of_kind("progress");
    if !progress.is_empty() {
        out.push_str(&format!("\n## Progress log ({})\n", progress.len()));
        for v in progress.iter().take(caps.progress_log) {
            out.push_str(&note_item(v));
        }
        hidden_line(
            &mut out,
            progress.len().saturating_sub(caps.progress_log),
            "recall(\"progress\")",
        );
    }

    // Recent sessions from the tool-call journal
    let sessions = summarize_sessions(&d.journal);
    if !sessions.is_empty() {
        out.push_str(&format!(
            "\n## Recent agent sessions ({} of {} shown)\n",
            sessions.len().min(caps.sessions),
            sessions.len()
        ));
        for s in sessions.iter().take(caps.sessions) {
            out.push_str(&session_block(s, brief));
        }
        hidden_line(
            &mut out,
            sessions.len().saturating_sub(caps.sessions),
            "older sessions are in the desktop app's AI Memory timeline",
        );
        if d.journal_total > d.journal.len() {
            out.push_str(&format!(
                "(summarized from the newest {} of {} journaled calls)\n",
                d.journal.len(),
                d.journal_total
            ));
        }
    }

    let counts = |k: &str| d.notes.iter().filter(|v| v.note.kind == k).count();
    let (dismissed, plain) = (counts("dismissed"), counts("note"));
    if (dismissed > 0 || plain > 0) && !brief {
        out.push_str(&format!(
            "\nAlso saved: {plain} notes, {dismissed} dismissed review leads (recall lists them).\n"
        ));
    }
    if d.journal.is_empty() && d.notes.is_empty() && d.progress.is_empty() {
        out.push_str(
            "\nNo journal yet: TrackerTop records each MCP tool call from now on. Record \
             milestones with remember(kind=\"progress\"), dead ends with kind=\"tried\" and \
             blockers with kind=\"blocked\" (each with a reason) so the next session sees them.\n",
        );
    }
    out
}

fn session_block(s: &SessionSummary, brief: bool) -> String {
    let same_day = s.first_ts.get(..10) == s.last_ts.get(..10);
    let range = if same_day {
        format!("{}–{}", fmt_ts(&s.first_ts), fmt_time(&s.last_ts))
    } else {
        format!("{} – {}", fmt_ts(&s.first_ts), fmt_ts(&s.last_ts))
    };
    let tools: Vec<String> = s
        .tools
        .iter()
        .take(if brief { 3 } else { 6 })
        .map(|(t, n)| format!("{t}×{n}"))
        .collect();
    let mut out = format!(
        "- {range} ({}) {} calls: {}",
        if s.source.is_empty() {
            "unknown client"
        } else {
            &s.source
        },
        s.calls,
        tools.join(", ")
    );
    if s.tools.len() > tools.len() {
        out.push_str(&format!(" +{} tools", s.tools.len() - tools.len()));
    }
    out.push('\n');
    if !s.focus.is_empty() {
        let n = if brief { 3 } else { 6 };
        out.push_str(&format!(
            "  focus: {}\n",
            s.focus
                .iter()
                .take(n)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(r) = s.results.first() {
        out.push_str(&format!(
            "  last {} {}: {}\n",
            r.tool,
            fmt_time(&r.timestamp),
            r.outcome
        ));
    }
    if !s.errors.is_empty() {
        let n = if brief { 1 } else { 3 };
        out.push_str(&format!("  errors ({}):", s.errors.len()));
        for e in s.errors.iter().take(n) {
            out.push_str(&format!(" {} `{}` → {};", e.tool, e.query, e.outcome));
        }
        out.push('\n');
    }
    out
}

/// One row of the desktop app's "what the agent did and why" timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineItem {
    pub timestamp: String,
    /// `calls`, `error`, `check`, or a note kind (`progress`, `decision`, …).
    pub kind: String,
    pub title: String,
    /// The why (a note's reason) or the call outcome.
    pub detail: String,
}

/// Merges journal rows and notes into a newest-first timeline. Runs of ordinary calls in
/// one session collapse into a single "N calls: …" row; failures and check / review
/// results get their own rows, as do notes.
pub fn timeline(
    entries: &[JournalEntry],
    notes: &[ProjectNote],
    limit: usize,
) -> Vec<TimelineItem> {
    let mut rows: Vec<&JournalEntry> = entries.iter().collect();
    rows.sort_by_key(|e| e.id);
    let mut items: Vec<TimelineItem> = Vec::new();
    let mut run: Vec<&JournalEntry> = Vec::new();
    let flush = |run: &mut Vec<&JournalEntry>, items: &mut Vec<TimelineItem>| {
        if run.is_empty() {
            return;
        }
        let mut tools: Vec<(String, usize)> = Vec::new();
        for e in run.iter() {
            match tools.iter_mut().find(|(t, _)| *t == e.tool) {
                Some(slot) => slot.1 += 1,
                None => tools.push((e.tool.clone(), 1)),
            }
        }
        tools.sort_by_key(|t| std::cmp::Reverse(t.1));
        let mut focus: Vec<String> = Vec::new();
        for e in run.iter().rev() {
            if let Some(t) = call_target(e)
                && !focus.contains(&t)
                && focus.len() < 4
            {
                focus.push(t);
            }
        }
        let last = run.last().expect("non-empty run");
        items.push(TimelineItem {
            timestamp: last.timestamp.clone(),
            kind: "calls".to_string(),
            title: format!(
                "{} call{}: {}",
                run.len(),
                if run.len() == 1 { "" } else { "s" },
                tools
                    .iter()
                    .take(4)
                    .map(|(t, n)| format!("{t}×{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            detail: if focus.is_empty() {
                String::new()
            } else {
                format!("looked at {}", focus.join(", "))
            },
        });
        run.clear();
    };
    for e in rows {
        let special = e.is_error() || RESULT_TOOLS.contains(&e.tool.as_str());
        if run.first().is_some_and(|r| r.session != e.session) || special {
            flush(&mut run, &mut items);
        }
        if special {
            items.push(TimelineItem {
                timestamp: e.timestamp.clone(),
                kind: if e.is_error() { "error" } else { "check" }.to_string(),
                title: format!("{} {}", e.tool, e.query).trim().to_string(),
                detail: e.outcome.clone(),
            });
        } else {
            run.push(e);
        }
    }
    flush(&mut run, &mut items);
    for n in notes {
        items.push(TimelineItem {
            timestamp: n.created_at.clone(),
            kind: n.kind.clone(),
            title: format!("[{}] {}", n.target, n.content),
            detail: n.reason.clone(),
        });
    }
    // RFC 3339 UTC strings from the same writer sort chronologically
    items.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    items.truncate(limit);
    items
}

/// Splits a name into text and number runs so `V0.10` sorts after `V0.9`.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    fn parts(s: &str) -> Vec<(bool, u64, String)> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut digits = false;
        for c in s.chars() {
            if c.is_ascii_digit() != digits && !cur.is_empty() {
                out.push((digits, cur.parse().unwrap_or(0), std::mem::take(&mut cur)));
            }
            digits = c.is_ascii_digit();
            cur.push(c.to_ascii_lowercase());
        }
        if !cur.is_empty() {
            out.push((digits, cur.parse().unwrap_or(0), cur));
        }
        out
    }
    let (pa, pb) = (parts(a), parts(b));
    for (x, y) in pa.iter().zip(&pb) {
        let ord = if x.0 && y.0 {
            x.1.cmp(&y.1)
        } else {
            x.2.cmp(&y.2)
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    pa.len().cmp(&pb.len())
}

/// Headings `resume` lifts out of the progress file.
fn wanted_heading(h: &str) -> bool {
    wanted_next(h)
        || h.trim_start_matches('#')
            .trim()
            .to_lowercase()
            .starts_with("kararını bekleyenler")
}

fn wanted_next(h: &str) -> bool {
    h.trim_start_matches('#')
        .trim()
        .to_lowercase()
        .starts_with("next")
}

/// `## Next` and `## Kararını bekleyenler` from the newest `docs/*PROGRESS*.md` (natural
/// order of names, so `V0.10_PROGRESS.md` beats `V0.9_PROGRESS.md`). Empty when there is
/// no such file.
pub fn progress_sections(repo_root: &Path) -> Vec<ProgressSection> {
    let docs = repo_root.join("docs");
    let Ok(dir) = std::fs::read_dir(&docs) else {
        return Vec::new();
    };
    let mut names: Vec<String> = dir
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            n.to_ascii_lowercase().ends_with(".md") && n.to_ascii_uppercase().contains("PROGRESS")
        })
        .collect();
    names.sort_by(|a, b| natural_cmp(a, b));
    let Some(name) = names.pop() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(docs.join(&name)) else {
        return Vec::new();
    };
    parse_progress_sections(&format!("docs/{name}"), &text)
}

/// The wanted `##` sections of a progress file (pure; see [`progress_sections`]).
pub fn parse_progress_sections(file: &str, text: &str) -> Vec<ProgressSection> {
    let mut out: Vec<ProgressSection> = Vec::new();
    let mut current: Option<ProgressSection> = None;
    let mut in_code = false;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
        }
        let is_heading = !in_code && (line.starts_with("## ") || line.starts_with("# "));
        if is_heading {
            if let Some(s) = current.take() {
                out.push(s);
            }
            if line.starts_with("## ") && wanted_heading(line) {
                current = Some(ProgressSection {
                    file: file.to_string(),
                    heading: line.to_string(),
                    line: i + 1,
                    lines: Vec::new(),
                });
            }
            continue;
        }
        if let Some(s) = current.as_mut()
            && !line.trim().is_empty()
        {
            s.lines.push(line.to_string());
        }
    }
    if let Some(s) = current.take() {
        out.push(s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(
        id: i64,
        session: &str,
        ts: &str,
        tool: &str,
        query: &str,
        outcome: &str,
    ) -> JournalEntry {
        JournalEntry {
            id,
            timestamp: ts.to_string(),
            session: session.to_string(),
            source: "mcp:test".to_string(),
            tool: tool.to_string(),
            query: query.to_string(),
            outcome: outcome.to_string(),
        }
    }

    fn note(id: i64, kind: &str, target: &str, content: &str, reason: &str) -> ProjectNote {
        ProjectNote {
            id,
            target: target.to_string(),
            content: content.to_string(),
            created_at: format!("2026-11-20T10:{:02}:00+00:00", id % 60),
            kind: kind.to_string(),
            reason: reason.to_string(),
            commit: String::new(),
            code_hash: String::new(),
            session: String::new(),
        }
    }

    #[test]
    fn queries_drop_answer_shaping_args_and_are_clipped() {
        let q = compact_query(&json!({"symbol": "Foo", "max_tokens": 900, "include_deps": true}));
        assert_eq!(q, "symbol=Foo");
        let q = compact_query(&json!({"paths": ["a.rs", "b.rs"], "since": "HEAD"}));
        assert_eq!(q, "paths=a.rs,b.rs since=HEAD");
        let long = "x ".repeat(400);
        let q = compact_query(&json!({ "note": long }));
        assert!(
            q.chars().count() <= MAX_FIELD_CHARS && q.ends_with('…'),
            "{q}"
        );
    }

    #[test]
    fn focus_comes_from_the_most_specific_key_and_keeps_spaces() {
        let e = |tool: &str, q: &str| entry(1, "s", "t", tool, q, "ok");
        assert_eq!(
            call_target(&e("smart_grep", "file_filter=src/ pattern=fn main")).as_deref(),
            Some("fn main")
        );
        assert_eq!(
            call_target(&e("task_context", "task=fix the parser bug")).as_deref(),
            Some("fix the parser bug")
        );
        assert_eq!(call_target(&e("remember", "kind=tried note=x")), None);
        assert_eq!(call_target(&e("repo_map", "")), None);
        assert_eq!(query_value("xsymbol=a symbol=b", "symbol"), Some("b"));
    }

    #[test]
    fn outcomes_keep_errors_results_and_misses() {
        assert_eq!(outcome_of("read_symbol", Err("boom\nmore")), "error: boom");
        assert_eq!(outcome_of("read_symbol", Ok("# fn x\nbody")), "ok");
        let checks = "# RUN_CHECKS test: `cargo test`\nRESULT: FAILED (exit 101) in 3.0s\n";
        assert_eq!(
            outcome_of("run_checks", Ok(checks)),
            "ok: RESULT: FAILED (exit 101) in 3.0s"
        );
        assert_eq!(
            outcome_of("review_changes", Ok("# REVIEW\n\n## Findings (2)\n")),
            "ok: Findings (2)"
        );
        assert!(
            outcome_of("read_symbol", Ok("SYMBOL `Q` NOT FOUND IN INDEX")).contains("NOT FOUND")
        );
    }

    #[test]
    fn sessions_are_grouped_newest_first_with_errors_and_results() {
        let rows = vec![
            entry(
                1,
                "s1",
                "2026-11-19T10:00:00+00:00",
                "read_symbol",
                "symbol=Old",
                "ok",
            ),
            entry(
                2,
                "s2",
                "2026-11-20T10:00:00+00:00",
                "read_symbol",
                "symbol=Foo",
                "ok",
            ),
            entry(
                3,
                "s2",
                "2026-11-20T10:01:00+00:00",
                "read_symbol",
                "symbol=Foo",
                "ok",
            ),
            entry(
                4,
                "s2",
                "2026-11-20T10:02:00+00:00",
                "smart_grep",
                "pattern=bar",
                "error: bad",
            ),
            entry(
                5,
                "s2",
                "2026-11-20T10:03:00+00:00",
                "run_checks",
                "kind=test",
                "ok: RESULT: PASSED",
            ),
        ];
        let s = summarize_sessions(&rows);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].session, "s2");
        assert_eq!(s[0].calls, 4);
        assert_eq!(s[0].tools[0], ("read_symbol".to_string(), 2));
        assert_eq!(s[0].focus[0], "Foo");
        assert_eq!(s[0].errors.len(), 1);
        assert_eq!(s[0].results[0].outcome, "ok: RESULT: PASSED");
        assert_eq!(s[1].session, "s1");
    }

    #[test]
    fn blocked_entries_close_on_same_target_or_id_mention_only() {
        let notes = vec![
            note(12, "progress", "project", "unrelated, see #123", ""),
            note(11, "progress", "src/a.rs", "parser fixed", ""),
            note(10, "blocked", "src/a.rs", "parser crashes", "upstream bug"),
            note(9, "blocked", "project", "waiting for CI quota", "quota"),
            note(8, "progress", "project", "earlier work, #7 done", ""),
            note(7, "blocked", "project", "needs user decision", "scope"),
            note(5, "blocked", "project", "#12 is not mine", "x"),
            note(4, "progress", "project", "fixes #5", ""),
        ];
        let r = resolved_blocks(&notes);
        assert!(r.contains(&10), "same target, later progress");
        assert!(r.contains(&7), "mentioned by id");
        assert!(
            !r.contains(&9),
            "`project` target alone does not close a block; #123 ≠ #9"
        );
        assert!(
            !r.contains(&5),
            "a progress entry written before the block cannot close it"
        );
    }

    #[test]
    fn progress_sections_take_next_and_pending_decisions_but_not_code_blocks() {
        let text = "# V0.9\n## Next\n1. do A\n\n2. do B\n```\n## Next fake\n```\n## Run log\nx\n## Kararını bekleyenler\n- merge?\n";
        let s = parse_progress_sections("docs/P.md", text);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].line, 2);
        assert_eq!(
            s[0].lines,
            vec!["1. do A", "2. do B", "```", "## Next fake", "```"]
        );
        assert_eq!(s[1].heading, "## Kararını bekleyenler");
        assert_eq!(s[1].lines, vec!["- merge?"]);
    }

    #[test]
    fn newest_progress_file_wins_by_natural_order() {
        assert_eq!(
            natural_cmp("V0.10_PROGRESS.md", "V0.9_PROGRESS.md"),
            Ordering::Greater
        );
        assert_eq!(
            natural_cmp("V0.8_PROGRESS.md", "V0.7_PROGRESS.md"),
            Ordering::Greater
        );
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("docs")).unwrap();
        std::fs::write(tmp.path().join("docs/V0.9_PROGRESS.md"), "## Next\nold\n").unwrap();
        std::fs::write(tmp.path().join("docs/V0.10_PROGRESS.md"), "## Next\nnew\n").unwrap();
        std::fs::write(tmp.path().join("docs/ROADMAP.md"), "## Next\nno\n").unwrap();
        let s = progress_sections(tmp.path());
        assert_eq!(s[0].file, "docs/V0.10_PROGRESS.md");
        assert_eq!(s[0].lines, vec!["new"]);
    }

    fn data(notes: Vec<ProjectNote>, journal: Vec<JournalEntry>) -> ResumeData {
        ResumeData {
            journal_total: journal.len(),
            journal,
            notes: notes
                .into_iter()
                .map(|n| NoteView {
                    note: n,
                    outdated: None,
                })
                .collect(),
            findings: Vec::new(),
            progress: Vec::new(),
            worktree: Ok(WorkTree {
                branch: "main".to_string(),
                files: Vec::new(),
            }),
        }
    }

    #[test]
    fn capped_lists_say_how_much_is_hidden() {
        let notes: Vec<ProjectNote> = (1..=15)
            .rev()
            .map(|i| note(i, "tried", "project", &format!("approach {i}"), "failed"))
            .collect();
        let full = render_resume(&data(notes.clone(), Vec::new()), false);
        assert!(full.contains("Tried and failed (15"), "{full}");
        assert!(
            full.contains("(+5 more not shown; recall(\"tried\"))"),
            "{full}"
        );
        let brief = render_resume(&data(notes, Vec::new()), true);
        assert!(brief.contains("(+12 more not shown"), "{brief}");
        // Newest first: the newest entry is shown, the oldest is not
        assert!(
            brief.contains("approach 15") && !brief.contains("approach 1 "),
            "{brief}"
        );
    }

    #[test]
    fn dismissed_findings_drop_out_unless_their_code_changed() {
        let mut d = data(
            vec![note(
                1,
                "dismissed",
                "orphan:src/a.rs:f",
                "fine",
                "used by macro",
            )],
            Vec::new(),
        );
        d.findings = vec![
            OpenFinding {
                base_ref: "main".into(),
                id: "orphan:src/a.rs:f".into(),
                summary: "f lost its last caller (src/a.rs:3)".into(),
                reviewed_at: "2026-11-20T10:00:00+00:00".into(),
            },
            OpenFinding {
                base_ref: "main".into(),
                id: "untested:src/b.rs:g".into(),
                summary: "g is untested (src/b.rs:9)".into(),
                reviewed_at: "2026-11-20T10:00:00+00:00".into(),
            },
        ];
        let out = render_resume(&d, false);
        assert!(out.contains("Open review findings (1"), "{out}");
        assert!(
            out.contains("src/b.rs:9") && !out.contains("src/a.rs:3"),
            "{out}"
        );
        d.notes[0].outdated = Some("POSSIBLY OUTDATED".into());
        let out = render_resume(&d, false);
        assert!(out.contains("Open review findings (2"), "{out}");
    }

    #[test]
    fn outdated_decisions_are_listed_first_and_counted() {
        let mut d = data(
            vec![
                note(3, "decision", "a", "newest decision", "r"),
                note(2, "decision", "b", "old decision", "r"),
            ],
            Vec::new(),
        );
        d.notes[1].outdated = Some("POSSIBLY OUTDATED: the code changed".into());
        let out = render_resume(&d, false);
        assert!(out.contains("Decisions (2, 1 POSSIBLY OUTDATED)"), "{out}");
        let old = out.find("old decision").unwrap();
        let new = out.find("newest decision").unwrap();
        assert!(old < new, "outdated first:\n{out}");
    }

    #[test]
    fn timeline_collapses_call_runs_but_keeps_errors_checks_and_notes() {
        let rows = vec![
            entry(
                1,
                "s1",
                "2026-11-20T10:00:00+00:00",
                "read_symbol",
                "symbol=Foo",
                "ok",
            ),
            entry(
                2,
                "s1",
                "2026-11-20T10:01:00+00:00",
                "smart_grep",
                "pattern=x",
                "ok",
            ),
            entry(
                3,
                "s1",
                "2026-11-20T10:02:00+00:00",
                "run_checks",
                "kind=test",
                "ok: RESULT: PASSED",
            ),
            entry(
                4,
                "s2",
                "2026-11-20T11:00:00+00:00",
                "read_symbol",
                "symbol=Bar",
                "error: nope",
            ),
        ];
        let mut n = note(5, "tried", "src/a.rs", "regex parser", "too slow");
        n.created_at = "2026-11-20T10:30:00+00:00".into();
        let t = timeline(&rows, &[n], 50);
        let kinds: Vec<&str> = t.iter().map(|i| i.kind.as_str()).collect();
        assert_eq!(kinds, vec!["error", "tried", "check", "calls"]);
        assert_eq!(t[3].title, "2 calls: read_symbol×1, smart_grep×1");
        assert_eq!(t[1].detail, "too slow");
    }
}
