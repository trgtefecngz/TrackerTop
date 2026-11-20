# Roadmap

## v0.7: agent productivity (released as v0.7.0)

TrackerTop is a tool for the AI agent itself. v0.7 makes an agent's work cheaper and more correct: fewer wasted tool calls, fewer wrong turns, and never a silently incomplete answer.

| # | Deliverable | Status |
|---|---|---|
| 1 | Correctness foundation: tree-sitter TS/TSX/JS extractor; every tool output states its coverage (capped lists say how much is hidden, "not indexed" vs "not found", heuristic extractors flagged); smart_grep proven against a plain scan | Done |
| 2 | Agent tools: `run_checks` (compact test/lint/build summary that never hides a failure), `changes_since` (what changed since an index generation or git ref), `project_playbook` (build/test/lint commands, detected vs guessed), decision memory (Markdown notes under `.trackertop/notes/`, decisions and dismissed review leads surfaced on read) | Done |
| 3 | Honest metrics: telemetry tagged by source (MCP client, CLI, UI, tests); conservative baseline next to the upper bound, savings as a range; log of what the agent asked for | Done |
| 4 | Nested ("matryoshka") Code Graph view: project → communities → files → symbols, aggregated edges, stable group ids | Done |

Released in v0.7.0 together with fixes from a pre-release review: qualified Rust calls no longer bind to namesakes, member notes no longer leak to other types, `changes_since` works in monorepo packages, and `run_checks` no longer reports passing output as failures or hides a crashed test binary.

## Order after v0.7

Everything below serves the agent first: catch mistakes before they are pushed, never answer from a guess, and do not lose what earlier sessions learned.

| Release | Theme | Main deliverables |
|---|---|---|
| v0.8 | Local review | `trackertop review` engine and a `review_changes` MCP tool; note provenance and staleness; small tool gaps |
| v0.9 | Precision | LSP bridge for exact call resolution; affected-test selection for `run_checks` |
| v1.0 | Editing and continuity | Symbol-level editing; working memory across sessions (`resume`) |
| Later | Breadth | Structural search/replace; version-exact dependency docs; more tree-sitter languages |

## v0.8: local, graph-grounded review (`trackertop review`), released as v0.8.0

### Goal

Review a change using the code graph TrackerTop already builds: what the change touches, what depends on it, what is untested, and what it likely breaks.

**Local first.** The engine runs on the working tree, before a commit or push, in seconds, offline, and with no monthly review quota. The agent calls it as an MCP tool (`review_changes`) at the end of every task. A GitHub Action is only a thin wrapper around the same engine, for contributors who do not run TrackerTop and for CI gating; it comes last.

### Also in v0.8

- **Note provenance.** Each note stores the commit and a hash of the code it is attached to, and shows them when it is surfaced.
- **Stale-note warning.** If that code changed after the note was written, the note is still shown, but flagged as possibly outdated. An old "dismissed" lead must never silently hide a bug in code that has since changed.
- **Review uses decision memory.** Findings dismissed with `remember(kind=dismissed)` are not raised again, unless the code changed since (see stale-note warning).
- **Small tool gaps found while dogfooding:**
  - `read_symbol` on `mod tests` and on impl tails.
  - "Who uses this type?" for enums and structs.
  - Bound `run_checks` output memory.
  - Python and Go on tree-sitter.

### Feature, not a separate tool

The expensive part of a graph-grounded reviewer already exists in TrackerTop:
- incremental indexing and language extractors;
- the symbol graph with PageRank and communities;
- `impact_analysis`, `diff_impact`, git co-change edges and `health_check`.

A separate tool would have to duplicate or extract all of that. `review` is a new consumer of the same engine: one binary, one index, one release pipeline.

Two things stay deliberate:
- **Offline by default.** The deterministic part never touches the network. The AI pass is opt-in and clearly marked.
- **A clear boundary.** Review logic lives in its own module (`src/review/`), so it could become a separate crate later if that is ever needed.

### Phase 1: deterministic review (offline, no AI)

`trackertop review [--base main] [--head HEAD] [--format md|json|sarif] [--fail-on <severity>]`

1. **Map the diff to symbols.** Read `git diff --unified=0 base...head` and map changed line ranges to symbols, using their `start_line..end_line`.
2. **Index both sides.** Index `head` (the working tree) and `base` (a temporary `git worktree`, cached by commit). Each check below compares the two graphs.
3. **Checks.** Each check produces a finding with a stable ID (`rule:path:symbol`):

| Rule | Finding |
|---|---|
| `impact` | Changed symbols with many transitive dependents (blast radius) |
| `untested` | Changed or added symbols with no `tests` edge |
| `new-cycle` | A dependency cycle present in head but not in base |
| `broken-ref` | A removed or renamed symbol that is still referenced |
| `cochange-miss` | Files that changed together with these files in ≥ N past commits but not in this PR |
| `bloat` | Files that crossed the token-bloat threshold in this PR |
| `orphan` | Code made unreachable by this PR |

4. **Output.**
   - A Markdown summary: scope, blast radius, tests to run, findings.
   - Machine-readable JSON and SARIF, so GitHub code scanning can show inline annotations.
   - Exit codes for CI gating.

### Phase 2: optional AI pass (`--ai`)

Phase 1 can only find structural problems. For logic bugs, an opt-in pass sends each changed symbol to a model, packed like `task_context` with its callers, callees and the diff hunk.

- **Two stages, borrowing what worked in this release's reviews:**
  1. A sweep proposes leads.
  2. A separate verification call must confirm each lead against the actual code before it is reported as a bug. Unverified leads are shown separately, collapsed, and never block.
- **Severity:** P1 (data loss or security), P2 (incorrect behaviour), P3 (edge case). Only confirmed P1/P2 findings can fail CI.
- **Provider:** the Claude API through the user's own `ANTHROPIC_API_KEY`. The key is read from the environment and never stored. Each run has a token and cost budget, and the report shows what it spent.

### Phase 3: GitHub integration

- A reusable Action: `uses: trgtefecngz/TrackerTop/review-action@v0.8`.
- It posts **one comment and edits it in place** instead of piling up new ones, plus SARIF upload for inline annotations.
- Re-review on demand with a `@trackertop review` comment (an `issue_comment` workflow), not on every push.
- Commands: `@trackertop wrong <id>`, `@trackertop suppress <rule>`. Dismissed findings are remembered in the repo (`.trackertop/review-suppressions.json`) and never raised again.

### Avoiding endless review loops

Fixing reviewer leads one round after another produced diminishing returns: later rounds were mostly heuristic-parser edge cases, and some fixes created new leads. To avoid that:
- Every review shows **"resolved since last review / new / unchanged"** per stable finding ID.
- A lead that a previous round dismissed as "not reproducible" stays dismissed.
- Re-reviews are explicit (on a comment), not automatic on every push.
- The report says when only unconfirmed P3 leads remain, which means the PR is ready.

### Prerequisite: tree-sitter for TypeScript/JavaScript (done in v0.7)

The TypeScript/JavaScript extractor is line-based: test spans, braces, strings, template literals and regexes are recognised by hand-written scanning. Most late review findings in v0.6.0 were edge cases of that scanner. Both the review's symbol mapping and the AI context depend on accurate spans, so this comes first:
- Use `tree-sitter-typescript` (TS and TSX grammars) and `tree-sitter-javascript`. Rust and HTML already use tree-sitter 0.25.
- Keep the current extractor's tests as the acceptance suite; they describe the expected spans.
- Python and Go are next in line for the same treatment.

### Milestones

| # | Deliverable | Done when |
|---|---|---|
| 1 | Tree-sitter TS/JS extractor | Done in v0.7 |
| 2 | Diff → symbol mapping + base/head indexing | `review` lists changed symbols correctly for add/modify/delete/rename |
| 3 | Deterministic rules + Markdown/JSON/SARIF | Each rule has fixture tests; runs offline in < 5 s on a 1k-file repo |
| 4 | `review_changes` MCP tool + dismissed-lead memory | The agent can review its own uncommitted work; dismissed findings stay dismissed until their code changes |
| 5 | Note provenance + stale-note warning | A note whose code changed is flagged; covered by tests |
| 6 | Opt-in AI pass with verification | False-positive rate measured on this repo's PR history; budget enforced |
| 7 | GitHub Action + single updating comment + commands | Thin wrapper over the engine; works on this repository's own PRs |

### Open questions

- Inline PR comments vs. SARIF annotations only. SARIF needs GitHub code scanning, which is free for public repositories.
- Whether to support other model providers in phase 2, or only Claude.
- Whether to cache the base-commit index in CI (actions/cache) or rebuild it on every run.

## v0.9: precision

- **LSP bridge.** When rust-analyzer, tsserver, pyright or gopls is available, ask it which function a call really reaches, and mark each edge as exact or heuristic. Tree-sitter stays the fallback. This removes the remaining guesswork in call edges (trait calls, methods on values, re-exports) instead of dropping those edges.
- **Affected-test selection.** `run_checks` can run only the tests that reach the changed symbols, for fast feedback while working. The full suite still runs before a commit and in CI.

## v1.0: editing and continuity

- **Symbol-level editing.** `replace_symbol_body`, `insert_after_symbol` and `rename_symbol`. Rename updates every reference the graph knows about and reports the references it could not resolve. Edits stop failing on whitespace mismatches.
- **Working memory (`resume`).** *Delivered early (feat/continuity): `resume`, the work journal, the SessionStart briefing and the AI Memory timeline.* What the last sessions were doing, what was tried and failed, what is still open, and the decisions taken, in a short summary at the start of a session. The desktop app shows the same log, so a person can follow what the agent did and why.

## Later

- Structural search and replace (syntax-aware patterns instead of text).
- Version-exact dependency docs: API signatures for the versions in `Cargo.lock` / `package-lock.json`, read locally.
- Watch mode: keep the index warm without a query.
- More tree-sitter languages: C/C++, C#, Java.
- Signed release binaries (Windows Authenticode, macOS notarization).
