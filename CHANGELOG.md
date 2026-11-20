# Changelog

All notable changes to TrackerTop are documented here.

## [Unreleased]

v0.7 is the agent-productivity release (see `docs/ROADMAP.md`).

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

[0.6.0]: https://github.com/trgtefecngz/TrackerTop/releases/tag/v0.6.0
[0.5.0]: https://github.com/trgtefecngz/TrackerTop/releases/tag/v0.5.0
