use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

const SKILL_MD: &str = r#"---
name: trackertop
description: Use TrackerTop MCP tools to navigate the codebase via deterministic knowledge graph, smart_grep, batch symbol reads, git diff_impact, and persistent memory before broad grep or full-file reads.
---

# TrackerTop AI-First Navigation Protocol

This repository is indexed by **TrackerTop** (open pluggable language core).
To save 80-95% context tokens and keep answers accurate, follow this order:

1. **Orient first:** Call `repo_map` once when starting work to see module clusters, God Nodes, and saved project notes.
2. **Start any task in 1 shot:** Call `task_context(task="...", max_tokens=1800)` instead of running multiple `grep` + `Read` calls.
3. **Never read whole large files blindly:** Call `file_outline(path="...")`, `read_symbol(symbol="...")`, or `read_symbols_batch(symbols=[...])` for multi-symbol inspection in a single round-trip.
4. **Search strings with graph awareness:** Call `smart_grep(pattern="...")` instead of raw `grep` — every match is tagged with its enclosing function/struct/shader.
5. **Before/after editing:** Call `impact_analysis(symbol="...")` or `diff_impact` to see direct dependents, git co-changing files, and linked tests.
6. **Know the bar:** Call `project_playbook` once to learn the build/test/lint commands and what CI runs.
7. **Verify cheaply:** Call `run_checks(kind="test"|"lint"|"all")` instead of running tests or linters in a shell: you get only the failing tests and errors with file:line.
8. **Catch up after changes:** Call `changes_since(since="HEAD")` to see what changed (by symbol) instead of re-reading files.
9. **Persist discoveries:** Call `remember(target="...", note="...")` for rules, and `remember(..., kind="decision"|"dismissed", reason="...")` for decisions and rejected review leads, so later sessions do not re-investigate them.
10. **Trust the coverage lines:** answers say when lists are cut, when a file is NOT INDEXED (read it directly) vs NOT FOUND, and when spans come from a heuristic extractor.
"#;

const CLAUDE_MD_SECTION: &str = r#"
## TrackerTop MCP Navigation (Token Optimization)

This project has **TrackerTop** configured as an MCP server (`trackertop`).
- Prefer `task_context`, `repo_map`, `smart_grep`, `file_outline`, `read_symbol`, `read_symbols_batch`, `impact_analysis`, `diff_impact`, and `remember`/`recall` **before** raw grep or full-file reads.
- Use `project_playbook` to learn the build/test commands, `run_checks` instead of running tests/lint in a shell, and `changes_since` to catch up after edits.
- Every TrackerTop call is strictly token-budgeted and logs real token savings (`trackertop stats`).
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiAgentKind {
    ClaudeCode,
    Cursor,
    Codex,
    Windsurf,
}

impl AiAgentKind {
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Cursor => "Cursor",
            Self::Codex => "Codex CLI",
            Self::Windsurf => "Windsurf",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "Global MCP + Skill + CLAUDE.md protocol",
            Self::Cursor => "Merges into ~/.cursor/mcp.json",
            Self::Codex => "Appends [mcp_servers.trackertop] to ~/.codex/config.toml",
            Self::Windsurf => "Merges into ~/.codeium/windsurf/mcp_config.json",
        }
    }

    pub fn all() -> [Self; 4] {
        [Self::ClaudeCode, Self::Cursor, Self::Codex, Self::Windsurf]
    }
}

fn user_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

pub fn current_exe_string() -> Result<String> {
    let exe_path = std::env::current_exe()
        .context("failed to determine current trackertop executable path")?;
    Ok(exe_path.to_string_lossy().to_string())
}

/// Returns a clean universal MCP JSON snippet users can paste into any MCP client (VS Code, Zed, Cline, etc.).
pub fn generic_mcp_json_snippet() -> String {
    let exe = current_exe_string().unwrap_or_else(|_| "trackertop".to_string());
    let val = json!({
        "mcpServers": {
            "trackertop": {
                "command": exe,
                "args": ["mcp"]
            }
        }
    });
    serde_json::to_string_pretty(&val).unwrap_or_default()
}

/// True only when the agent's config actually registers a `trackertop` MCP server.
///
/// A plain substring search gave false positives: the Claude SessionStart hook in
/// `~/.claude/settings.json` contains the TrackerTop path even when `claude mcp add` failed.
pub fn is_agent_installed(agent: AiAgentKind) -> bool {
    let Some(home) = user_home() else {
        return false;
    };
    let path = match agent {
        // `claude mcp add --scope user` stores servers under top-level `mcpServers`
        AiAgentKind::ClaudeCode => home.join(".claude.json"),
        AiAgentKind::Cursor => home.join(".cursor").join("mcp.json"),
        AiAgentKind::Codex => home.join(".codex").join("config.toml"),
        AiAgentKind::Windsurf => home
            .join(".codeium")
            .join("windsurf")
            .join("mcp_config.json"),
    };
    let Ok(Some((content, _))) = crate::fsutil::read_text_file(&path) else {
        return false;
    };
    match agent {
        AiAgentKind::Codex => toml_registers_trackertop(&content),
        _ => json_registers_trackertop(&content),
    }
}

fn json_registers_trackertop(content: &str) -> bool {
    serde_json::from_str::<Value>(content.trim_start_matches('\u{feff}'))
        .ok()
        .and_then(|v| v.get("mcpServers")?.get("trackertop").cloned())
        .is_some_and(|s| s.is_object())
}

fn toml_registers_trackertop(content: &str) -> bool {
    content
        .lines()
        .any(|l| l.trim() == "[mcp_servers.trackertop]")
}

/// Checks whether TrackerTop appears registered in user-level Claude Code configuration.
pub fn is_claude_global_installed() -> bool {
    is_agent_installed(AiAgentKind::ClaudeCode)
}

pub fn install_agent(agent: AiAgentKind, repo_root: &Path) -> Result<Vec<String>> {
    match agent {
        AiAgentKind::ClaudeCode => install_claude_code(repo_root, true),
        AiAgentKind::Cursor => {
            let home = user_home().context("cannot determine home directory")?;
            let path = home.join(".cursor").join("mcp.json");
            merge_json_mcp_config(&path)?;
            Ok(vec![format!("Connected Cursor: {}", path.display())])
        }
        AiAgentKind::Windsurf => {
            let home = user_home().context("cannot determine home directory")?;
            let path = home
                .join(".codeium")
                .join("windsurf")
                .join("mcp_config.json");
            merge_json_mcp_config(&path)?;
            Ok(vec![format!("Connected Windsurf: {}", path.display())])
        }
        AiAgentKind::Codex => {
            let home = user_home().context("cannot determine home directory")?;
            let path = home.join(".codex").join("config.toml");
            add_codex_server(&path, &current_exe_string()?)?;
            Ok(vec![format!("Connected Codex CLI: {}", path.display())])
        }
    }
}

/// Adds the `[mcp_servers.trackertop]` table to Codex's `config.toml` unless it is there.
/// The existing file is backed up, keeps its encoding, and is never written when it could
/// not be read.
fn add_codex_server(path: &Path, exe: &str) -> Result<()> {
    let exe = exe.replace('\\', "\\\\").replace('"', "\\\"");
    let existing = crate::fsutil::read_text_file(path)?.map_or_else(String::new, |(t, _)| t);
    if existing.contains("[mcp_servers.trackertop]") {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!(
        "\n[mcp_servers.trackertop]\ncommand = \"{exe}\"\nargs = [\"mcp\"]\n"
    ));
    crate::fsutil::rewrite_text_file(path, &updated)
}

/// Appends TrackerTop's navigation section to a `CLAUDE.md` unless it is already there.
/// Returns whether the file changed. The existing file is backed up, keeps its encoding
/// (Windows PowerShell 5.1 writes UTF-16), and is never written when it could not be read.
fn add_claude_md_section(path: &Path) -> Result<bool> {
    let existing = crate::fsutil::read_text_file(path)?.map_or_else(String::new, |(t, _)| t);
    if existing.contains("TrackerTop MCP Navigation") {
        return Ok(false);
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(CLAUDE_MD_SECTION);
    crate::fsutil::rewrite_text_file(path, &updated)?;
    Ok(true)
}

/// Reads a user-owned JSON config file as an object.
/// A missing or empty file yields `{}`. A file that cannot be parsed (e.g. JSONC with
/// comments or trailing commas) or whose root is not an object is an ERROR: TrackerTop must
/// never replace a user's configuration it does not understand.
pub(crate) fn read_json_config(path: &Path) -> Result<Value> {
    let Some((text, _)) = crate::fsutil::read_text_file(path)? else {
        return Ok(json!({}));
    };
    let body = text.trim_start_matches('\u{feff}').trim();
    if body.is_empty() {
        return Ok(json!({}));
    }
    let val: Value = serde_json::from_str(body).with_context(|| {
        format!(
            "{} is not plain JSON (comments or trailing commas?). TrackerTop left it untouched; \
             fix the file or add the `trackertop` MCP entry manually",
            path.display()
        )
    })?;
    if !val.is_object() {
        bail!(
            "{} does not contain a JSON object at its root. TrackerTop left it untouched",
            path.display()
        );
    }
    Ok(val)
}

/// Backs up the existing file (`<name>.trackertop.bak`) and writes `val` atomically, in the
/// file's existing encoding.
pub(crate) fn write_json_config(path: &Path, val: &Value) -> Result<()> {
    crate::fsutil::rewrite_text_file(path, &serde_json::to_string_pretty(val)?)
}

fn merge_json_mcp_config(config_path: &Path) -> Result<()> {
    let exe_str = current_exe_string()?;
    let mut root_val = read_json_config(config_path)?;
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
                "args": ["mcp"]
            }),
        );
    } else {
        bail!(
            "`mcpServers` in {} is not an object. TrackerTop left it untouched",
            config_path.display()
        );
    }
    write_json_config(config_path, &root_val)
}

/// Installs TrackerTop integration for Claude Code:
/// 1. Registers MCP server (project `.mcp.json` by default, or user-level via `claude mcp add --scope user`)
/// 2. Creates `SKILL.md`
/// 3. Adds concise navigation rules to `CLAUDE.md` (idempotent)
pub fn install_claude_code(repo_root: &Path, global: bool) -> Result<Vec<String>> {
    let exe_str = current_exe_string()?;
    let repo_str = repo_root.to_string_lossy().to_string();
    let mut actions = Vec::new();

    if global {
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
                actions.push(format!(
                    "Registered global Claude Code MCP: `{exe_str} mcp`"
                ));
            }
            _ => {
                actions.push(format!(
                    "Could not run `claude mcp add`. Run manually:\n  claude mcp add --transport stdio --scope user trackertop -- \"{exe_str}\" mcp"
                ));
            }
        }
    } else {
        let mcp_json_path = repo_root.join(".mcp.json");
        let mut root_val = read_json_config(&mcp_json_path)?;
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
        } else {
            bail!(
                "`mcpServers` in {} is not an object. TrackerTop left it untouched",
                mcp_json_path.display()
            );
        }
        write_json_config(&mcp_json_path, &root_val)?;
        actions.push(format!(
            "Wrote project MCP config: {}",
            mcp_json_path.display()
        ));
    }

    let (skill_dir, claude_md) = if global {
        let home = user_home().unwrap_or_else(|| repo_root.to_path_buf());
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
    actions.push(format!("Updated skill: {}", skill_path.display()));

    if let Some(parent) = claude_md.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if add_claude_md_section(&claude_md)? {
        actions.push(format!("Updated {}", claude_md.display()));
    } else {
        actions.push(format!("{} already configured", claude_md.display()));
    }

    // Install lightweight SessionStart hook in `~/.claude/settings.json` when global
    if global && let Some(home) = user_home() {
        let settings_path = home.join(".claude").join("settings.json");
        match install_claude_session_hook(&settings_path, &exe_str) {
            Ok(()) => actions.push(format!(
                "Installed Claude Code SessionStart hook in {}",
                settings_path.display()
            )),
            Err(e) => actions.push(format!("Skipped SessionStart hook: {e:#}")),
        }
    }

    Ok(actions)
}

/// Safely merges a lightweight `SessionStart` hook into `~/.claude/settings.json`
/// without overwriting any existing user settings or hooks.
pub(crate) fn install_claude_session_hook(settings_path: &Path, exe_str: &str) -> Result<()> {
    let mut root = read_json_config(settings_path)?;
    let hook_cmd = format!("\"{exe_str}\" hook-hint");
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if let Some(hooks_obj) = hooks.as_object_mut() {
        let session_start = hooks_obj.entry("SessionStart").or_insert_with(|| json!([]));
        if let Some(arr) = session_start.as_array_mut() {
            let already = arr
                .iter()
                .any(|entry| entry.to_string().contains("hook-hint"));
            if !already {
                arr.push(json!({
                    "hooks": [
                        {
                            "type": "command",
                            "command": hook_cmd
                        }
                    ]
                }));
            } else {
                // Already installed: leave the user's file byte-for-byte untouched.
                return Ok(());
            }
        } else {
            bail!(
                "`hooks.SessionStart` in {} is not an array",
                settings_path.display()
            );
        }
    } else {
        bail!("`hooks` in {} is not an object", settings_path.display());
    }
    write_json_config(settings_path, &root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_json_config_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let original = "{\n  // user comment\n  \"theme\": \"dark\",\n}\n";
        std::fs::write(&p, original).unwrap();

        assert!(install_claude_session_hook(&p, "C:/tt.exe").is_err());
        assert!(merge_json_mcp_config(&p).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), original);
    }

    #[test]
    fn config_is_not_modified_when_backup_path_is_not_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let original = r#"{"theme":"dark"}"#;
        std::fs::write(&p, original).unwrap();
        // Something that is not a usable backup already occupies the backup path
        std::fs::create_dir(dir.path().join("settings.json.trackertop.bak")).unwrap();

        let err = install_claude_session_hook(&p, "C:/tt.exe").unwrap_err();
        assert!(format!("{err:#}").contains("not a regular file"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), original);
    }

    #[test]
    fn session_hook_merge_preserves_existing_settings_and_backs_up() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(
            &p,
            r#"{"theme":"dark","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"other"}]}]}}"#,
        )
        .unwrap();

        install_claude_session_hook(&p, "C:/tt.exe").unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        assert!(dir.path().join("settings.json.trackertop.bak").exists());

        // Idempotent: second run does not add a duplicate
        install_claude_session_hook(&p, "C:/tt.exe").unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn missing_or_empty_config_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mcp.json");
        assert_eq!(read_json_config(&p).unwrap(), json!({}));
        std::fs::write(&p, "  \n").unwrap();
        assert_eq!(read_json_config(&p).unwrap(), json!({}));
    }

    #[test]
    fn first_write_creates_missing_parent_folders() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir
            .path()
            .join(".codeium")
            .join("windsurf")
            .join("mcp_config.json");
        merge_json_mcp_config(&p).unwrap();
        assert!(json_registers_trackertop(
            &std::fs::read_to_string(&p).unwrap()
        ));
    }

    #[test]
    fn installed_detection_requires_a_real_mcp_registration() {
        // Hook-only settings mention the binary path but register no MCP server
        let hook_only = r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"\"C:/x/trackertop.exe\" hook-hint"}]}]}}"#;
        assert!(!json_registers_trackertop(hook_only));
        assert!(!json_registers_trackertop(
            r#"{"projects":{"C:/trackertop":{}}}"#
        ));
        assert!(!json_registers_trackertop("not json trackertop"));
        assert!(json_registers_trackertop(
            r#"{"mcpServers":{"trackertop":{"command":"t","args":["mcp"]}}}"#
        ));

        assert!(toml_registers_trackertop(
            "[other]\nx = 1\n\n[mcp_servers.trackertop]\ncommand = \"t\"\n"
        ));
        assert!(!toml_registers_trackertop(
            "# [mcp_servers.trackertop] removed\n"
        ));
    }

    fn utf16le(s: &str) -> Vec<u8> {
        crate::fsutil::encode_text(s, crate::fsutil::TextEncoding::Utf16Le)
    }

    #[test]
    fn codex_config_keeps_utf16_and_refuses_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let original = utf16le("model = \"o3\"\n");
        std::fs::write(&p, &original).unwrap();
        add_codex_server(&p, r"C:\t\trackertop.exe").unwrap();
        let (text, enc) = crate::fsutil::decode_text(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(enc, crate::fsutil::TextEncoding::Utf16Le);
        assert!(text.starts_with("model = \"o3\"\n"), "{text}");
        assert!(toml_registers_trackertop(&text), "{text}");
        assert_eq!(
            std::fs::read(dir.path().join("config.toml.trackertop.bak")).unwrap(),
            original
        );

        let bad = b"model = \"caf\xE9\"\n".to_vec();
        std::fs::write(&p, &bad).unwrap();
        assert!(add_codex_server(&p, "t").is_err());
        assert_eq!(std::fs::read(&p).unwrap(), bad);
    }

    #[test]
    fn session_hook_keeps_utf16_settings_and_refuses_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, utf16le(r#"{"theme":"dark"}"#)).unwrap();
        install_claude_session_hook(&p, "C:/t/trackertop.exe").unwrap();
        let (text, enc) = crate::fsutil::decode_text(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(enc, crate::fsutil::TextEncoding::Utf16Le);
        assert!(
            text.contains("\"theme\"") && text.contains("hook-hint"),
            "{text}"
        );

        let bad = b"{\"theme\":\"caf\xE9\"}".to_vec();
        std::fs::write(&p, &bad).unwrap();
        assert!(install_claude_session_hook(&p, "t").is_err());
        assert_eq!(std::fs::read(&p).unwrap(), bad);
    }

    #[test]
    fn claude_md_section_is_added_once_and_refused_when_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("CLAUDE.md");
        assert!(
            add_claude_md_section(&p).unwrap(),
            "a missing file is created"
        );
        assert!(
            !add_claude_md_section(&p).unwrap(),
            "second run changes nothing"
        );
        // UTF-16 without a byte order mark cannot be told apart from binary: refused
        let no_bom: Vec<u8> = "# Rules\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        std::fs::write(&p, &no_bom).unwrap();
        assert!(add_claude_md_section(&p).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), no_bom);
    }
}
