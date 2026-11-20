#![cfg_attr(
    all(target_os = "windows", feature = "gui"),
    windows_subsystem = "windows"
)]

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use trackertop::context::TrackerEngine;
use trackertop::db::Database;
use trackertop::indexer::index_repository;
use trackertop::install::{AiAgentKind, install_agent, install_claude_code};
use trackertop::mcp::run_mcp_server;
use trackertop::parser::default_registry;
use trackertop::registry::load_global_overview;
use trackertop::report::generate_markdown_report;

/// On Windows GUI builds (`windows_subsystem = "windows"`), double-clicking opens
/// zero console window. When invoked with CLI arguments from PowerShell/CMD (and stdout
/// is NOT already a redirected pipe like Claude Code MCP), attach the parent console
/// so CLI output prints normally.
#[cfg(all(target_os = "windows", feature = "gui"))]
fn attach_parent_console_if_cli() {
    use std::ffi::c_void;
    const STD_OUTPUT_HANDLE: u32 = (-11i32) as u32;
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;

    unsafe extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> *mut c_void;
        fn GetFileType(h_file: *mut c_void) -> u32;
        fn AttachConsole(dw_process_id: u32) -> i32;
    }

    let args: Vec<String> = std::env::args().collect();
    let is_gui_launch = args.len() <= 1 || args.iter().any(|a| a == "ui");
    if is_gui_launch {
        return;
    }

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        // FILE_TYPE_UNKNOWN (0) means no valid pipe/file/console is attached yet
        if handle.is_null() || GetFileType(handle) == 0 {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

#[derive(Parser)]
#[command(
    name = "trackertop",
    version,
    about = "AI-first codebase knowledge graph, surgical context packer, MCP server, multi-project desktop app & token-savings tracker"
)]
struct Cli {
    /// Path to target repository (defaults to current directory)
    #[arg(long, global = true)]
    repo: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Launch the offline native multi-project desktop application (default when run with no arguments)
    Ui,
    /// Incrementally index the repository into .trackertop/index.db
    Index {
        /// Force full re-parse of all files
        #[arg(long)]
        force: bool,
    },
    /// Run the MCP (Model Context Protocol) server over stdio for Claude Code & other AI agents
    Mcp,
    /// Run AI-first graph queries directly from the CLI (also logs token savings)
    Query {
        #[command(subcommand)]
        action: QueryAction,
    },
    /// Display real cumulative token savings and estimated USD cost reduction
    Stats {
        /// Clear recorded telemetry history
        #[arg(long)]
        reset: bool,
    },
    /// List all tracked projects and global cumulative token savings
    Projects,
    /// List all registered language extractors in the open parser core
    Languages,
    /// Generate concise Markdown summary at .trackertop/GRAPH_REPORT.md
    Report,
    /// Configure AI agent integration (claude, cursor, codex, windsurf)
    Install {
        /// Target agent (claude, cursor, codex, windsurf)
        #[arg(default_value = "claude")]
        target: String,
        /// Register globally in Claude Code user scope instead of project .mcp.json
        #[arg(long)]
        global: bool,
    },
    /// Lightweight SessionStart hook output reminding AI agents to prefer TrackerTop MCP tools
    HookHint,
}

#[derive(Subcommand)]
enum QueryAction {
    /// Architectural map: languages, God Nodes, and module clusters
    Map {
        #[arg(long, default_value_t = 900)]
        max_tokens: usize,
    },
    /// Signature-only outline of a file
    Outline {
        path: String,
        #[arg(long, default_value_t = 800)]
        max_tokens: usize,
    },
    /// Surgically read one symbol + 1-hop dependency signatures
    Symbol {
        name: String,
        #[arg(long, default_value_t = true)]
        deps: bool,
        #[arg(long, default_value_t = 1400)]
        max_tokens: usize,
    },
    /// Hybrid lexical + PageRank symbol search
    Find {
        query: String,
        #[arg(long, default_value_t = 12)]
        limit: usize,
        #[arg(long, default_value_t = 900)]
        max_tokens: usize,
    },
    /// Show N-hop callers and callees of a symbol
    Calls {
        symbol: String,
        #[arg(long, default_value_t = 2)]
        depth: usize,
        #[arg(long, default_value_t = 1000)]
        max_tokens: usize,
    },
    /// Blast radius & affected tests for a symbol
    Impact {
        symbol: String,
        #[arg(long, default_value_t = 2)]
        depth: usize,
        #[arg(long, default_value_t = 1000)]
        max_tokens: usize,
    },
    /// Pack highest-signal surgical context for a task inside max_tokens
    Pack {
        task: String,
        #[arg(long, default_value_t = 1800)]
        max_tokens: usize,
    },
    /// Shortest path between two symbols/files
    Path {
        from: String,
        to: String,
        #[arg(long, default_value_t = 600)]
        max_tokens: usize,
    },
    /// Graph-aware text search tagging every match with its enclosing symbol
    Grep {
        pattern: String,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long, default_value_t = 1200)]
        max_tokens: usize,
    },
    /// Git diff blast radius + co-changing files + linked tests
    Diff {
        #[arg(long, default_value_t = 1200)]
        max_tokens: usize,
    },
    /// Code health diagnostics (token bloat, circular cycles, orphans)
    Health {
        #[arg(long, default_value_t = 1000)]
        max_tokens: usize,
    },
}

fn main() -> Result<()> {
    #[cfg(all(target_os = "windows", feature = "gui"))]
    attach_parent_console_if_cli();

    let cli = Cli::parse();
    let repo_root = match cli.repo.clone() {
        Some(p) => std::fs::canonicalize(&p).unwrap_or(p),
        None => std::env::current_dir()?,
    };

    let cmd = cli.command.unwrap_or(Commands::Ui);

    match cmd {
        Commands::Ui => {
            #[cfg(feature = "gui")]
            {
                let initial = cli.repo.or_else(|| std::env::current_dir().ok());
                trackertop::ui::run_desktop_app(initial)?;
            }
            #[cfg(not(feature = "gui"))]
            {
                anyhow::bail!(
                    "TrackerTop was built without the `gui` feature. Rebuild with `--features gui` or pass a CLI subcommand (`trackertop --help`)."
                );
            }
        }
        Commands::Index { force } => {
            let stats = index_repository(&repo_root, force)?;
            println!(
                "TrackerTop indexed {} in {} ms\n  Files: {} total ({} parsed, {} cached, {} deleted)\n  Graph: {} symbols, {} edges\n  Repo size: {} tokens",
                repo_root.display(),
                stats.elapsed_ms,
                stats.total_files,
                stats.parsed_files,
                stats.skipped_files,
                stats.deleted_files,
                stats.total_symbols,
                stats.total_edges,
                stats.total_repo_tokens
            );
        }
        Commands::Mcp => {
            run_mcp_server(&repo_root)?;
        }
        Commands::Query { action } => {
            let engine = TrackerEngine::new(&repo_root);
            let res = match action {
                QueryAction::Map { max_tokens } => engine.repo_map(max_tokens)?,
                QueryAction::Outline { path, max_tokens } => {
                    engine.file_outline(&path, max_tokens)?
                }
                QueryAction::Symbol {
                    name,
                    deps,
                    max_tokens,
                } => engine.read_symbol(&name, deps, max_tokens)?,
                QueryAction::Find {
                    query,
                    limit,
                    max_tokens,
                } => engine.find_symbol(&query, limit, max_tokens)?,
                QueryAction::Calls {
                    symbol,
                    depth,
                    max_tokens,
                } => engine.call_graph(&symbol, depth, max_tokens)?,
                QueryAction::Impact {
                    symbol,
                    depth,
                    max_tokens,
                } => engine.impact_analysis(&symbol, depth, max_tokens)?,
                QueryAction::Pack { task, max_tokens } => engine.task_context(&task, max_tokens)?,
                QueryAction::Path {
                    from,
                    to,
                    max_tokens,
                } => engine.shortest_path(&from, &to, max_tokens)?,
                QueryAction::Grep {
                    pattern,
                    filter,
                    max_tokens,
                } => engine.smart_grep(&pattern, filter.as_deref(), max_tokens)?,
                QueryAction::Diff { max_tokens } => engine.diff_impact(max_tokens)?,
                QueryAction::Health { max_tokens } => engine.health_check(max_tokens)?,
            };
            println!("{}", res.text);
        }
        Commands::Stats { reset } => {
            let db = Database::open_for_repo(&repo_root)?;
            if reset {
                db.clear_telemetry()?;
                println!("Cleared TrackerTop telemetry history.");
                return Ok(());
            }
            let s = db.get_token_stats(10)?;
            print!("{}", s.summary_text(10));
            if !s.by_tool.is_empty() {
                println!("\nBy Tool:");
                for t in &s.by_tool {
                    println!(
                        "  {:<16} {:>3}x | sent {:>6} | base {:>7} | saved {:>7} ({:>5.1}%)",
                        t.tool,
                        t.queries,
                        t.tokens_sent,
                        t.baseline_tokens,
                        t.tokens_saved,
                        t.savings_percent
                    );
                }
            }
        }
        Commands::Projects => {
            let ov = load_global_overview();
            println!(
                "=== TrackerTop Tracked Projects ({}) ===",
                ov.projects.len()
            );
            println!(
                "Global Savings: {} tok ({:.1}%) | Est. USD: ${:.4} (Sonnet) / ${:.4} (Opus)\n",
                ov.total_tokens_saved,
                ov.savings_percent,
                ov.est_usd_saved_sonnet,
                ov.est_usd_saved_opus
            );
            for p in ov.projects {
                println!(
                    "  {:<22} {:>4} files | {:>7} saved ({:>5.1}%) | {}",
                    p.entry.name,
                    p.total_files,
                    p.stats.total_tokens_saved,
                    p.stats.savings_percent,
                    p.entry.path
                );
            }
        }
        Commands::Languages => {
            println!("=== Registered Language Extractors ===");
            for l in default_registry().languages() {
                println!(
                    "  {:<12} {:<12} [{}] ({})",
                    l.id,
                    l.display_name,
                    l.extensions.join(", "),
                    l.color_hex
                );
            }
        }
        Commands::Report => {
            index_repository(&repo_root, false)?;
            let md_path = generate_markdown_report(&repo_root)?;
            println!("Generated Markdown summary: {}", md_path.display());
        }
        Commands::Install { target, global } => {
            let actions = match target.to_ascii_lowercase().as_str() {
                "claude" => install_claude_code(&repo_root, global)?,
                "cursor" => install_agent(AiAgentKind::Cursor, &repo_root)?,
                "codex" => install_agent(AiAgentKind::Codex, &repo_root)?,
                "windsurf" => install_agent(AiAgentKind::Windsurf, &repo_root)?,
                _ => anyhow::bail!(
                    "Unsupported install target `{target}`. Supported: claude, cursor, codex, windsurf"
                ),
            };
            println!("TrackerTop {target} integration complete:");
            for a in actions {
                println!("  - {a}");
            }
        }
        Commands::HookHint => {
            println!(
                "TrackerTop MCP is active for this workspace. Prefer `repo_map`, `task_context`, `smart_grep`, `file_outline`, `read_symbol`, `read_symbols_batch`, and `diff_impact` BEFORE raw grep or reading full files (saves 80-99% tokens)."
            );
        }
    }

    Ok(())
}
