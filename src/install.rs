use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

const SKILL_MD: &str = r#"---
name: trackertop
description: Use TrackerTop MCP tools to navigate the codebase via deterministic knowledge graph and surgical symbol slicing before grepping or reading whole files.
---

# TrackerTop AI-First Navigation Protocol

This repository is indexed by **TrackerTop** (Rust, HTML, WGSL, PowerShell, Docs).
To save 80-95% context tokens and avoid slow blind file scans, follow this order:

1. **Orient first:** Call `repo_map` once when starting work to see module clusters and architectural hubs (God Nodes).
2. **Start any task in 1 shot:** Call `task_context(task="...", max_tokens=1800)` instead of running multiple `grep` + `Read` calls.
3. **Never read whole large files blindly:** Call `file_outline(path="...")` to inspect signatures and line ranges, then `read_symbol(symbol="...")` to pull ONLY the exact function/struct/shader you need.
4. **Before refactoring:** Call `impact_analysis(symbol="...")` to see direct dependents, transitive blast radius, and linked tests to re-run.
5. **Check savings:** Call `token_stats` anytime to report exact tokens saved.
"#;

const CLAUDE_MD_SECTION: &str = r#"
## TrackerTop MCP Navigation (Token Optimization)

This project has **TrackerTop** configured as an MCP server (`trackertop`).
- Prefer `task_context`, `repo_map`, `file_outline`, `read_symbol`, and `impact_analysis` **before** running broad grep searches or reading full source files.
- Every TrackerTop call is strictly token-budgeted and logs real token savings (`trackertop stats`).
"#;

/// Checks whether TrackerTop appears registered in user-level Claude Code configuration.
pub fn is_claude_global_installed() -> bool {
    let Some(home) = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
    else {
        return false;
    };
    let candidates = [
        home.join(".claude.json"),
        home.join(".claude").join("settings.json"),
    ];
    for path in candidates {
        if let Ok(content) = std::fs::read_to_string(path) {
            if content.contains("\"trackertop\"") {
                return true;
            }
        }
    }
    false
}

/// Installs TrackerTop integration for Claude Code:
/// 1. Registers MCP server (project `.mcp.json` by default, or user-level via `claude mcp add --scope user`)
/// 2. Creates `.claude/skills/trackertop/SKILL.md`
/// 3. Adds concise navigation rules to `CLAUDE.md` (idempotent)
pub fn install_claude_code(repo_root: &Path, global: bool) -> Result<Vec<String>> {
    let exe_path = std::env::current_exe()
        .context("failed to determine current trackertop executable path")?;
    let exe_str = exe_path.to_string_lossy().to_string();
    let repo_str = repo_root.to_string_lossy().to_string();
    let mut actions = Vec::new();

    if global {
        // Attempt `claude mcp add --transport stdio --scope user trackertop -- <exe> mcp`
        let mut cmd = Command::new("claude");
        cmd.args([
            "mcp",
            "add",
            "--transport",
            "stdio",
            "--scope",
            "user",
            "trackertop",
            "--",
            &exe_str,
            "mcp",
        ]);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let status = cmd.status();
        match status {
            Ok(s) if s.success() => {
                actions.push(format!("Registered global Claude Code MCP server: `{exe_str} mcp`"));
            }
            _ => {
                actions.push(format!(
                    "Could not invoke `claude mcp add` automatically. Run manually:\n  claude mcp add --transport stdio --scope user trackertop -- \"{exe_str}\" mcp"
                ));
            }
        }
    } else {
        // Write or merge project-level `.mcp.json`
        let mcp_json_path = repo_root.join(".mcp.json");
        let mut root_val: Value = if mcp_json_path.exists() {
            let existing = std::fs::read_to_string(&mcp_json_path).unwrap_or_default();
            serde_json::from_str(&existing).unwrap_or_else(|_| json!({}))
        } else {
            json!({})
        };

        if !root_val.is_object() {
            root_val = json!({});
        }
        let servers = root_val
            .as_object_mut()
            .unwrap()
            .entry("mcpServers")
            .or_insert_with(|| json!({}));
        if let Some(obj) = servers.as_object_mut() {
            obj.insert(
                "trackertop".to_string(),
                json!({
                    "command": exe_str,
                    "args": ["mcp", "--repo", repo_str]
                }),
            );
        }
        std::fs::write(&mcp_json_path, serde_json::to_string_pretty(&root_val)?)?;
        actions.push(format!("Wrote project MCP config: {}", mcp_json_path.display()));
    }

    // Write skill + CLAUDE.md in global `~/.claude/` when `global` is true, otherwise project-local
    let (skill_dir, claude_md) = if global {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| repo_root.to_path_buf());
        let claude_home = home.join(".claude");
        (
            claude_home.join("skills").join("trackertop"),
            claude_home.join("CLAUDE.md"),
        )
    } else {
        (
            repo_root.join(".claude").join("skills").join("trackertop"),
            repo_root.join("CLAUDE.md"),
        )
    };

    std::fs::create_dir_all(&skill_dir)?;
    let skill_path = skill_dir.join("SKILL.md");
    std::fs::write(&skill_path, SKILL_MD)?;
    actions.push(format!("Wrote Claude Code skill: {}", skill_path.display()));

    if let Some(parent) = claude_md.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing_md = std::fs::read_to_string(&claude_md).unwrap_or_default();
    if !existing_md.contains("TrackerTop MCP Navigation") {
        let mut updated = existing_md;
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(CLAUDE_MD_SECTION);
        std::fs::write(&claude_md, updated)?;
        actions.push(format!("Updated {}", claude_md.display()));
    } else {
        actions.push(format!("{} already has TrackerTop instructions (kept as-is)", claude_md.display()));
    }

    Ok(actions)
}
