# Roadmap

## v0.7: agent productivity (in progress, `feat/v0.7-agent-tools`)

TrackerTop is a tool for the AI agent itself. v0.7 makes an agent's work cheaper and more correct: fewer wasted tool calls, fewer wrong turns, and never a silently incomplete answer.

| # | Deliverable | Status |
|---|---|---|
| 1 | Correctness foundation: tree-sitter TS/TSX/JS extractor; every tool output states its coverage (capped lists say how much is hidden, "not indexed" vs "not found", heuristic extractors flagged); smart_grep proven against a plain scan | Done |
| 2 | Agent tools: `run_checks` (compact test/lint/build summary that never hides a failure), `changes_since` (what changed since an index generation or git ref), `project_playbook` (build/test/lint commands, detected vs guessed), decision memory (Markdown notes under `.trackertop/notes/`, decisions and dismissed review leads surfaced on read) | Next |
| 3 | Honest metrics: telemetry tagged by source (MCP client, CLI, UI, tests); conservative baseline next to the upper bound, savings as a range; optional log of what the agent asked for | Planned |
| 4 | Nested ("matryoshka") Code Graph view: project → communities → files → symbols | Lowest priority |

The graph-grounded PR review moves to v0.8.

## v0.8: graph-grounded PR review (`trackertop review`)

### Goal

Review a pull request using the code graph TrackerTop already builds: what the change touches, what depends on it, what is untested, and what it likely breaks. It runs locally with `trackertop review` and on GitHub as an Action that comments on the PR.

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

- A reusable Action: `uses: trgtefecngz/TrackerTop/review-action@v0.7`.
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
| 4 | GitHub Action + single updating comment + commands | Works on this repository's own PRs |
| 5 | Opt-in AI pass with verification | False-positive rate measured on this repo's PR history; budget enforced |

### Open questions

- Inline PR comments vs. SARIF annotations only. SARIF needs GitHub code scanning, which is free for public repositories.
- Whether to support other model providers in phase 2, or only Claude.
- Whether to cache the base-commit index in CI (actions/cache) or rebuild it on every run.

## Later

- Watch mode: keep the index warm without a query.
- More tree-sitter languages: C/C++, C#, Java.
- Signed release binaries (Windows Authenticode, macOS notarization).
