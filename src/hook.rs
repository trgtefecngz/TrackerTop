//! `trackertop hook-hint`: the Claude Code SessionStart hook. It prints the classic one-line
//! hint plus, when the project has a TrackerTop index, a short `resume` briefing, so a new
//! session (source `startup`) or one after `/compact` (source `compact`) starts knowing
//! where work stands.
//!
//! The hook must never break or stall session start: all work runs on a worker thread with
//! a deadline, and on any error, panic or timeout only the classic hint is printed.

use crate::context::TrackerEngine;
use serde_json::Value;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

/// The hint printed before v0.9, and still the fallback.
pub const HOOK_HINT: &str = "TrackerTop MCP is active for this workspace. Prefer `repo_map`, `task_context`, `smart_grep`, `file_outline`, `read_symbol`, `read_symbols_batch`, and `diff_impact` BEFORE raw grep or reading full files (saves 80-99% tokens).";

/// Token budget for the briefing (the task asks for roughly 300-500).
pub const BRIEFING_TOKENS: usize = 450;

/// Deadline for the briefing; past it the classic hint alone is printed.
pub const BRIEFING_DEADLINE: Duration = Duration::from_millis(900);

/// The nearest directory at or above `start` with a TrackerTop index, without leaving the
/// git repository `start` is in (a worktree must not pick up its parent checkout's index).
pub fn find_index_root(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join(".trackertop").join("index.db").is_file() {
            return Some(d.to_path_buf());
        }
        if d.join(".git").exists() {
            return None;
        }
        dir = d.parent();
    }
    None
}

/// Where the session runs and why it started, from the hook's stdin JSON
/// (`{"cwd": ..., "source": "startup" | "resume" | "clear" | "compact", ...}`).
fn parse_input(stdin_json: &str) -> (Option<PathBuf>, String) {
    let v: Value = serde_json::from_str(stdin_json.trim()).unwrap_or(Value::Null);
    let cwd = v
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let source = v
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("startup")
        .to_string();
    (cwd, source)
}

/// The briefing alone (no fallback): `Ok(None)` when the project has no index.
pub fn briefing_for(start: &Path, source: &str) -> anyhow::Result<Option<String>> {
    let Some(root) = find_index_root(start) else {
        return Ok(None);
    };
    let engine = TrackerEngine::new(&root);
    let Some(text) = engine.briefing(BRIEFING_TOKENS)? else {
        return Ok(None);
    };
    let why = if source == "compact" {
        "Context was just compacted. "
    } else {
        ""
    };
    Ok(Some(format!(
        "{why}{text}\nRecord milestones, dead ends and blockers with remember(kind=progress|tried|blocked, reason=...) so the next session sees them."
    )))
}

/// Full hook output for the given stdin JSON. `repo` (the `--repo` flag) wins over the
/// stdin `cwd`, which wins over `CLAUDE_PROJECT_DIR` and the current directory.
pub fn hook_output(stdin_json: &str, repo: Option<PathBuf>, deadline: Duration) -> String {
    let (cwd, source) = parse_input(stdin_json);
    let start = repo
        .or(cwd)
        .or_else(|| std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok());
    let Some(start) = start else {
        return HOOK_HINT.to_string();
    };
    let (tx, rx) = mpsc::channel();
    // Detached on purpose: a stuck git or a locked DB must not hold up the session
    std::thread::spawn(move || {
        let res = std::panic::catch_unwind(|| briefing_for(&start, &source));
        let _ = tx.send(res);
    });
    match rx.recv_timeout(deadline) {
        Ok(Ok(Ok(Some(brief)))) => format!("{HOOK_HINT}\n\n{brief}"),
        _ => HOOK_HINT.to_string(),
    }
}

/// Reads the hook's stdin JSON without ever blocking session start: nothing is read from a
/// terminal, and a pipe that stays open is abandoned after a short wait.
pub fn read_hook_stdin() -> String {
    if std::io::stdin().is_terminal() {
        return String::new();
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::stdin().take(64 * 1024).read_to_string(&mut s);
        let _ = tx.send(s);
    });
    rx.recv_timeout(Duration::from_millis(200))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_gives_cwd_and_source_and_garbage_is_harmless() {
        let (cwd, src) = parse_input(r#"{"cwd":"C:/p","source":"compact","session_id":"x"}"#);
        assert_eq!(cwd, Some(PathBuf::from("C:/p")));
        assert_eq!(src, "compact");
        let (cwd, src) = parse_input("not json");
        assert_eq!((cwd, src.as_str()), (None, "startup"));
    }

    #[test]
    fn index_root_is_found_upward_but_not_across_a_repo_boundary() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outer = tmp.path();
        std::fs::create_dir_all(outer.join(".trackertop")).unwrap();
        std::fs::write(outer.join(".trackertop/index.db"), b"").unwrap();
        std::fs::create_dir_all(outer.join("src/deep")).unwrap();
        assert_eq!(
            find_index_root(&outer.join("src/deep")).as_deref(),
            Some(outer)
        );
        // A nested checkout (worktree) with its own .git stops the walk
        let wt = outer.join("wt");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: elsewhere").unwrap();
        assert_eq!(find_index_root(&wt.join("src")), None);
    }

    #[test]
    fn no_index_means_the_classic_hint_and_no_trackertop_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let input = format!(
            r#"{{"cwd":{},"source":"startup"}}"#,
            serde_json::to_string(&tmp.path().to_string_lossy()).unwrap()
        );
        let out = hook_output(&input, None, Duration::from_secs(5));
        assert_eq!(out, HOOK_HINT);
        assert!(!tmp.path().join(".trackertop").exists());
    }

    #[test]
    fn a_broken_index_falls_back_to_the_classic_hint() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".trackertop")).unwrap();
        std::fs::write(
            tmp.path().join(".trackertop/index.db"),
            b"this is not sqlite",
        )
        .unwrap();
        let out = hook_output("", Some(tmp.path().to_path_buf()), Duration::from_secs(5));
        assert_eq!(out, HOOK_HINT);
    }
}
