use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use trackertop::context::TrackerEngine;
use trackertop::db::Database;
use trackertop::indexer::index_repository;
use trackertop::install::install_claude_code;
use trackertop::mcp::run_mcp_server;
use trackertop::parser::default_registry;
use trackertop::registry::load_global_overview;
use trackertop::report::generate_reports;

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
    /// Generate offline interactive HTML graph + token-savings dashboard and GRAPH_REPORT.md
    Report,
    /// Configure Claude Code integration (.mcp.json, SKILL.md, CLAUDE.md)
    Install {
        /// Target agent (currently: claude)
        #[arg(default_value = "claude")]
        target: String,
        /// Register globally in Claude Code user scope instead of project .mcp.json
        #[arg(long)]
        global: bool,
    },
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
}

fn main() -> Result<()> {
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
                anyhow::bail!("TrackerTop was built without the `gui` feature. Rebuild with `--features gui` or pass a CLI subcommand (`trackertop --help`).");
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
                QueryAction::Outline { path, max_tokens } => engine.file_outline(&path, max_tokens)?,
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
            println!("=== TrackerTop Token Savings ===");
            println!("Queries logged   : {}", s.total_queries);
            println!("Tokens sent      : {}", s.total_tokens_sent);
            println!("Baseline avoided : {}", s.total_baseline_tokens);
            println!(
                "Net tokens saved : {} ({:.1}%)",
                s.total_tokens_saved, s.savings_percent
            );
            println!(
                "Est. USD saved   : ${:.4} (Sonnet) | ${:.4} (Opus)",
                s.est_usd_saved_sonnet, s.est_usd_saved_opus
            );
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
            let paths = generate_reports(&repo_root)?;
            println!("Generated TrackerTop reports:");
            println!("  Interactive Dashboard : {}", paths.html_path.display());
            println!("  Markdown Summary      : {}", paths.markdown_path.display());
        }
        Commands::Install { target, global } => {
            if !target.eq_ignore_ascii_case("claude") {
                anyhow::bail!("Unsupported install target `{target}`. Supported: claude");
            }
            let actions = install_claude_code(&repo_root, global)?;
            println!("TrackerTop Claude Code integration complete:");
            for a in actions {
                println!("  - {a}");
            }
        }
    }

    Ok(())
}
