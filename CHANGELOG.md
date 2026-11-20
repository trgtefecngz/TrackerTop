# Changelog

All notable changes to TrackerTop are documented here.

## [0.8.0] - 2026-11-20


### Changed: desktop app redesign
- New design system: one accent colour on dark neutrals, flat cards with 1px borders, a spacing and type scale. Glows, aurora gradients and saturated fills are gone.
- Inter (UI) and JetBrains Mono (code) are embedded in the binary (SIL Open Font License; see `assets/fonts/`).
- Code Graph:
  - Nested is the default layout.
  - Every layout is force-directed and clustered: Clusters by community, Folders by folder.
  - Node colour shows the community; node size shows PageRank.
  - Edges are curved. In Clusters and Folders, edges between groups are bundled below 110% zoom, and the coverage line says how many.
  - More labels appear as you zoom in. A label that would cover another is skipped, except for the hovered or selected node and its neighbours.
  - Hovering a node dims everything not connected to it.
  - Drilling in or out is animated.
  - A minimap shows the current view and can be clicked to move it.
  - Edges that leave a nested level are drawn as pills that name where they go and how many there are. Their counts add up to the coverage line's "edges leave this scope". Click a pill to open that level.
- Agent activity layer:
  - Nodes glow by how many MCP calls named them in the last 7 days.
  - Files with uncommitted changes get a dashed ring.
  - The latest agent session is drawn as a numbered path you can replay.
  - Badges mark notes (N), decisions (D) and health findings (!).
  - A telemetry query counts toward a node only when it names a single file or symbol. Free-text queries and ambiguous names are not counted.
- An inspector panel shows the selected node: PageRank, tokens, edges, agent activity, notes and findings.

### Added: `trackertop review` (step 1)
- `trackertop review [--base <ref>] [--format md|json]` lists which symbols your branch and uncommitted work added, removed or modified compared with the merge base, with `file:line` on both sides. The base is checked out and indexed separately under `.trackertop/review-base/`, without touching your branch, index or working tree.
- Review findings, each with a stable id, severity, `file:line` and evidence:
  - `broken-ref`: a removed symbol that a surviving caller still calls, verified against the working tree;
  - `impact`: a modified function with many code dependents, with the tests that reach it;
  - `untested`: new or modified code that no test reaches;
  - `new-cycle`: an import cycle the change created between JS/TS/Python/Go files;
  - `bloat`: a file that grew past 3,500 tokens;
  - `orphan`: a function whose last caller this change removed or rewrote, reported only when its name appears nowhere else in the project's code (so a function passed as a value or named in a registry is never called unused);
  - `cochange-miss`: a file that changed together with a file you touched in at least 4 commits and 60% of that file's recent history (at the base, bulk commits ignored), but not in this change. Reported as a habit with the commits it rests on, never as proof.
- New MCP tool `review_changes`: the same review for the agent, findings first. A finding checked and dismissed with `remember(target=<finding id>, kind=dismissed, reason=...)` is not raised again and is listed with its reason instead.
- `trackertop review --format sarif` writes SARIF 2.1.0 for code-scanning UIs (each finding keeps its stable id as a fingerprint), and `--fail-on low|medium|high` exits with status 1 when a finding at or above that severity is raised.
- `review_changes` says what changed since the previous review against the same base: findings resolved (with what they were), new, and unchanged, by stable id. A dismissed finding is not counted as resolved.

### Added: note provenance
- Notes, decisions and dismissed leads record the commit and a hash of the code they are about. When that code changes, the note is still shown but marked **POSSIBLY OUTDATED**, so an old decision is re-checked instead of trusted. A dismissed review finding is raised again once its code changes.

### Changed: `run_checks` memory
- Command output is kept up to 16 MB from the start plus the last 8–16 MB, instead of the whole log in memory. When the middle is dropped, the report says so (**PARTIAL OUTPUT**), so a failure printed there is never silently lost.

### Fixed: symbol lookups never guess silently
- `read_symbol`, `read_symbols_batch`, `call_graph` and `impact_analysis` used to fall back to the best fuzzy match without saying so. For example, `src/review/rules.rs::tests` returned an unrelated test. Lookups now try an exact symbol id, `path::name`, an exact qualified name, then an exact name, in that order. When several symbols share the name, the answer says **AMBIGUOUS** and lists them; a fuzzy match is labelled as the closest match.

### Added: line ranges in `read_symbol`
- `read_symbol("src/lib.rs:L120-160")` reads a line range that no symbol covers, such as an impl tail or a test module's helpers, and names the innermost symbol around it. A range past the end of the file is clamped and says so; one that starts past the end, or a file that is not indexed, gets an explicit error.

### Changed: Python and Go on tree-sitter
- The Python and Go extractors now read a syntax tree instead of scanning lines. Text in strings and comments is no longer taken for a call, a `def` inside a docstring is no longer a symbol, and braces inside Go strings no longer end a function early.
- A syntax error, such as an unclosed bracket in a file being edited, no longer hides every later definition. The file is re-parsed piece by piece from its top-level definitions. Python definitions inside `for`, `while` and `match` blocks are kept.
- Fixed along the way: multi-line Python and Go signatures (the function used to end at its first line), grouped Go `type ( ... )` declarations (previously missed), generic Go receivers (`Stack[T]` is now `Stack`), and multi-line `from x import (...)` lists.

### Fixed: stale graph after upgrading TrackerTop
- The index records which extractor revision parsed it. An index written by another TrackerTop version is re-parsed in full on its next run. Before, files whose content had not changed kept the references an older extractor produced, so call-edge fixes never reached them: on this repository, `TrackerEngine::finalize_inner` showed 0 callers instead of 19.

### Fixed: "who uses this type" (Rust)
- Type uses inside function bodies are now edges: `match` arms on an enum (`Level::High`), struct literals, `let` annotations, type paths such as `Rec::new()`. Before, only signatures counted, so `impact_analysis` on `Severity` listed 6 direct users instead of 18.
- A Rust type use only resolves to a type (struct, enum, trait, alias). It never binds to a function that shares the name, and an external type gets no edge.

### Fixed: call edges (all tools)
- Calls inside Rust macro arguments (`assert_eq!(parse(x), 3)`, `vec![a.b()]`) are now edges. Most Rust test-to-code edges were missing, so tests looked unrelated to the code they test.
- A Rust method call on a value (`x.flush()`) resolves only to methods, never to a free function of the same name; a call into a library type gets no edge.
- A call never resolves to a module or doc section of the same name (`review()` used to point at `pub mod review`).
- A bare Rust call `f()` never resolves to a method, which needs `x.f()` or `Type::f()`.

## [0.7.0] - 2026-11-20

v0.7 is the agent-productivity release (see `docs/ROADMAP.md`).

### Added: nested graph view (desktop app)
- The Code Graph tab has a new layout, **Nested**. It shows one level at a time: project → communities → files → symbols. Click a node to drill in; use the breadcrumbs to go back up.
- Edges are aggregated per level. A line between two communities stands for the number of symbol-level dependencies between them.
- A coverage line states what is not drawn:
  - edges that leave the current scope;
  - edges inside a single node;
  - children cut by the 400-node cap.
- Group ids are stable across re-indexes. A community is named by its first file path, not by a number that shifts when files are added.
- Fixed: a community's label could differ between runs on ties, because it depended on hash-map order. It is now deterministic.

### Changed: honest metrics
- Every telemetry row records who made the call:
  - `mcp:<client name>`, taken from the MCP `initialize` request (for example `mcp:claude-code`);
  - `cli` or `ui`;
  - a custom tag from `TRACKERTOP_TELEMETRY_SOURCE`, such as `benchmark`;
  - `legacy` for rows recorded before 0.7.
  The telemetry table is migrated in place.
- Statistics count real AI-agent usage (MCP calls) by default, in `token_stats`, `trackertop stats`, the report, the project list and the desktop app. Calls from the CLI, UI, tests, benchmarks and legacy rows are listed per source and not mixed in.
- Savings are shown as a range:
  - **conservative**: targeted reads of at most 1,500 tokens from at most 5 touched files, never less than what was sent;
  - **upper bound**: the previous figure, every touched file read whole.
  The conservative figure is computed from stored columns, so it covers old rows too.
- `token_stats` and `trackertop stats` end with the latest requests (tool and query), so you can see what the agent asked for.

### Added: decision memory
- `remember` takes a `kind` (`note`, `decision` or `dismissed`, the last for a review lead that was checked and rejected) and a `reason`, which is required for decisions and dismissed leads.
- Notes are shown, labelled with kind and reason, wherever the related code is read: `read_symbol`, `read_symbols_batch`, `file_outline` (notes on the file or any of its symbols) and `repo_map`. Settled questions are not investigated again.
- Every note is also written as Markdown in `.trackertop/notes/`. If the index database is deleted or rebuilt, the notes are restored from those files.
- The existing notes table is migrated in place; old notes become kind `note`.

### Fixed: pre-release review
- A note on a type's member (`Store::load`) was shown when reading any symbol with the same bare name (`Config::load`), so a dismissed lead on one could silence the other. Member notes now match only their own symbol; module-qualified notes (`registry::load`) still match the free function.
- `changes_since` in a project that is a subfolder of a git repository (a monorepo package) mapped no hunks, because git reported paths from the repository top level. Diffs and old file versions are now read relative to the project.
- `run_checks`:
  - `cargo test -- --show-output` no longer reports passing tests as failures.
  - Jest's `● Validation Warning:` and `● Deprecation Warning:` headings are no longer counted as failing tests.
  - A test binary that crashes (signal, stack overflow) is reported even when other failures were recognized.
- Notes restored from `.trackertop/notes/` keep their text when it contains a `Reason:` paragraph, and files re-saved with Windows line endings are no longer skipped.

### Fixed: false call edges in Rust
- A qualified call such as `Stroke::new(...)` was resolved by its last segment alone, so it was tied to any `new` in the project. In this repository, `TrackerTopApp::new` showed 18 callers instead of 2, and `impact_analysis` reported the wrong blast radius.
  - `Type::f` now resolves only to that exact type's function, and `Self::f` to the surrounding impl's type.
  - `module::f` resolves only inside that module.
  - Calls into external types and crates (`std::fs::read`), enum variants and trait paths get no edge. A missing edge is better than a wrong one.
- Turbofish generics (`Vec::<u8>::new`) no longer cut the callee name short.

### Fixed: memory notes
- A note surfaced for any target that merely contained its text, so a note on `run` appeared on `run_checks` and `src/runner.rs`. Targets now match exactly or by whole `::` or path segments.
- Only the newest 100 notes were ever searched, so older notes silently stopped appearing. There is no cap now.

### Added: `project_playbook`
- New MCP tool to call once at the start of work.
- It lists how to build, test, lint and format the project, and what CI actually runs (shell plumbing and release workflows are left out).
- It also lists toolchain versions (edition and MSRV, Node, Go, Python), format/lint configs, Makefile/justfile targets, and the agent instruction files to read first (`CLAUDE.md`, `AGENTS.md`, …).
- Each item is marked as detected or guessed. An ecosystem default that CI runs is marked as confirmed.

### Added: `changes_since`
- New MCP tool: what changed in the working tree (staged, unstaged, untracked) since a git ref, `HEAD` by default.
  - For each file: the touched symbols with their current line spans, the symbols added or removed since the ref (the old version is parsed with the same extractor), and only the changed lines.
  - Can be limited to paths, or to hunks that touch given symbols.
  - Errors on an invalid ref or outside a git repository.

### Added: `run_checks`
- New MCP tool. It runs the project's own test, lint, build or format command and returns a compact summary instead of the log: pass/fail counts, only the failing test names with their assertion lines and `file:line`, and compiler/linter errors with locations.
- Commands come from `Cargo.toml`, `package.json` scripts (npm, pnpm, yarn or bun), `go.mod` and pytest/ruff config, each marked as detected or guessed. They run without a shell and are stopped by a timeout.
- Recognized output: cargo/rustc/clippy/rustfmt, Jest, Vitest, pytest, `go test`.
- A failure is never hidden:
  - a failed run with nothing recognized becomes an **UNRECOGNIZED FAILURE** and includes the output tail;
  - exit code 0 with failures in the log is not reported as passed;
  - timeouts are marked PARTIAL.

### Changed: TypeScript / JavaScript extraction uses tree-sitter
- `.ts`, `.tsx` and `.js`/`.jsx`/`.mjs`/`.cjs` are parsed with the tree-sitter TypeScript, TSX and JavaScript grammars. Strings, template literals, regex literals, comments and JSX can no longer shift where a symbol ends.
- New: class fields holding arrow functions become methods, interface `extends` edges, dynamic `import("./x")`, `it.only`/`test.skip`, tests nested in `describe`. Calls inside strings and comments are no longer edges.
- An `it(`/`test(` call left unterminated by a syntax error is recorded as its first line only.

### Changed: tool answers state their coverage
- Lists that are cut to fit say so: `(showing 10 of 37 callers; 27 more not shown)`. `read_symbols_batch` names the queries beyond its 10-per-call limit instead of dropping them.
- Lookups distinguish **NOT INDEXED** (the file exists, but its type is not indexed or it is ignored) from **NOT FOUND**; symbol misses report the index's coverage and suggest `smart_grep`.
- `smart_grep` ends with a coverage line: files searched, matching lines in total vs shown, `file_filter` exclusions, unreadable files, and the fact that non-indexed files are not searched. Case folding is now Unicode-aware.
- `file_outline` names the extractor (`tree-sitter` or heuristic); `read_symbol` warns when spans come from a heuristic extractor.

## [0.6.0] - 2026-11-20

A reliability and safety release. There are no new MCP tools; everything that already existed now behaves correctly with several agents at once, on unusual file systems, and with configs the installer did not create.

### Fixed: your data and configs
- **Agent configs are never clobbered.** v0.5.0 replaced any config it could not parse (Cursor's `mcp.json` with comments, a broken `~/.claude/settings.json`, a project `.mcp.json`) with an empty object. The installer now stops and leaves the file untouched.
- **Backups.** Before its first change to a config, TrackerTop saves the original as `<file>.trackertop.bak`.
  - That first backup is never overwritten, even by concurrent installers.
  - It is never published half-written.
  - It is never more readable than the original.
  - A directory or other non-file at the backup path stops the installer.
- **Atomic, durable writes.** Configs and the project list are written to a temp file, flushed to disk and renamed into place. The rewrite keeps the file's permissions (a `chmod 600` config stays private) and keeps symlinks such as a config linked from a dotfiles repo. Temp files cannot be redirected by a planted symlink.
- **Project list (`~/.trackertop/projects.json`).**
  - Updates are serialized with an OS file lock, so concurrent sessions no longer lose projects.
  - A corrupted file is moved aside instead of being silently reset.
  - A temporary read error is no longer mistaken for corruption.
  - Network-share entries saved by v0.5.0 (`UNC\…`) are migrated.

### Fixed: indexing and the graph
- **Concurrent indexing could permanently lose graph edges.** A slower indexer could overwrite a newer edge set with one built from stale data. Edges are now written only if the files have not changed since they were resolved, and interrupted rebuilds are repaired on the next run.
- **"database is locked"** errors when the desktop app and several agents use the same project.
- **Edits missed by the fast path.** A file written while being indexed, or edited twice within the same second at the same size, could be skipped forever. Recently modified files are now re-checked (git's "racy file" rule).
- Files whose timestamp changed but whose content did not (`git checkout`, no-op saves) are no longer re-hashed on every run.
- The reported edge count matches the stored edges, and duplicate edges keep their strongest weight.

### Fixed: TypeScript / JavaScript extraction
- `it(...)` / `test(...)` blocks end where their call closes. One-line tests, Allman braces, comments, multi-line arguments and expression-bodied arrows no longer swallow the following functions or lose their body.
- Braces and parentheses inside strings, template literals, regex literals, comments and JSX closing tags are ignored when finding where a block ends.
- A comment above a test is attached to that test.

### Fixed: desktop app
- A crashed index worker no longer leaves the app busy forever.
- Saving a note and resetting stats now report failures.
- "Connected" status for an agent now requires a real MCP registration.

### Changed
- **Indexing is refused in your home folder, a drive root, or a bare Desktop/Downloads/Documents folder.** Without `--repo`, a globally installed MCP server indexes its working directory, so an agent started from `~` used to scan your whole profile. Projects inside those folders are unaffected.
- `.trackertop/` writes its own `.gitignore`, so `index.db` never ends up in your commits.
- **Faster MCP calls:** the built graph is cached between calls and invalidated when any process re-indexes. Measured on a 361-file, 126k-line repo: ~177 ms → ~55 ms per `find_symbol`.
- Minimum Rust version for building from source: **1.89**.

### Release & installers
- Every release asset ships with a `.sha256` file, and both installers verify it before installing.
- New **macOS Intel** (`macos-x64`) binary. `install.sh` picks the binary by CPU and fails clearly on unsupported platforms.
- `install.ps1` replaces the executable atomically and explains what to do if TrackerTop is still running.
- CI now enforces `cargo fmt`, `clippy -D warnings` (GUI and headless) and ShellCheck.

## [0.5.0] - 2026-11-19

Global Edition: TypeScript/JavaScript, Python and Go extractors, custom app icon, Claude Code SessionStart hook, macOS release CI, one-line installers.

[0.8.0]: https://github.com/trgtefecngz/TrackerTop/releases/tag/v0.8.0
[0.7.0]: https://github.com/trgtefecngz/TrackerTop/releases/tag/v0.7.0
[0.6.0]: https://github.com/trgtefecngz/TrackerTop/releases/tag/v0.6.0
[0.5.0]: https://github.com/trgtefecngz/TrackerTop/releases/tag/v0.5.0
