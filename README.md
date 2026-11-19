# ⚡ TrackerTop

**AI-first codebase knowledge graph, surgical context packer, MCP server, offline native multi-project desktop app, and real token-savings tracker — built in Rust.**

TrackerTop replaces blind `grep` loops and full-file reads for AI coding agents (Claude Code, Cursor, Codex, etc.) with a deterministic, token-budgeted knowledge graph, tracks **exact BPE tokens saved** on every query, and ships with a **100% offline native desktop app** to monitor all your projects in one place.

---

## Key Features

- **AI-First Surgical Context:** Exposes 9 MCP tools (`repo_map`, `task_context`, `file_outline`, `read_symbol`, `find_symbol`, `call_graph`, `impact_analysis`, `shortest_path`, `token_stats`) strictly bounded by `max_tokens`.
- **Automatic On-Demand Incremental Indexing:** Zero background CPU drain. On every AI query, TrackerTop checks file mtimes/hashes (`blake3`) in ~5–10 ms and re-parses only modified files — so AI agents always see 100% fresh code.
- **Real Token-Savings Telemetry:** Uses offline BPE tokenization (`cl100k_base`) to measure exact tokens sent vs. baseline full-file tokens avoided, logging every query with estimated USD savings (Sonnet / Opus).
- **Offline Native Multi-Project Desktop App:** Double-click `trackertop.exe` (or run `trackertop ui`) to open the native GPU-rendered desktop app:
  - Track and switch between **multiple projects** (any repo used by Claude Code auto-registers!)
  - **All Projects** global savings overview + live per-project token-savings feed
  - Interactive force-directed **Code Graph** with symbol inspector
  - One-click **Connect Claude Code (Global)** setup
- **Open, Pluggable Language Core (`LanguageRegistry`):** Built-in extractors for **Rust**, **HTML**, **WGSL**, **PowerShell**, **Markdown**, and **TOML** — and designed so contributors can add any new language in 3 steps without touching the graph, MCP, or UI. See [docs/ADDING_LANGUAGES.md](docs/ADDING_LANGUAGES.md).

---

## Quick Start

### Option A: Download Portable Windows App (No Rust Required)
1. Download **`TrackerTop-v0.4.1-windows-x64.exe`** from [GitHub Releases](https://github.com/trgtefecngz/TrackerTop/releases/latest).
2. Double-click to open the offline native desktop app (no console window, no installation needed).
3. Click **Connect AI Agents** (Claude Code, Cursor, Codex CLI, Windsurf, or copy universal MCP JSON) and **+ Add**.

### Option B: Build from Source
```bash
cargo build --release
```
*(For headless CLI/MCP servers without GUI dependencies: `cargo build --release --no-default-features`)*

### CLI Usage (Optional)
```bash
# Connect Claude Code globally across all repositories
./target/release/trackertop install claude --global

# Index current repo
./target/release/trackertop index

# Inspect real token savings
./target/release/trackertop stats
./target/release/trackertop projects

# Export concise Markdown summary (.trackertop/GRAPH_REPORT.md)
./target/release/trackertop report
```

---

## MCP Tools Exposed to AI Agents (15 Tools)

| Tool | Purpose |
|---|---|
| `repo_map` | Compact architectural overview: languages, God Nodes, module clusters, and saved project notes |
| `task_context` | Flagship single-shot packer: exact code for core targets + 1-hop dependency signatures inside `max_tokens` |
| `smart_grep` | Graph-aware text search (replaces raw grep) — tags every match with its enclosing function/struct/shader |
| `file_outline` | Signatures and line ranges only (~85% fewer tokens than reading the whole file) |
| `read_symbol` | Exact source lines for one symbol + 1-hop caller/callee signatures + attached AI memory notes |
| `read_symbols_batch` | Reads 2–8 symbols in a single round-trip within a shared token budget |
| `find_symbol` | Hybrid lexical + PageRank search across all registered languages |
| `call_graph` | Directed N-hop callers and callees around any symbol |
| `impact_analysis` | Blast radius before refactoring: direct dependents, transitive dependents, and linked tests |
| `diff_impact` | Git-powered blast radius for uncommitted / latest changes + git co-changing files + tests |
| `health_check` | Token-bloat warnings (>=3.5K tok), circular dependency cycles, and potential orphan code files |
| `shortest_path` | Shortest dependency path connecting two symbols or files |
| `remember` / `recall` | Persistent AI architectural memory auto-injected into future sessions |
| `token_stats` | Real cumulative tokens saved and estimated USD cost reduction |

---

## Contributing a Language

TrackerTop's parser core is completely open via `LanguageExtractor` and `LanguageRegistry`.
Read [docs/ADDING_LANGUAGES.md](docs/ADDING_LANGUAGES.md) to add Python, TypeScript, Go, C++, GLSL, Zig, or any other language in minutes.

---

## License

MIT
