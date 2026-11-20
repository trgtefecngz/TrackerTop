use crate::db::Database;
use crate::graph::CodeGraph;
use crate::indexer::index_repository;
use crate::model::{ProjectNote, Symbol, SymbolKind, TokenStatsSummary};
use crate::parser::default_registry;
use crate::tokens::{count_tokens, truncate_to_tokens};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

mod analysis;
mod memory;
mod reading;
mod search;
mod workflow;

pub use search::{GrepFile, GrepScan, grep_scan};

/// Result of an AI-first query, including exact token-savings metrics.
#[derive(Debug, Clone)]
pub struct QueryOutput {
    pub text: String,
    pub tokens_sent: usize,
    pub baseline_tokens: usize,
    pub tokens_saved: usize,
    pub files_avoided: usize,
    pub latency_ms: u64,
}

pub struct TrackerEngine {
    pub repo_root: PathBuf,
    /// Last built graph, tagged with the DB `index_generation` it was built from.
    /// Long-lived engines (the MCP server) reuse it until the index changes.
    graph_cache: Mutex<Option<(i64, Arc<CodeGraph>)>>,
    /// Telemetry source tag for calls made through this engine (see [`Self::set_source`]).
    source: Mutex<String>,
}

impl TrackerEngine {
    /// The source defaults to `cli`, or to `TRACKERTOP_TELEMETRY_SOURCE` when set (e.g.
    /// `benchmark` for scripted measurement runs, so they never count as agent usage).
    pub fn new(repo_root: &Path) -> Self {
        let source = std::env::var("TRACKERTOP_TELEMETRY_SOURCE")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "cli".to_string());
        Self {
            repo_root: repo_root.to_path_buf(),
            graph_cache: Mutex::new(None),
            source: Mutex::new(source),
        }
    }

    /// Tags later telemetry rows: `mcp:<client>` for MCP sessions, `ui` for the desktop app.
    /// An explicit `TRACKERTOP_TELEMETRY_SOURCE` always wins, so benchmarks stay separate.
    pub fn set_source(&self, source: &str) {
        if std::env::var("TRACKERTOP_TELEMETRY_SOURCE").is_ok_and(|s| !s.trim().is_empty()) {
            return;
        }
        *self
            .source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = source.to_string();
    }

    pub fn source(&self) -> String {
        self.source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn load_fresh(&self) -> Result<(Database, Arc<CodeGraph>)> {
        // Fast incremental refresh (mtime + size check skips unchanged files in milliseconds)
        let stats = index_repository(&self.repo_root, false)?;
        let db = Database::open_for_repo(&self.repo_root)?;

        let mut cache = self
            .graph_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((generation, graph)) = cache.as_ref()
            && *generation == stats.generation
        {
            return Ok((db, Arc::clone(graph)));
        }
        let graph = Arc::new(CodeGraph::load(&db)?);
        *cache = Some((stats.generation, Arc::clone(&graph)));
        Ok((db, graph))
    }

    #[allow(clippy::too_many_arguments)]
    fn finalize(
        &self,
        db: &Database,
        tool: &str,
        query_label: &str,
        body: String,
        max_tokens: usize,
        baseline_tokens: usize,
        files_avoided: usize,
        start: Instant,
    ) -> Result<QueryOutput> {
        self.finalize_inner(
            db,
            tool,
            query_label,
            body,
            max_tokens,
            baseline_tokens,
            files_avoided,
            start,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finalize_inner(
        &self,
        db: &Database,
        tool: &str,
        query_label: &str,
        body: String,
        max_tokens: usize,
        baseline_tokens: usize,
        files_avoided: usize,
        start: Instant,
        record_telemetry: bool,
    ) -> Result<QueryOutput> {
        let budget = if max_tokens == 0 { 1500 } else { max_tokens };
        // Reserve ~16 tokens for the compact savings footer
        let (trimmed, _, _) = truncate_to_tokens(&body, budget.saturating_sub(16).max(64));
        let sent_est = count_tokens(&trimmed) + 14;
        let saved = baseline_tokens.saturating_sub(sent_est);
        let pct = if baseline_tokens > 0 {
            (saved as f64 / baseline_tokens as f64) * 100.0
        } else {
            0.0
        };

        let final_text = format!(
            "{trimmed}\n\n[ttop:{tool} | sent {sent_est} tok | baseline {baseline_tokens} tok | saved {saved} tok ({pct:.0}%)]"
        );
        let tokens_sent = count_tokens(&final_text);
        let tokens_saved = baseline_tokens.saturating_sub(tokens_sent);
        let latency_ms = start.elapsed().as_millis() as u64;

        if record_telemetry {
            let _ = db.record_telemetry(
                tool,
                query_label,
                tokens_sent,
                baseline_tokens,
                files_avoided,
                latency_ms,
                &self.source(),
            );
        }

        Ok(QueryOutput {
            text: final_text,
            tokens_sent,
            baseline_tokens,
            tokens_saved,
            files_avoided,
            latency_ms,
        })
    }

    /// 8. Token-savings telemetry summary.
    pub fn token_stats(&self) -> Result<TokenStatsSummary> {
        let db = Database::open_for_repo(&self.repo_root)?;
        db.get_token_stats(20)
    }
}
/// One line telling the agent that a list was cut, so a partial list never looks complete.
fn more_note(total: usize, shown: usize, what: &str) -> String {
    if total > shown {
        format!(
            "(showing {shown} of {total} {what}; {} more not shown)\n",
            total - shown
        )
    } else {
        String::new()
    }
}
