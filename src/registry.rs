use crate::db::Database;
use crate::model::TokenStatsSummary;
use crate::tokens::estimate_usd_saved;
use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub path: String,
    pub name: String,
    pub added_at: String,
    pub last_seen: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RegistryFile {
    projects: Vec<ProjectEntry>,
}

/// Live summary of a registered project loaded from its local `.trackertop/index.db`.
#[derive(Debug, Clone)]
pub struct ProjectSnapshot {
    pub entry: ProjectEntry,
    pub exists_on_disk: bool,
    pub indexed: bool,
    pub total_files: usize,
    pub total_symbols: usize,
    pub total_edges: usize,
    pub total_repo_tokens: usize,
    pub stats: TokenStatsSummary,
}

/// Global aggregate across all registered projects.
#[derive(Debug, Clone, Default)]
pub struct GlobalOverview {
    pub projects: Vec<ProjectSnapshot>,
    pub total_files: usize,
    pub total_symbols: usize,
    pub total_repo_tokens: usize,
    pub total_queries: usize,
    pub total_tokens_sent: usize,
    pub total_baseline_tokens: usize,
    pub total_tokens_saved: usize,
    pub savings_percent: f64,
    pub est_usd_saved_sonnet: f64,
    pub est_usd_saved_opus: f64,
}

/// Returns `%USERPROFILE%\.trackertop\projects.json` (Windows) or `$HOME/.trackertop/projects.json` (Unix).
pub fn registry_file_path() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)?;
    Some(home.join(".trackertop").join("projects.json"))
}

fn canonical_path_string(path: &Path) -> String {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = canon.to_string_lossy().to_string();
    // Strip Windows extended-length prefix `\\?\` for clean display and dedup
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

fn is_temp_path(path: &Path) -> bool {
    let tmp = canonical_path_string(&std::env::temp_dir()).to_ascii_lowercase();
    let cur = canonical_path_string(path).to_ascii_lowercase();
    cur.starts_with(&tmp)
}

fn load_registry_file() -> RegistryFile {
    let Some(path) = registry_file_path() else {
        return RegistryFile::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return RegistryFile::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_registry_file(reg: &RegistryFile) -> Result<()> {
    let Some(path) = registry_file_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(reg)?;
    std::fs::write(&path, json)?;
    Ok(())
}

/// Registers (or updates `last_seen` for) a repository in the user's global project list.
/// Skips temporary directories created during automated tests.
pub fn register_project(repo_root: &Path) -> Result<()> {
    if is_temp_path(repo_root) {
        return Ok(());
    }
    let canon = canonical_path_string(repo_root);
    let now = Utc::now().to_rfc3339();
    let name = Path::new(&canon)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Project")
        .to_string();

    let mut reg = load_registry_file();
    if let Some(existing) = reg
        .projects
        .iter_mut()
        .find(|p| p.path.eq_ignore_ascii_case(&canon))
    {
        existing.last_seen = now;
        existing.name = name;
    } else {
        reg.projects.push(ProjectEntry {
            path: canon,
            name,
            added_at: now.clone(),
            last_seen: now,
        });
    }
    save_registry_file(&reg)
}

pub fn remove_project(repo_path: &str) -> Result<()> {
    let mut reg = load_registry_file();
    reg.projects
        .retain(|p| !p.path.eq_ignore_ascii_case(repo_path));
    save_registry_file(&reg)
}

/// Removes a project from the list and optionally deletes its local `.trackertop/` cache folder.
pub fn remove_project_with_cache(repo_path: &str, delete_cache: bool) -> Result<()> {
    remove_project(repo_path)?;
    if delete_cache {
        let cache_dir = Path::new(repo_path).join(".trackertop");
        if cache_dir.exists() {
            let _ = std::fs::remove_dir_all(cache_dir);
        }
    }
    Ok(())
}

pub fn list_projects() -> Vec<ProjectEntry> {
    let mut reg = load_registry_file();
    reg.projects.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    reg.projects
}

/// Loads live metrics & token savings across every registered project.
pub fn load_global_overview() -> GlobalOverview {
    let entries = list_projects();
    let mut overview = GlobalOverview::default();

    for entry in entries {
        let p = PathBuf::from(&entry.path);
        let exists = p.exists();
        let db_path = p.join(".trackertop").join("index.db");
        let mut snap = ProjectSnapshot {
            entry,
            exists_on_disk: exists,
            indexed: false,
            total_files: 0,
            total_symbols: 0,
            total_edges: 0,
            total_repo_tokens: 0,
            stats: TokenStatsSummary::default(),
        };

        if exists && db_path.exists() {
            if let Ok(db) = Database::open_path(&db_path) {
                snap.indexed = true;
                if let Ok(files) = db.load_all_files() {
                    snap.total_files = files.len();
                    snap.total_repo_tokens = files.iter().map(|f| f.token_count).sum();
                }
                if let Ok(syms) = db.load_all_symbols() {
                    snap.total_symbols = syms.len();
                }
                if let Ok(edges) = db.load_all_edges() {
                    snap.total_edges = edges.len();
                }
                if let Ok(stats) = db.get_token_stats(15) {
                    snap.stats = stats;
                }
            }
        }

        overview.total_files += snap.total_files;
        overview.total_symbols += snap.total_symbols;
        overview.total_repo_tokens += snap.total_repo_tokens;
        overview.total_queries += snap.stats.total_queries;
        overview.total_tokens_sent += snap.stats.total_tokens_sent;
        overview.total_baseline_tokens += snap.stats.total_baseline_tokens;
        overview.total_tokens_saved += snap.stats.total_tokens_saved;
        overview.projects.push(snap);
    }

    overview.savings_percent = if overview.total_baseline_tokens > 0 {
        (overview.total_tokens_saved as f64 / overview.total_baseline_tokens as f64) * 100.0
    } else {
        0.0
    };
    let (sonnet, opus) = estimate_usd_saved(overview.total_tokens_saved);
    overview.est_usd_saved_sonnet = sonnet;
    overview.est_usd_saved_opus = opus;
    overview
}
