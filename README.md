# ⚡ TrackerTop

**AI-first codebase knowledge graph, surgical context packer, MCP server, and real token-savings tracker — built in Rust.**

TrackerTop replaces blind `grep` loops and full-file reads for AI coding agents (Claude Code, Cursor, Codex, etc.) with a deterministic, token-budgeted knowledge graph and tracks **exact BPE tokens saved** on every query.

---

## Why TrackerTop?

- **AI-First Surgical Context:** Instead of dumping 900-line files into an LLM context window, TrackerTop exposes `task_context`, `file_outline`, `read_symbol`, `call_graph`, and `impact_analysis` — strictly capped by `max_tokens`.
- **Real Token-Savings Telemetry:** Uses offline BPE tokenization (`cl100k_base`) to measure exact tokens sent vs. baseline full-file tokens avoided, logging every query to `.trackertop/index.db` with estimated USD savings.
- **Fast & Deterministic (100% Offline):** Built in Rust with Tree-sitter, `petgraph`, `blake3` incremental hashing, `rayon` parallelism, and bundled SQLite. Zero LLM cost for indexing.
- **Supported Languages:**
  - **Rust** (`.rs`) — Tree-sitter AST (`mod`, `use`, `struct`, `enum`, `trait`, `impl`, `fn`, `#[test]`, calls, type refs)
  - **HTML** (`.html`, `.htm`) — Tree-sitter AST (assets/links, `#id` elements, `<canvas>`, `<template>`, custom elements, inline `<script>` functions & shader fetches)
  - **WGSL** (`.wgsl`) — Dedicated shader parser (`@vertex`/`@fragment`/`@compute`, `@group/@binding` uniforms/storage, `struct`, `fn`, `#import`, calls)
  - **PowerShell** (`.ps1`, `.psm1`, `.psd1`) — Dedicated parser (`function`/`filter`, `class`, `enum`, `param(...)`, `Import-Module`, dot-sourcing, Verb-Noun calls)
  - **Docs & Config** (`.md`, `.toml`) — Section headings, backtick symbol cross-references, Cargo dependencies
- **Offline Interactive Visualization:** `trackertop report` generates a self-contained `.trackertop/report.html` (force-directed graph + cluster view + token-savings charts) and `.trackertop/GRAPH_REPORT.md`.

---

## Quick Start

### 1. Build
```bash
cargo build --release
```

### 2. Index a Repository
```bash
./target/release/trackertop index
```

### 3. Connect to Claude Code (One Command)
```bash
# Project-scoped (.mcp.json + .claude/skills/trackertop/SKILL.md + CLAUDE.md)
./target/release/trackertop install claude

# Or globally for all repositories:
./target/release/trackertop install claude --global
```

### 4. Inspect Token Savings & Interactive Graph
```bash
./target/release/trackertop stats
./target/release/trackertop report
# Open .trackertop/report.html in your browser!
```

---

## MCP Tools Exposed to AI Agents

| Tool | Purpose |
|---|---|
| `repo_map` | Compact architectural overview: languages, God Nodes (PageRank hubs), and module clusters |
| `task_context` | Flagship single-shot packer: exact code for core targets + 1-hop dependency signatures inside `max_tokens` |
| `file_outline` | Signatures and line ranges only (~85% fewer tokens than reading the whole file) |
| `read_symbol` | Exact source lines for one symbol + 1-hop caller/callee signatures |
| `find_symbol` | Hybrid lexical + PageRank search across all supported languages |
| `call_graph` | Directed N-hop callers and callees around any symbol |
| `impact_analysis` | Blast radius before refactoring: direct dependents, transitive dependents, and linked tests |
| `shortest_path` | Shortest dependency path connecting two symbols or files |
| `token_stats` | Real cumulative tokens saved and estimated USD cost reduction |

---

## License

MIT
