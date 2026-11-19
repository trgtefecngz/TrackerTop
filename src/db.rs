use crate::model::{
    Edge, EdgeKind, FileRecord, ProjectNote, Symbol, SymbolKind, TelemetryEntry, TokenStatsSummary,
    ToolSavingsBreakdown,
};
use crate::tokens::estimate_usd_saved;
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Database {
    conn: Connection,
    pub db_path: PathBuf,
}

impl Database {
    pub fn open_for_repo(repo_root: &Path) -> Result<Self> {
        let dir = repo_root.join(".trackertop");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        let db_path = dir.join("index.db");
        Self::open_path(&db_path)
    }

    pub fn open_path(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path)
            .with_context(|| format!("failed to open sqlite db at {}", db_path.display()))?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            ",
        )?;
        let db = Self {
            conn,
            db_path: db_path.to_path_buf(),
        };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS files (
                path TEXT PRIMARY KEY,
                language TEXT NOT NULL,
                hash TEXT NOT NULL,
                mtime_secs INTEGER NOT NULL,
                size_bytes INTEGER NOT NULL,
                line_count INTEGER NOT NULL,
                token_count INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS symbols (
                id TEXT PRIMARY KEY,
                file_path TEXT NOT NULL,
                name TEXT NOT NULL,
                qualified_name TEXT NOT NULL,
                kind TEXT NOT NULL,
                signature TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                token_count INTEGER NOT NULL,
                visibility TEXT NOT NULL,
                doc_summary TEXT NOT NULL,
                parent_id TEXT,
                FOREIGN KEY (file_path) REFERENCES files(path) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS raw_refs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_path TEXT NOT NULL,
                source_symbol_id TEXT NOT NULL,
                target_name TEXT NOT NULL,
                kind TEXT NOT NULL,
                line INTEGER NOT NULL,
                FOREIGN KEY (file_path) REFERENCES files(path) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS edges (
                source_id TEXT NOT NULL,
                target_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                weight REAL NOT NULL,
                PRIMARY KEY (source_id, target_id, kind)
            );

            CREATE TABLE IF NOT EXISTS telemetry (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                tool TEXT NOT NULL,
                query TEXT NOT NULL,
                tokens_sent INTEGER NOT NULL,
                baseline_tokens INTEGER NOT NULL,
                tokens_saved INTEGER NOT NULL,
                files_avoided INTEGER NOT NULL,
                latency_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS notes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                target TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_path);
            CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
            CREATE INDEX IF NOT EXISTS idx_raw_refs_file ON raw_refs(file_path);
            CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source_id);
            CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target_id);
            CREATE INDEX IF NOT EXISTS idx_notes_target ON notes(target);
            ",
        )?;
        Ok(())
    }

    pub fn load_file_map(&self) -> Result<HashMap<String, FileRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, language, hash, mtime_secs, size_bytes, line_count, token_count FROM files",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(FileRecord {
                path: row.get(0)?,
                language: row.get(1)?,
                hash: row.get(2)?,
                mtime_secs: row.get(3)?,
                size_bytes: row.get::<_, i64>(4)? as u64,
                line_count: row.get::<_, i64>(5)? as usize,
                token_count: row.get::<_, i64>(6)? as usize,
            })
        })?;
        let mut map = HashMap::new();
        for r in rows {
            let rec = r?;
            map.insert(rec.path.clone(), rec);
        }
        Ok(map)
    }

    pub fn delete_files(&mut self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM files WHERE path = ?1")?;
            for p in paths {
                stmt.execute(params![p])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_parsed_files(
        &mut self,
        entries: &[(FileRecord, Vec<Symbol>, Vec<crate::model::RawReference>)],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut del_syms = tx.prepare("DELETE FROM symbols WHERE file_path = ?1")?;
            let mut del_refs = tx.prepare("DELETE FROM raw_refs WHERE file_path = ?1")?;
            let mut ins_file = tx.prepare(
                "INSERT INTO files (path, language, hash, mtime_secs, size_bytes, line_count, token_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(path) DO UPDATE SET
                   language=excluded.language,
                   hash=excluded.hash,
                   mtime_secs=excluded.mtime_secs,
                   size_bytes=excluded.size_bytes,
                   line_count=excluded.line_count,
                   token_count=excluded.token_count",
            )?;
            let mut ins_sym = tx.prepare(
                "INSERT OR REPLACE INTO symbols
                 (id, file_path, name, qualified_name, kind, signature, start_line, end_line, token_count, visibility, doc_summary, parent_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?;
            let mut ins_ref = tx.prepare(
                "INSERT INTO raw_refs (file_path, source_symbol_id, target_name, kind, line)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;

            for (file, syms, refs) in entries {
                ins_file.execute(params![
                    file.path,
                    file.language,
                    file.hash,
                    file.mtime_secs,
                    file.size_bytes as i64,
                    file.line_count as i64,
                    file.token_count as i64,
                ])?;
                del_syms.execute(params![file.path])?;
                del_refs.execute(params![file.path])?;

                for s in syms {
                    ins_sym.execute(params![
                        s.id,
                        s.file_path,
                        s.name,
                        s.qualified_name,
                        s.kind.as_str(),
                        s.signature,
                        s.start_line as i64,
                        s.end_line as i64,
                        s.token_count as i64,
                        s.visibility,
                        s.doc_summary,
                        s.parent_id,
                    ])?;
                }
                for r in refs {
                    ins_ref.execute(params![
                        file.path,
                        r.source_symbol_id,
                        r.target_name,
                        r.kind.as_str(),
                        r.line as i64,
                    ])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn replace_edges(&mut self, edges: &[Edge]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM edges", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO edges (source_id, target_id, kind, weight)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for e in edges {
                stmt.execute(params![e.source_id, e.target_id, e.kind.as_str(), e.weight])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_all_files(&self) -> Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, language, hash, mtime_secs, size_bytes, line_count, token_count
             FROM files ORDER BY path",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(FileRecord {
                path: row.get(0)?,
                language: row.get(1)?,
                hash: row.get(2)?,
                mtime_secs: row.get(3)?,
                size_bytes: row.get::<_, i64>(4)? as u64,
                line_count: row.get::<_, i64>(5)? as usize,
                token_count: row.get::<_, i64>(6)? as usize,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn load_all_symbols(&self) -> Result<Vec<Symbol>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, file_path, name, qualified_name, kind, signature, start_line, end_line,
                    token_count, visibility, doc_summary, parent_id
             FROM symbols ORDER BY file_path, start_line",
        )?;
        let rows = stmt.query_map([], |row| {
            let kind_str: String = row.get(4)?;
            Ok(Symbol {
                id: row.get(0)?,
                file_path: row.get(1)?,
                name: row.get(2)?,
                qualified_name: row.get(3)?,
                kind: SymbolKind::from_str_lossy(&kind_str),
                signature: row.get(5)?,
                start_line: row.get::<_, i64>(6)? as usize,
                end_line: row.get::<_, i64>(7)? as usize,
                token_count: row.get::<_, i64>(8)? as usize,
                visibility: row.get(9)?,
                doc_summary: row.get(10)?,
                parent_id: row.get(11)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn load_all_raw_refs(&self) -> Result<Vec<(String, crate::model::RawReference)>> {
        let mut stmt = self.conn.prepare(
            "SELECT file_path, source_symbol_id, target_name, kind, line FROM raw_refs",
        )?;
        let rows = stmt.query_map([], |row| {
            let kind_str: String = row.get(3)?;
            Ok((
                row.get::<_, String>(0)?,
                crate::model::RawReference {
                    source_symbol_id: row.get(1)?,
                    target_name: row.get(2)?,
                    kind: EdgeKind::from_str_lossy(&kind_str),
                    line: row.get::<_, i64>(4)? as usize,
                },
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn load_all_edges(&self) -> Result<Vec<Edge>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source_id, target_id, kind, weight FROM edges")?;
        let rows = stmt.query_map([], |row| {
            let kind_str: String = row.get(2)?;
            Ok(Edge {
                source_id: row.get(0)?,
                target_id: row.get(1)?,
                kind: EdgeKind::from_str_lossy(&kind_str),
                weight: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn record_telemetry(
        &self,
        tool: &str,
        query: &str,
        tokens_sent: usize,
        baseline_tokens: usize,
        files_avoided: usize,
        latency_ms: u64,
    ) -> Result<()> {
        let tokens_saved = baseline_tokens.saturating_sub(tokens_sent);
        let ts = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO telemetry (timestamp, tool, query, tokens_sent, baseline_tokens, tokens_saved, files_avoided, latency_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                ts,
                tool,
                query,
                tokens_sent as i64,
                baseline_tokens as i64,
                tokens_saved as i64,
                files_avoided as i64,
                latency_ms as i64,
            ],
        )?;
        Ok(())
    }

    pub fn get_token_stats(&self, recent_limit: usize) -> Result<TokenStatsSummary> {
        let (total_queries, total_sent, total_base, total_saved): (i64, i64, i64, i64) =
            self.conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(tokens_sent),0), COALESCE(SUM(baseline_tokens),0), COALESCE(SUM(tokens_saved),0)
                 FROM telemetry",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;

        let mut stmt = self.conn.prepare(
            "SELECT tool, COUNT(*), SUM(tokens_sent), SUM(baseline_tokens), SUM(tokens_saved)
             FROM telemetry
             GROUP BY tool
             ORDER BY SUM(tokens_saved) DESC",
        )?;
        let by_tool = stmt
            .query_map([], |r| {
                let sent = r.get::<_, i64>(2)? as usize;
                let base = r.get::<_, i64>(3)? as usize;
                let saved = r.get::<_, i64>(4)? as usize;
                let pct = if base > 0 {
                    (saved as f64 / base as f64) * 100.0
                } else {
                    0.0
                };
                Ok(ToolSavingsBreakdown {
                    tool: r.get(0)?,
                    queries: r.get::<_, i64>(1)? as usize,
                    tokens_sent: sent,
                    baseline_tokens: base,
                    tokens_saved: saved,
                    savings_percent: pct,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut recent_stmt = self.conn.prepare(
            "SELECT id, timestamp, tool, query, tokens_sent, baseline_tokens, tokens_saved, files_avoided, latency_ms
             FROM telemetry
             ORDER BY id DESC
             LIMIT ?1",
        )?;
        let recent = recent_stmt
            .query_map(params![recent_limit as i64], |r| {
                Ok(TelemetryEntry {
                    id: r.get(0)?,
                    timestamp: r.get(1)?,
                    tool: r.get(2)?,
                    query: r.get(3)?,
                    tokens_sent: r.get::<_, i64>(4)? as usize,
                    baseline_tokens: r.get::<_, i64>(5)? as usize,
                    tokens_saved: r.get::<_, i64>(6)? as usize,
                    files_avoided: r.get::<_, i64>(7)? as usize,
                    latency_ms: r.get::<_, i64>(8)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let savings_percent = if total_base > 0 {
            (total_saved as f64 / total_base as f64) * 100.0
        } else {
            0.0
        };
        let (sonnet_usd, opus_usd) = estimate_usd_saved(total_saved as usize);

        Ok(TokenStatsSummary {
            total_queries: total_queries as usize,
            total_tokens_sent: total_sent as usize,
            total_baseline_tokens: total_base as usize,
            total_tokens_saved: total_saved as usize,
            savings_percent,
            est_usd_saved_sonnet: sonnet_usd,
            est_usd_saved_opus: opus_usd,
            by_tool,
            recent,
        })
    }

    pub fn clear_telemetry(&self) -> Result<()> {
        self.conn.execute("DELETE FROM telemetry", [])?;
        Ok(())
    }

    pub fn add_note(&self, target: &str, content: &str) -> Result<i64> {
        let ts = Utc::now().to_rfc3339();
        let clean_target = if target.trim().is_empty() {
            "project"
        } else {
            target.trim()
        };
        self.conn.execute(
            "INSERT INTO notes (target, content, created_at) VALUES (?1, ?2, ?3)",
            params![clean_target, content.trim(), ts],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn list_notes(&self, filter: Option<&str>) -> Result<Vec<ProjectNote>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, target, content, created_at FROM notes ORDER BY id DESC LIMIT 100",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ProjectNote {
                    id: r.get(0)?,
                    target: r.get(1)?,
                    content: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        if let Some(q) = filter {
            let ql = q.trim().to_ascii_lowercase();
            if !ql.is_empty() {
                return Ok(rows
                    .into_iter()
                    .filter(|n| {
                        n.target.to_ascii_lowercase().contains(&ql)
                            || n.content.to_ascii_lowercase().contains(&ql)
                    })
                    .collect());
            }
        }
        Ok(rows)
    }

    pub fn find_notes_for_targets(&self, targets: &[&str]) -> Result<Vec<ProjectNote>> {
        let all = self.list_notes(None)?;
        let lower_targets: Vec<String> = targets.iter().map(|t| t.to_ascii_lowercase()).collect();
        Ok(all
            .into_iter()
            .filter(|n| {
                let nt = n.target.to_ascii_lowercase();
                lower_targets.iter().any(|t| nt == *t || t.contains(&nt) || nt.contains(t))
            })
            .collect())
    }

    pub fn delete_note(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM notes WHERE id = ?1", params![id])?;
        Ok(())
    }
}
