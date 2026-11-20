use crate::context::TrackerEngine;
use anyhow::Result;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::path::Path;

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Runs the TrackerTop Model Context Protocol (MCP) server over newline-delimited stdio JSON-RPC 2.0.
/// All protocol frames go to stdout; diagnostics must only ever go to stderr.
pub fn run_mcp_server(repo_root: &Path) -> Result<()> {
    let engine = TrackerEngine::new(repo_root);
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    for line_res in stdin.lock().lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let err_resp = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32700, "message": format!("Parse error: {e}") }
                });
                writeln!(out, "{err_resp}")?;
                out.flush()?;
                continue;
            }
        };

        if let Some(resp) = handle_jsonrpc_message(&engine, &req) {
            writeln!(out, "{resp}")?;
            out.flush()?;
        }
    }
    Ok(())
}

/// Pure handler exposed for integration tests as well as stdio loop.
pub fn handle_jsonrpc_message(engine: &TrackerEngine, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

    // Notifications (no `id`) must not receive a JSON-RPC response
    let id = id?;

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "trackertop",
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": "TrackerTop provides a token-budgeted deterministic codebase graph with smart_grep, batch symbol reads, git diff_impact, code health, and persistent AI memory. Prefer task_context, repo_map, smart_grep, file_outline, read_symbol, read_symbols_batch, impact_analysis, and diff_impact BEFORE running broad grep or reading full files to save 80-95% tokens."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => dispatch_tool_call(engine, &params),
        _ => Err((-32601, format!("Method not found: {method}"))),
    };

    match result {
        Ok(res) => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": res
        })),
        Err((code, msg)) => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": msg }
        })),
    }
}

fn dispatch_tool_call(
    engine: &TrackerEngine,
    params: &Value,
) -> std::result::Result<Value, (i32, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "Missing tool `name`".to_string()))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let max_tokens = args.get("max_tokens").and_then(Value::as_u64).unwrap_or(0) as usize;

    let text_res = match name {
        "repo_map" => engine.repo_map(max_tokens).map(|q| q.text),
        "file_outline" => {
            let path = args
                .get("path")
                .and_then(Value::as_str)
                .ok_or((-32602, "Missing required argument `path`".to_string()))?;
            engine.file_outline(path, max_tokens).map(|q| q.text)
        }
        "read_symbol" => {
            let symbol = args
                .get("symbol")
                .and_then(Value::as_str)
                .ok_or((-32602, "Missing required argument `symbol`".to_string()))?;
            let include_deps = args
                .get("include_deps")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            engine
                .read_symbol(symbol, include_deps, max_tokens)
                .map(|q| q.text)
        }
        "read_symbols_batch" => {
            let symbols: Vec<String> = args
                .get("symbols")
                .and_then(Value::as_array)
                .ok_or((-32602, "Missing required array argument `symbols`".to_string()))?
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect();
            let include_deps = args
                .get("include_deps")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            engine
                .read_symbols_batch(&symbols, include_deps, max_tokens)
                .map(|q| q.text)
        }
        "smart_grep" => {
            let pattern = args
                .get("pattern")
                .and_then(Value::as_str)
                .ok_or((-32602, "Missing required argument `pattern`".to_string()))?;
            let file_filter = args.get("file_filter").and_then(Value::as_str);
            engine
                .smart_grep(pattern, file_filter, max_tokens)
                .map(|q| q.text)
        }
        "find_symbol" => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .ok_or((-32602, "Missing required argument `query`".to_string()))?;
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(12) as usize;
            engine.find_symbol(query, limit, max_tokens).map(|q| q.text)
        }
        "call_graph" => {
            let symbol = args
                .get("symbol")
                .and_then(Value::as_str)
                .ok_or((-32602, "Missing required argument `symbol`".to_string()))?;
            let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(2) as usize;
            engine.call_graph(symbol, depth, max_tokens).map(|q| q.text)
        }
        "impact_analysis" => {
            let symbol = args
                .get("symbol")
                .and_then(Value::as_str)
                .ok_or((-32602, "Missing required argument `symbol`".to_string()))?;
            let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(2) as usize;
            engine
                .impact_analysis(symbol, depth, max_tokens)
                .map(|q| q.text)
        }
        "diff_impact" => engine.diff_impact(max_tokens).map(|q| q.text),
        "health_check" => engine.health_check(max_tokens).map(|q| q.text),
        "task_context" => {
            let task = args
                .get("task")
                .and_then(Value::as_str)
                .ok_or((-32602, "Missing required argument `task`".to_string()))?;
            engine.task_context(task, max_tokens).map(|q| q.text)
        }
        "shortest_path" => {
            let from = args
                .get("from")
                .and_then(Value::as_str)
                .ok_or((-32602, "Missing required argument `from`".to_string()))?;
            let to = args
                .get("to")
                .and_then(Value::as_str)
                .ok_or((-32602, "Missing required argument `to`".to_string()))?;
            engine.shortest_path(from, to, max_tokens).map(|q| q.text)
        }
        "remember" => {
            let target = args.get("target").and_then(Value::as_str).unwrap_or("project");
            let note = args
                .get("note")
                .and_then(Value::as_str)
                .ok_or((-32602, "Missing required argument `note`".to_string()))?;
            engine.remember(target, note)
        }
        "recall" => {
            let query = args.get("query").and_then(Value::as_str);
            engine.recall(query)
        }
        "run_checks" => {
            let kind = args.get("kind").and_then(Value::as_str).unwrap_or("test");
            let extra = args.get("extra_args").and_then(Value::as_str);
            let timeout = args
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(600);
            engine
                .run_checks(kind, extra, timeout, max_tokens)
                .map(|q| q.text)
        }
        "token_stats" => engine.token_stats().map(|s| {
            format!(
                "# TRACKERTOP TOKEN SAVINGS\nQueries: {}\nTokens sent: {}\nBaseline avoided: {}\nNet saved: {} tok ({:.1}%)\nEst. USD saved: ${:.4} (Sonnet) / ${:.4} (Opus)",
                s.total_queries,
                s.total_tokens_sent,
                s.total_baseline_tokens,
                s.total_tokens_saved,
                s.savings_percent,
                s.est_usd_saved_sonnet,
                s.est_usd_saved_opus
            )
        }),
        _ => return Err((-32602, format!("Unknown tool: {name}"))),
    };

    match text_res {
        Ok(text) => Ok(json!({
            "content": [{ "type": "text", "text": text }]
        })),
        Err(e) => Ok(json!({
            "isError": true,
            "content": [{ "type": "text", "text": format!("TrackerTop error: {e}") }]
        })),
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "repo_map",
            "description": "Start here in any unfamiliar repo. Returns a compact architectural map: language breakdown, top architectural hubs (God Nodes), module clusters, and saved project memory notes within a strict token budget.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 900)" }
                }
            }
        },
        {
            "name": "task_context",
            "description": "Best single-shot tool for starting a task or bugfix. Given a task or symbol query, packs exact surgical code for the top targets plus 1-hop dependency signatures strictly inside max_tokens.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "Feature, symbol, shader, or bug description/keywords" },
                    "max_tokens": { "type": "integer", "description": "Strict token budget (default 1800)" }
                },
                "required": ["task"]
            }
        },
        {
            "name": "smart_grep",
            "description": "Graph-aware text search (replaces raw grep). Finds string/error/binding matches across indexed files and tags every matching line with its enclosing function, struct, or shader symbol.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Substring to search for (case-insensitive)" },
                    "file_filter": { "type": "string", "description": "Optional path substring filter (e.g. 'shaders/' or '.rs')" },
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 1200)" }
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "file_outline",
            "description": "Returns only symbol signatures and line ranges for a file instead of reading the entire file (saves ~85% tokens).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative file path or filename suffix" },
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 800)" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "read_symbol",
            "description": "Surgically reads ONLY the exact lines of one function, struct, shader entry point, or PowerShell function plus 1-hop callee/caller signatures and attached memory notes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Symbol name, qualified name, or ID" },
                    "include_deps": { "type": "boolean", "description": "Include 1-hop callee/caller signatures (default true)" },
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 1400)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "read_symbols_batch",
            "description": "Reads 2-8 symbols in a single round-trip within a shared token budget (much faster and cheaper than multiple read_symbol calls).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbols": { "type": "array", "items": { "type": "string" }, "description": "List of symbol names to read" },
                    "include_deps": { "type": "boolean", "description": "Include 1-hop callee signatures (default false)" },
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 2200)" }
                },
                "required": ["symbols"]
            }
        },
        {
            "name": "find_symbol",
            "description": "Hybrid lexical + PageRank search across all registered languages.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Symbol name or keywords" },
                    "limit": { "type": "integer", "description": "Max results (default 12)" },
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 900)" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "call_graph",
            "description": "Shows callers and callees/types around a symbol up to N hops.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Symbol name or ID" },
                    "depth": { "type": "integer", "description": "Hops (1-4, default 2)" },
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 1000)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "impact_analysis",
            "description": "Blast radius before refactoring: shows direct dependents, transitive dependents, affected files, and linked tests to re-run.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Symbol you plan to change" },
                    "depth": { "type": "integer", "description": "Max hop depth (1-4, default 2)" },
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 1000)" }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "diff_impact",
            "description": "Git-powered blast radius for current uncommitted changes (or latest commit): lists dependent files, frequently co-changing files from git history, and linked tests to re-run.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 1200)" }
                }
            }
        },
        {
            "name": "health_check",
            "description": "Code health & AI-friendliness diagnostics: finds token-bloated files (>=3500 tok), circular dependency cycles, and potential orphan code files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 1000)" }
                }
            }
        },
        {
            "name": "shortest_path",
            "description": "Finds the shortest dependency/call path connecting two symbols or files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Source symbol or file" },
                    "to": { "type": "string", "description": "Target symbol or file" },
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 600)" }
                },
                "required": ["from", "to"]
            }
        },
        {
            "name": "remember",
            "description": "Saves a persistent architectural note attached to a symbol, file path, or 'project'. Automatically surfaced in future sessions when reading that target.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Symbol name, file path, or 'project'" },
                    "note": { "type": "string", "description": "Concise architectural rule, gotcha, or invariant" }
                },
                "required": ["note"]
            }
        },
        {
            "name": "recall",
            "description": "Recalls saved persistent architectural memory notes for this repository.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Optional filter keyword" }
                }
            }
        },
        {
            "name": "run_checks",
            "description": "Run the project's tests, lint, build or format check and get a compact summary instead of the full log: pass/fail counts, ONLY the failing test names with their assertion/error lines and file:line, and compiler/linter errors with locations. Use this instead of running `cargo test`, `npm test`, `pytest`, `go test`, clippy or eslint in a shell. Commands are detected from Cargo.toml, package.json scripts, go.mod and pytest/ruff config (marked detected vs guessed). It never hides a failure: a failed run whose output it cannot parse is reported as UNRECOGNIZED FAILURE with the output tail, and timeouts are marked PARTIAL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "description": "test (default), lint, build, format, or all (format + lint + test)" },
                    "extra_args": { "type": "string", "description": "Extra arguments for the single matching command, e.g. a test name filter `parser::` or `-p mycrate` (split on whitespace, no shell)" },
                    "timeout_secs": { "type": "integer", "description": "Kill the command after this many seconds (default 600, max 3600)" },
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 1500); failing names are listed before details" }
                }
            }
        },
        {
            "name": "token_stats",
            "description": "Returns cumulative real token savings and estimated USD cost savings tracked by TrackerTop in this repository.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }
    ])
}
