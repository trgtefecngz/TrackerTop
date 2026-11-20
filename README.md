# ⚡ TrackerTop

[![License: MIT](https://img.shields.io/badge/License-MIT-34d399.svg)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/Rust-100%25%20Offline-fb923c.svg)](Cargo.toml)
[![MCP Compatible](https://img.shields.io/badge/MCP-15%20AI%20Tools-38bdf8.svg)](src/mcp.rs)
[![Platforms](https://img.shields.io/badge/Platform-Windows%20%7C%20macOS%20%7C%20Linux-a78bfa.svg)](https://github.com/trgtefecngz/TrackerTop/releases/latest)

**AI-first codebase knowledge graph, surgical context packer, MCP server, offline native multi-project desktop app, and real token-savings tracker — built in Rust.**

TrackerTop replaces blind `grep` loops and full-file reads for **Claude Code, Cursor, Codex CLI, Windsurf, and any MCP client** with a deterministic, token-budgeted knowledge graph, tracks **exact BPE tokens saved** on every query, and ships with a **100% offline native desktop app** to monitor all your projects in one place.

---

## Real-World Token Savings Benchmark

Measured offline on a real **342-file, 443,561-token** multi-language game repository (Rust + WGSL + HTML + TOML + Markdown):

| Operation | Traditional Raw Reads | TrackerTop MCP | Savings | Latency |
|---|---:|---:|---:|---:|
| Architectural overview (`repo_map`) | 120,688 tok | **819 tok** | **99.3%** | **88 ms** |
| Git change blast radius (`diff_impact`) | 79,815 tok | **319 tok** | **99.6%** | **<40 ms** |
| Symbol-tagged search (`smart_grep`) | 11,690 tok | **135 tok** | **98.8%** | **<25 ms** |
| Surgical single-symbol read (`read_symbol`) | 16,456 tok | **1,296 tok** | **92.1%** | **<20 ms** |

---

## Quick Start

### Option A: One-Line Install (No Rust Required)

**Windows (PowerShell):**
```powershell
irm https://raw.githubusercontent.com/trgtefecngz/TrackerTop/main/scripts/install.ps1 | iex
```
*(Downloads the latest portable `.exe`, verifies its SHA256 checksum, adds it to User PATH, creates a Desktop shortcut, and launches TrackerTop.)*

**macOS / Linux:**
```bash
curl -fsSL https://raw.githubusercontent.com/trgtefecngz/TrackerTop/main/scripts/install.sh | bash
```
*(Detects Apple Silicon / Intel Mac / Linux x64, verifies the SHA256 checksum, and installs to `~/.local/bin`.)*

**Or download directly:** grab `TrackerTop-<version>-windows-x64.exe`, `TrackerTop-<version>-macos-arm64`, `TrackerTop-<version>-macos-x64` or `trackertop-<version>-linux-x64-headless` from [GitHub Releases](https://github.com/trgtefecngz/TrackerTop/releases/latest). Each binary has a matching `.sha256` file.

### Option B: Build from Source
```bash
cargo install --git https://github.com/trgtefecngz/TrackerTop
# Or locally:
cargo build --release
```
*(Headless CLI/MCP-only build: `cargo build --release --no-default-features`)*

### Connect Your AI Coding Agents (1 Click)
1. Open **TrackerTop** (double-click — zero console window, 100% offline).
2. Click **Connect AI Agents** in the top bar:
   - **Claude Code** (global MCP + `SKILL.md` + `CLAUDE.md` + `SessionStart` hook)
   - **Cursor**, **Codex CLI**, **Windsurf** (1-click config merge)
   - **Copy Universal MCP JSON** for VS Code Copilot, Zed, Cline, Antigravity, etc.
3. Click **+ Add** (or just open any folder in Claude Code — it auto-registers and updates live!).

---

## Supported Languages (9 Built-In + Open Core)

TrackerTop uses an open, pluggable **`LanguageRegistry`** (`LanguageExtractor` trait). Built-in extractors:

| Language | Extensions | Extracted Structures & Bridges |
|---|---|---|
| **Rust** | `.rs` | Tree-sitter AST (`mod`, `use`, `struct`, `enum`, `trait`, `impl`, `fn`, `#[test]`, calls, type refs, **Rust ↔ WGSL/HTML/TOML asset bridge**) |
| **TypeScript / JS** | `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs` | Imports/require, classes, methods, interfaces, types, enums, arrow functions, tests, calls |
| **Python** | `.py`, `.pyi` | Imports, classes & inheritance, functions/methods, `test_*`, docstrings, calls |
| **Go** | `.go` | Imports, structs, interfaces, receiver methods, `Test*`, calls |
| **HTML** | `.html`, `.htm` | Tree-sitter AST (assets/links, `#id`, `<canvas>`, `<template>`, custom elements, inline `<script>` & shader fetches) |
| **WGSL** | `.wgsl` | `@vertex`/`@fragment`/`@compute`, `@group/@binding` uniforms/storage, `struct`, `fn`, `#import`, calls |
| **PowerShell** | `.ps1`, `.psm1`, `.psd1` | `function`/`filter`/`class`/`enum`, `param(...)`, `Import-Module`, dot-sourcing, Verb-Noun calls |
| **Markdown & TOML** | `.md`, `.toml` | Section headings, backtick symbol cross-refs, Cargo dependencies |

Want to add **C/C++, C#, Java, Zig, GLSL, Ruby, or Swift**? Read [docs/ADDING_LANGUAGES.md](docs/ADDING_LANGUAGES.md) — 3 steps, zero changes to the graph, MCP server, or UI.

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

## License

MIT
