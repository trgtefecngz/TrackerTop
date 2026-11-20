---
name: trackertop-dev
description: TrackerTop-specific rules for development work in this repository (Rust MCP server + desktop app) — building without fighting the locked release binary, pre-push checks, progress files, releases, and which TrackerTop MCP tools to use. Load it together with the general `agent-dev-workflow` skill at the start of any implementation, bug fix, milestone run or release in TrackerTop.
---

# TrackerTop development layer

Follow the user-level `agent-dev-workflow` skill for the general method (start of work,
failing-test-first, budget and context rules, self-review, git hygiene). This file adds
only what is specific to TrackerTop; where the two differ, this file wins.

## Building

- `target/release/trackertop.exe` is usually **locked** by the running MCP server. Build
  and test into a separate target dir so you never touch it:

  ```sh
  CARGO_TARGET_DIR=target/check cargo build
  CARGO_TARGET_DIR=target/check cargo test
  ```

  (PowerShell: `$env:CARGO_TARGET_DIR = 'target/check'` first.)
- To swap the binary the MCP server runs: rename the locked exe (e.g. to
  `trackertop.old.exe`), then `cargo build --release`. The new binary is picked up when
  the MCP server restarts; say so instead of claiming the server already runs new code.

## Checks before every push

All green — CI (`.github/workflows/ci.yml`) runs the same set:

1. `cargo fmt --check` (or `cargo fmt`, then commit the result)
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo clippy --all-targets --no-default-features -- -D warnings` (no `gui` feature)
4. `cargo test` and `cargo test --no-default-features`

Use `CARGO_TARGET_DIR=target/check` for all of them. Prefer the `run_checks` MCP tool,
which runs them with filtered output.

## Progress, roadmap, release

- Progress files: `docs/V0.x_PROGRESS.md` (one per version). Read `## Next` and the
  decisions sections first; append a run-log entry per finished item; write the handoff
  into `## Next`. Open questions for the user go under `## Kararını bekleyenler`.
- Roadmap: `docs/ROADMAP.md` — update it when a milestone lands or scope changes.
- Release: `.github/workflows/release.yml` triggers on a pushed `v*` tag; the release
  notes come from `CHANGELOG.md`, so the version's CHANGELOG section must be complete
  before tagging. Tag and release only on the user's explicit word.
- The SessionStart hook (`trackertop hook-hint`) prints a short `resume` at startup and
  after `/compact`, including the progress file's `## Next` and `## Kararını bekleyenler`
  sections; keep those self-contained (paths, order, open decisions).

## Use TrackerTop's own MCP tools first

The repo dogfoods its own server (`trackertop`). Before shell greps or full-file reads:

| Need | Tool |
|---|---|
| Commands, CI, conventions at start | `project_playbook` |
| One function / struct | `read_symbol` with `path::name` (e.g. `src/review/rules.rs::some_fn`) |
| Several symbols at once | `read_symbols_batch` |
| Orientation / starting a task | `repo_map`, `task_context` |
| Signatures + line ranges of a file | `file_outline` |
| Where work stands (new session, after a compact) | `resume` |
| Catch up after edits | `changes_since` |
| Record a milestone, dead end or blocker | `remember` with `kind` `progress` / `tried` / `blocked` and a `reason` |
| Blast radius before refactor | `impact_analysis`, `diff_impact` |
| Build / lint / test | `run_checks` |
| Self-review of the diff | `review_changes` (v0.8+) |
| Record a settled question | `remember` with `kind` (`decision` / `dismissed`) and a `reason` |

If a tool gives a wrong or incomplete answer, that is a TrackerTop bug: note it in the
progress file (and add a failing test if it is in scope) rather than silently working
around it. The product exists to make the agent's own work easier — friction you hit is
signal.
