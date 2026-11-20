use crate::db::Database;
use crate::graph::CodeGraph;
use crate::model::SymbolKind;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Generates `.trackertop/GRAPH_REPORT.md` — a concise Markdown summary of
/// repository architecture, God Nodes, module clusters, and real token savings.
pub fn generate_markdown_report(repo_root: &Path) -> Result<PathBuf> {
    let db = Database::open_for_repo(repo_root)?;
    let cg = CodeGraph::load(&db)?;
    let stats = db.get_token_stats(30)?;

    let out_dir = repo_root.join(".trackertop");
    std::fs::create_dir_all(&out_dir)?;

    let repo_name = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Repository");

    let total_symbols = cg
        .symbols
        .values()
        .filter(|s| s.kind != SymbolKind::File && s.kind != SymbolKind::Impl)
        .count();
    let total_repo_tokens: usize = cg.files.values().map(|f| f.token_count).sum();
    let god_nodes = cg.god_nodes(15, false);
    let communities = cg.communities();

    let mut md = String::new();
    md.push_str(&format!("# TrackerTop Report — {repo_name}\n\n"));
    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n|---|---|\n");
    md.push_str(&format!("| Indexed Files | {} |\n", cg.files.len()));
    md.push_str(&format!("| Extracted Symbols | {total_symbols} |\n"));
    md.push_str(&format!("| Graph Edges | {} |\n", cg.edges.len()));
    md.push_str(&format!("| Total Repo Tokens | {total_repo_tokens} |\n"));
    md.push_str(&format!(
        "| AI Queries Logged | {} |\n",
        stats.total_queries
    ));
    md.push_str(&format!(
        "| Net Tokens Saved | **{} ({:.1}%)** |\n",
        stats.total_tokens_saved, stats.savings_percent
    ));
    md.push_str(&format!(
        "| Est. Savings (Sonnet / Opus) | ${:.4} / ${:.4} |\n\n",
        stats.est_usd_saved_sonnet, stats.est_usd_saved_opus
    ));

    md.push_str("## Architectural Hubs (God Nodes)\n\n");
    md.push_str("| Symbol | Kind | File | In | Out | Score |\n|---|---|---|---:|---:|---:|\n");
    for g in &god_nodes {
        md.push_str(&format!(
            "| `{}` | {} | `{}` | {} | {} | {:.2} |\n",
            g.name,
            g.kind.as_str(),
            g.file_path,
            g.in_degree,
            g.out_degree,
            g.score
        ));
    }

    md.push_str("\n## Module Clusters (Communities)\n\n");
    for c in &communities {
        md.push_str(&format!(
            "- **C{} `{}`** — {} files, {} symbols, {} tokens | Hubs: `{}`\n",
            c.id,
            c.label,
            c.files.len(),
            c.symbol_count,
            c.total_tokens,
            c.top_hubs.join("`, `")
        ));
    }

    if !stats.by_tool.is_empty() {
        md.push_str("\n## Token Savings by Tool\n\n");
        md.push_str("| Tool | Queries | Sent | Baseline | Saved | Savings % |\n|---|---:|---:|---:|---:|---:|\n");
        for t in &stats.by_tool {
            md.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {:.1}% |\n",
                t.tool,
                t.queries,
                t.tokens_sent,
                t.baseline_tokens,
                t.tokens_saved,
                t.savings_percent
            ));
        }
    }

    let md_path = out_dir.join("GRAPH_REPORT.md");
    std::fs::write(&md_path, md)
        .with_context(|| format!("failed to write {}", md_path.display()))?;
    Ok(md_path)
}
