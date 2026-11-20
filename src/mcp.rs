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
    engine.set_source("mcp:unknown"); // refined by the client name on `initialize`
    // One server process per agent session: its start time + pid name the session in the
    // work journal (it survives `/compact`, which keeps the same process)
    engine.set_session(&format!(
        "{}#{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        std::process::id()
    ));
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

/// Records a `tools/call` in the work journal: tool, compacted arguments and outcome.
/// Best effort: a journal failure never changes the response.
fn journal_tool_call(
    engine: &TrackerEngine,
    params: &Value,
    res: &std::result::Result<Value, (i32, String)>,
) {
    let tool = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("(missing)");
    let query = crate::journal::compact_query(params.get("arguments").unwrap_or(&Value::Null));
    let text = |v: &Value| {
        v.pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let outcome = match res {
        Ok(v) if v.get("isError").and_then(Value::as_bool) == Some(true) => {
            let t = text(v);
            crate::journal::outcome_of(tool, Err(t.trim_start_matches("TrackerTop error: ")))
        }
        Ok(v) => crate::journal::outcome_of(tool, Ok(&text(v))),
        Err((_, msg)) => crate::journal::outcome_of(tool, Err(msg)),
    };
    let _ = engine.journal_call(tool, &query, &outcome);
}

/// Pure handler exposed for integration tests as well as stdio loop.
pub fn handle_jsonrpc_message(engine: &TrackerEngine, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

    // Notifications (no `id`) must not receive a JSON-RPC response
    let id = id?;

    if method == "initialize" {
        // Tag telemetry with the calling agent so real usage is separable from CLI/tests
        let client: String = params
            .pointer("/clientInfo/name")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '))
            .take(40)
            .collect();
        let client = client.trim();
        engine.set_source(&format!(
            "mcp:{}",
            if client.is_empty() { "unknown" } else { client }
        ));
    }

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
            "instructions": "TrackerTop provides a token-budgeted deterministic codebase graph with smart_grep, batch symbol reads, git diff_impact, code health, and persistent AI memory. Prefer task_context, repo_map, smart_grep, file_outline, read_symbol, read_symbols_batch, impact_analysis, and diff_impact BEFORE running broad grep or reading full files to save 80-95% tokens. Call project_playbook once at the start, run_checks instead of running tests/lint in a shell, changes_since to catch up after edits, review_changes before committing or pushing, and remember(kind=decision|dismissed, reason=...) to record settled questions. Starting or resuming work (a new session, after /compact)? Call resume first: what earlier sessions did, what failed or is blocked, open findings, decisions and uncommitted work. Record milestones and dead ends with remember(kind=progress|tried|blocked, reason=...) so the next session sees them. Every answer states its coverage (truncation, NOT INDEXED vs NOT FOUND)."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let res = dispatch_tool_call(engine, &params);
            journal_tool_call(engine, &params, &res);
            res
        }
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
                .ok_or((
                    -32602,
                    "Missing required array argument `symbols`".to_string(),
                ))?
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
            let target = args
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or("project");
            let note = args
                .get("note")
                .and_then(Value::as_str)
                .ok_or((-32602, "Missing required argument `note`".to_string()))?;
            let kind = args.get("kind").and_then(Value::as_str).unwrap_or("note");
            let reason = args.get("reason").and_then(Value::as_str).unwrap_or("");
            engine.remember_kind(target, note, kind, reason)
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
        "changes_since" => {
            let since = args.get("since").and_then(Value::as_str).unwrap_or("HEAD");
            let list = |key: &str| -> Vec<String> {
                args.get(key)
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            engine
                .changes_since(since, &list("paths"), &list("symbols"), max_tokens)
                .map(|q| q.text)
        }
        "review_changes" => {
            let base = args.get("base").and_then(Value::as_str).unwrap_or("");
            engine.review_changes(base, max_tokens).map(|q| q.text)
        }
        "project_playbook" => engine.project_playbook(max_tokens).map(|q| q.text),
        "resume" => engine.resume(max_tokens).map(|q| q.text),
        "token_stats" => engine.token_stats().map(|s| s.summary_text(5)),
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
                    "symbol": { "type": "string", "description": "Symbol name, qualified name, ID or `path::name`; or a line range `path:Lstart-end` to read lines no symbol covers (an impl tail, test helpers)" },
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
            "description": "Code health & AI-friendliness diagnostics: oversized files (size outliers of this repo, largest first), circular dependency cycles through code (Rust module cycles are allowed), and orphan code files (entry points and scripts run directly are listed apart). States what it covered.",
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
            "description": "Saves a persistent note attached to a symbol, file path, or 'project': an architectural rule (kind=note), a decision and why it was made (kind=decision), or a review lead / suspected bug that was checked and rejected (kind=dismissed). Work-journal entries for the next session: a milestone reached (kind=progress), an approach that failed and why (kind=tried), or something blocked and on what (kind=blocked; it stays open until a later progress entry on the same target, or one mentioning its #id, closes it). Notes are shown whenever that symbol or file is read in later sessions (read_symbol, read_symbols_batch, file_outline) and by `resume`, so settled questions are not re-investigated. Also kept as Markdown in .trackertop/notes/.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Symbol name, qualified name, file path, or 'project'" },
                    "note": { "type": "string", "description": "Concise rule, gotcha, decision, dismissed lead, milestone, failed approach or blocker" },
                    "kind": { "type": "string", "enum": ["note", "decision", "dismissed", "progress", "tried", "blocked"], "description": "note (default), decision, dismissed, progress, tried or blocked" },
                    "reason": { "type": "string", "description": "Why (required for decision, dismissed, tried and blocked)" }
                },
                "required": ["note"]
            }
        },
        {
            "name": "resume",
            "description": "Where work stands, in one budgeted answer: call it first in a new session or after /compact instead of rediscovering. Lists uncommitted work (files and touched symbols with lines), the progress file's `## Next` and `## Kararını bekleyenler` sections (newest docs/*PROGRESS*.md), open blocked entries, approaches tried and failed, open review findings from the last review_changes, decisions (POSSIBLY OUTDATED ones first), the progress log, and what recent agent sessions did (tools, focus, check results, errors). Every item is dated; capped lists say how many items are hidden and how to see them.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 3000)" }
                }
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
            "name": "changes_since",
            "description": "What changed since a git ref (default HEAD = your uncommitted work), including staged, unstaged and untracked files, mapped onto symbols. For each changed file it shows the touched symbols with current line spans, symbols added/removed since the ref, and only the changed lines. Use it to catch up after edits (yours, the user's, a merge or a checkout) instead of re-reading whole files; pass `symbols` to see only hunks touching those symbols. Invalid refs and non-git directories are errors.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "since": { "type": "string", "description": "Git ref to compare against: HEAD (default), HEAD~3, a branch, tag or commit" },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Optional repo-relative files or directories to limit the diff to" },
                    "symbols": { "type": "array", "items": { "type": "string" }, "description": "Optional symbols: show only hunks that touch them" },
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 2000)" }
                }
            }
        },
        {
            "name": "review_changes",
            "description": "Review your branch and uncommitted work (staged, unstaged, untracked) against its base branch before committing or pushing, offline in seconds. Lists findings first, each with a stable id, severity, file:line and evidence: broken-ref (a removed symbol that a surviving caller still calls, verified in the working tree), orphan (a function whose last caller the change removed, with no mention left in the code), impact (a modified function with many code dependents, plus the tests that reach it), untested (new or modified code no test reaches), new-cycle (an import cycle the change created, JS/TS/Python/Go), cochange-miss (a file that habitually changes with one you touched but not this time; a habit, not proof) and bloat. Then what was resolved / new / unchanged since your previous review against the same base, then every added/removed/modified symbol with file:line at base and head. After checking a finding and deciding it is not a problem, call remember(target=<finding id>, kind=\"dismissed\", reason=...) and it is not raised again while its code is unchanged.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "base": { "type": "string", "description": "Branch, tag or commit to compare against (default: origin/HEAD, main or master); the merge base with HEAD is used" },
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 2500); findings come before the symbol list" }
                }
            }
        },
        {
            "name": "project_playbook",
            "description": "Call once at the start of work in a repo: how to build, test, lint and format it, and what CI runs (the bar a change must pass), the toolchain versions, format/lint configs, task-runner targets and agent instruction files to read first (CLAUDE.md, AGENTS.md, ...). Every item is marked detected (declared by the project) or guessed (ecosystem default). Saves opening Cargo.toml, package.json, workflows and configs one by one.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "max_tokens": { "type": "integer", "description": "Token budget cap (default 1200)" }
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
