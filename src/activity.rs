//! Agent activity: which files and symbols AI agents touched, derived from the telemetry
//! table (MCP calls) and the git working tree.
//!
//! A telemetry row stores the tool and its query text, not a resolved target. A query is
//! attributed to a file only when it names one unambiguously: an indexed path, a path suffix
//! matching exactly one file, or a symbol name whose every match lives in one file. Free-text
//! queries (`task_context`, `smart_grep` patterns) and ambiguous names (`new`) stay
//! unattributed and are counted as such, never guessed.

use crate::model::{FileRecord, Symbol, SymbolKind, TelemetryEntry};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};

/// Rows further apart than this, or from another client, start a new session.
pub const SESSION_GAP_MINUTES: i64 = 30;
/// Heat covers this many days before now.
pub const HEAT_DAYS: i64 = 7;

/// One telemetry row of the latest session, with what it resolved to.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivityStep {
    pub timestamp: String,
    pub tool: String,
    pub query: String,
    pub tokens_sent: usize,
    /// Files the query names (empty when unattributed).
    pub files: Vec<String>,
    /// The symbol id when the query names exactly one symbol.
    pub symbol: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AgentActivity {
    /// Calls per file within [`HEAT_DAYS`].
    pub heat: HashMap<String, usize>,
    /// Calls per symbol id within [`HEAT_DAYS`].
    pub symbol_heat: HashMap<String, usize>,
    /// Agent calls within [`HEAT_DAYS`].
    pub calls: usize,
    /// Of those, calls attributed to at least one file.
    pub attributed: usize,
    /// The latest session, oldest step first.
    pub session: Vec<ActivityStep>,
    /// Client of the latest session (`mcp:<name>`).
    pub session_source: String,
    /// Files with uncommitted changes in the working tree.
    pub changed: HashSet<String>,
}

impl AgentActivity {
    pub fn max_heat(&self) -> usize {
        self.heat.values().copied().max().unwrap_or(0)
    }
}

/// Resolves telemetry query text to files and symbols of the index.
pub struct QueryResolver<'a> {
    files: &'a HashMap<String, FileRecord>,
    by_name: HashMap<&'a str, Vec<&'a Symbol>>,
    by_qualified: HashMap<&'a str, Vec<&'a Symbol>>,
}

impl<'a> QueryResolver<'a> {
    pub fn new(
        files: &'a HashMap<String, FileRecord>,
        symbols: &'a HashMap<String, Symbol>,
    ) -> Self {
        let mut by_name: HashMap<&str, Vec<&Symbol>> = HashMap::new();
        let mut by_qualified: HashMap<&str, Vec<&Symbol>> = HashMap::new();
        for s in symbols.values() {
            if s.kind == SymbolKind::File {
                continue;
            }
            by_name.entry(s.name.as_str()).or_default().push(s);
            by_qualified
                .entry(s.qualified_name.as_str())
                .or_default()
                .push(s);
        }
        Self {
            files,
            by_name,
            by_qualified,
        }
    }

    /// Files and (when unique) the symbol id a query names. See the module docs.
    pub fn resolve(&self, query: &str) -> (Vec<String>, Option<String>) {
        let q = query.trim().replace('\\', "/");
        let q = q.trim_start_matches("./");
        if let Some(r) = self.resolve_one(q) {
            return r;
        }
        // Batch tools take comma-separated targets.
        if q.contains(',') {
            let mut files: Vec<String> = Vec::new();
            for part in q.split(',') {
                if let Some((fs, _)) = self.resolve_one(part.trim()) {
                    for f in fs {
                        if !files.contains(&f) {
                            files.push(f);
                        }
                    }
                }
            }
            return (files, None);
        }
        (Vec::new(), None)
    }

    fn resolve_one(&self, q: &str) -> Option<(Vec<String>, Option<String>)> {
        if q.is_empty() {
            return None;
        }
        if self.files.contains_key(q) {
            return Some((vec![q.to_string()], None));
        }
        if q.contains('/') || (q.contains('.') && !q.contains("::")) {
            let suffix = format!("/{q}");
            let mut hits = self.files.keys().filter(|p| p.ends_with(&suffix));
            if let (Some(one), None) = (hits.next(), hits.next()) {
                return Some((vec![one.clone()], None));
            }
        }
        let cands = self.by_qualified.get(q).or_else(|| self.by_name.get(q))?;
        let first = cands.first()?;
        if cands.iter().any(|s| s.file_path != first.file_path) {
            return None;
        }
        let symbol = (cands.len() == 1).then(|| first.id.clone());
        Some((vec![first.file_path.clone()], symbol))
    }
}

fn parse_ts(ts: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Builds heat and the latest session from agent telemetry rows (oldest first).
pub fn build_activity(
    entries: &[TelemetryEntry],
    resolver: &QueryResolver<'_>,
    changed: HashSet<String>,
    now: DateTime<Utc>,
) -> AgentActivity {
    let mut act = AgentActivity {
        changed,
        ..Default::default()
    };
    let since = now - chrono::Duration::days(HEAT_DAYS);
    let resolved: Vec<(Vec<String>, Option<String>)> =
        entries.iter().map(|e| resolver.resolve(&e.query)).collect();

    for (e, (files, symbol)) in entries.iter().zip(&resolved) {
        if !parse_ts(&e.timestamp).is_some_and(|t| t >= since && t <= now) {
            continue;
        }
        act.calls += 1;
        if !files.is_empty() {
            act.attributed += 1;
        }
        for f in files {
            *act.heat.entry(f.clone()).or_insert(0) += 1;
        }
        if let Some(s) = symbol {
            *act.symbol_heat.entry(s.clone()).or_insert(0) += 1;
        }
    }

    // Latest session: walk back from the newest row while the client stays the same and
    // consecutive rows are at most SESSION_GAP_MINUTES apart.
    let Some(last) = entries.last() else {
        return act;
    };
    let mut start = entries.len() - 1;
    while start > 0 {
        let (prev, cur) = (&entries[start - 1], &entries[start]);
        let close = match (parse_ts(&prev.timestamp), parse_ts(&cur.timestamp)) {
            (Some(a), Some(b)) => (b - a).num_minutes() <= SESSION_GAP_MINUTES && b >= a,
            _ => false,
        };
        if prev.source != last.source || !close {
            break;
        }
        start -= 1;
    }
    act.session_source = last.source.clone();
    act.session = entries[start..]
        .iter()
        .zip(&resolved[start..])
        .map(|(e, (files, symbol))| ActivityStep {
            timestamp: e.timestamp.clone(),
            tool: e.tool.clone(),
            query: e.query.clone(),
            tokens_sent: e.tokens_sent,
            files: files.clone(),
            symbol: symbol.clone(),
        })
        .collect();
    act
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> (String, FileRecord) {
        (
            path.to_string(),
            FileRecord {
                path: path.to_string(),
                language: "rust".into(),
                hash: String::new(),
                mtime_secs: 0,
                size_bytes: 0,
                line_count: 1,
                token_count: 1,
            },
        )
    }

    fn sym(path: &str, qualified: &str) -> (String, Symbol) {
        let name = qualified.rsplit("::").next().unwrap().to_string();
        let id = format!("{path}::{qualified}#1");
        (
            id.clone(),
            Symbol {
                id,
                file_path: path.into(),
                name,
                qualified_name: qualified.into(),
                kind: SymbolKind::Function,
                signature: String::new(),
                start_line: 1,
                end_line: 2,
                token_count: 1,
                visibility: String::new(),
                doc_summary: String::new(),
                parent_id: None,
            },
        )
    }

    fn entry(id: i64, ts: &str, tool: &str, query: &str, source: &str) -> TelemetryEntry {
        TelemetryEntry {
            id,
            timestamp: ts.into(),
            tool: tool.into(),
            query: query.into(),
            tokens_sent: 10,
            baseline_tokens: 100,
            tokens_saved: 90,
            files_avoided: 1,
            latency_ms: 1,
            source: source.into(),
        }
    }

    fn fixture() -> (HashMap<String, FileRecord>, HashMap<String, Symbol>) {
        let files = ["src/ui.rs", "src/nested.rs", "src/graph.rs", "tests/ui.rs"]
            .into_iter()
            .map(file)
            .collect();
        let symbols = [
            sym("src/nested.rs", "NestedGraph::view"),
            sym("src/graph.rs", "CodeGraph::new"),
            sym("src/nested.rs", "NestedGraph::new"),
            sym("src/graph.rs", "CodeGraph"),
        ]
        .into_iter()
        .collect();
        (files, symbols)
    }

    #[test]
    fn queries_resolve_only_when_unambiguous() {
        let (files, symbols) = fixture();
        let r = QueryResolver::new(&files, &symbols);
        assert_eq!(r.resolve("src/ui.rs").0, vec!["src/ui.rs"]);
        assert_eq!(r.resolve(".\\src\\graph.rs").0, vec!["src/graph.rs"]);
        // `ui.rs` matches src/ui.rs and tests/ui.rs: unattributed.
        assert!(r.resolve("ui.rs").0.is_empty());
        assert_eq!(r.resolve("nested.rs").0, vec!["src/nested.rs"]);
        let (f, s) = r.resolve("NestedGraph::view");
        assert_eq!(f, vec!["src/nested.rs"]);
        assert_eq!(s.as_deref(), Some("src/nested.rs::NestedGraph::view#1"));
        // `view` is unique by name; `new` lives in two files: unattributed.
        assert_eq!(r.resolve("view").0, vec!["src/nested.rs"]);
        assert!(r.resolve("new").0.is_empty());
        assert!(r.resolve("how does drill-down work").0.is_empty());
        let (f, s) = r.resolve("CodeGraph, src/ui.rs");
        assert_eq!(f, vec!["src/graph.rs", "src/ui.rs"]);
        assert_eq!(s, None);
    }

    #[test]
    fn heat_counts_recent_attributed_calls_and_session_splits_on_gap_and_client() {
        let (files, symbols) = fixture();
        let r = QueryResolver::new(&files, &symbols);
        let now = parse_ts("2026-11-20T15:00:00+00:00").unwrap();
        let rows = vec![
            entry(
                1,
                "2026-11-01T10:00:00+00:00",
                "file_outline",
                "src/ui.rs",
                "mcp:a",
            ),
            entry(
                2,
                "2026-11-20T13:00:00+00:00",
                "file_outline",
                "src/ui.rs",
                "mcp:a",
            ),
            entry(
                3,
                "2026-11-20T14:00:00+00:00",
                "read_symbol",
                "NestedGraph::view",
                "mcp:b",
            ),
            entry(
                4,
                "2026-11-20T14:10:00+00:00",
                "task_context",
                "drill down",
                "mcp:b",
            ),
            entry(
                5,
                "2026-11-20T14:20:00+00:00",
                "file_outline",
                "src/ui.rs",
                "mcp:b",
            ),
        ];
        let act = build_activity(&rows, &r, HashSet::new(), now);
        // Row 1 is older than HEAT_DAYS.
        assert_eq!(act.calls, 4);
        assert_eq!(act.attributed, 3);
        assert_eq!(act.heat.get("src/ui.rs"), Some(&2));
        assert_eq!(act.heat.get("src/nested.rs"), Some(&1));
        assert_eq!(act.max_heat(), 2);
        // Row 2 is another client and an hour earlier: the session is rows 3..5.
        assert_eq!(act.session_source, "mcp:b");
        let tools: Vec<&str> = act.session.iter().map(|s| s.tool.as_str()).collect();
        assert_eq!(tools, ["read_symbol", "task_context", "file_outline"]);
        assert!(act.session[1].files.is_empty());

        let gap = vec![
            entry(1, "2026-11-20T12:00:00+00:00", "a", "src/ui.rs", "mcp:b"),
            entry(2, "2026-11-20T14:00:00+00:00", "b", "src/ui.rs", "mcp:b"),
        ];
        assert_eq!(
            build_activity(&gap, &r, HashSet::new(), now).session.len(),
            1
        );
        assert!(
            build_activity(&[], &r, HashSet::new(), now)
                .session
                .is_empty()
        );
    }
}
