# Design note: opt-in AI pass and GitHub Action wrapper

Status: **design only** (v0.8 milestone 6). No network code exists, and none is added by this note. It refines `docs/ROADMAP.md` phases 2 and 3 using what v0.8 taught us.

## 1. Who the AI pass is for

TrackerTop is a tool for the agent. In the MCP case **the calling agent is already the model**: sending its code to a second model from inside `review_changes` would add cost, a key and a privacy question, and would answer nothing the agent cannot check itself. So the AI pass splits in two:

| Context | Who reasons | What TrackerTop does |
|---|---|---|
| MCP (`review_changes`) | the calling agent | packs the evidence for each changed symbol; **no network** |
| CI (`trackertop review --ai`) | a model through the user's own API key | the same packs, sent in a two-stage sweep → verify |

The shared, offline part is the **review pack**: for each added or modified callable, ordered by `impact`:
- the diff hunk(s) that touch it, with base and head `file:line`;
- its head body, plus the base body when it was modified;
- 1-hop callers and callees (signatures only), and tests that reach it;
- notes on it, with the POSSIBLY OUTDATED flag;
- the deterministic findings already raised on it, so the model does not repeat them.

### Step A (next, offline): `review_changes(detail="packs")`
- Returns the packs within the token budget, highest-impact symbols first, and says how many were left out ("12 of 30 symbols packed; next: …").
- The agent reviews them itself and records a lead it rejects with `remember(kind=dismissed)`, as it does now.
- Done when: on this repository's own branch, the packs for the top 10 symbols fit in 6k tokens and every pack line points to a real `file:line` (golden test).

### Step B (later, CI only): `trackertop review --ai`
- **Opt-in twice:** the `--ai` flag *and* `ANTHROPIC_API_KEY` in the environment. The key is never written to disk or logs. Without both, nothing leaves the machine.
- **Two stages:**
  1. The sweep sends the packs in batches and asks for leads. Each lead must name `file:line` and quote the line.
  2. The verification step makes one call per lead, with the pack and the quoted line. It must answer confirmed or rejected and give a reason.
  - A lead whose quoted line is not in the working tree at that location is dropped before verification. This is the same deterministic check `broken-ref` uses.
- **Reporting:** confirmed P1 (data loss or security) and P2 (wrong behaviour) leads are findings with rule `ai`. P3 and unconfirmed leads are listed separately, collapsed, and never fail CI.
- **Stable ids:** `ai:<path>:<symbol>:<category>`, where category is a fixed slug list (null-deref, off-by-one, error-swallowed, …). Free-text claims change from run to run, so they cannot be ids. Dismissals and "resolved / new / unchanged" then work as they do for the deterministic rules.
- **Budget:** a hard token budget per run (default 60k input). The report shows tokens spent and the symbols left unreviewed. A budget overrun stops the run with a partial-coverage notice; the run never silently skips symbols.
- **Measured before shipping:** replay the merged PRs of this repository. Count confirmed leads that match a later fix as true positives, and the rest as false positives. Do not ship while confirmed false positives exceed 1 in 5.

## 2. GitHub Action (thin wrapper)

A composite action, `review-action/action.yml`, in this repository:

1. `actions/checkout` with `fetch-depth: 0`. The merge base and `cochange-miss` history need the full history; a shallow clone silently weakens `cochange-miss`, so the action fails if the history is shallow.
2. Download the release binary for the runner OS from this repository's GitHub release and verify its SHA-256 against the checksum file published with the release. It is never built from the PR.
3. Cache `.trackertop/review-base/` with `actions/cache`, keyed by base commit, TrackerTop version and `EXTRACTOR_REVISION`, so a revision bump never reuses a stale snapshot.
4. `trackertop review --base ${{ github.event.pull_request.base.sha }} --format sarif --fail-on <input, default high> > review.sarif`, then the same run in Markdown for the comment.
5. Upload `review.sarif` with `github/codeql-action/upload-sarif` (free on public repositories), and post **one** comment marked `<!-- trackertop-review -->`, editing it in place on later runs.

Security: the trigger is `pull_request`, never `pull_request_target`, and the deterministic review needs no secrets. Step B, the AI review, runs only on `workflow_dispatch` or a maintainer's `@trackertop review` comment. It uses the repository's own secret and is not available to fork PRs.

## 3. Open decisions (need the maintainer)

1. **Dismissals in CI.** Dismissals live in `.trackertop/` notes, which git ignores, so the CLI and the Action do not see the agent's dismissals. `--fail-on` can therefore fail CI on a finding the agent already dismissed. Options:
   - (a) commit `.trackertop-review.json`, with the id, reason, code hash and author of each dismissal;
   - (b) keep dismissals local and accept the gap.
   - Recommended: (a). The code hash keeps a dismissal valid only while the code is unchanged, exactly as `review_changes` does.
2. **Providers for step B.** Claude only (one prompt format to verify), or pluggable.
3. **Comment vs SARIF only.** SARIF needs code scanning to be enabled on the repository; the comment works everywhere.
