use crate::db::Database;
use crate::fsutil::{FileLock, atomic_write};
use crate::model::TokenStatsSummary;
use crate::tokens::estimate_usd_saved;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// The key a project is stored under in `projects.json`. Anything that needs to find a
/// project again (e.g. the UI selecting a freshly indexed one) must use this same function.
pub fn canonical_path_string(path: &Path) -> String {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    // Strip the Windows verbatim prefix (keeping UNC shares intact) for display and dedup
    crate::fsutil::display_path(&canon)
}

fn is_temp_path(path: &Path) -> bool {
    let tmp = canonical_path_string(&std::env::temp_dir()).to_ascii_lowercase();
    let cur = canonical_path_string(path).to_ascii_lowercase();
    cur.starts_with(&tmp)
}

/// How long to wait for another TrackerTop process holding the registry lock.
const REGISTRY_LOCK_WAIT: Duration = Duration::from_secs(3);
/// An already-registered project only gets `last_seen` rewritten this often, so the
/// per-query re-index of every MCP call does not rewrite `projects.json` each time.
const LAST_SEEN_REFRESH_SECS: i64 = 300;

/// Project paths are case-insensitive on Windows and (by default) macOS, but two directories
/// differing only in case are different projects on Linux.
fn same_project_path(a: &str, b: &str) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Raw registry text. Missing file -> `Ok(None)`. Any other I/O failure (permission or
/// sharing violation, ...) is an `Err` and must NOT be mistaken for corruption.
fn read_registry_text(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Parses registry text. Empty text -> empty registry; unparseable -> `None` (corrupted).
fn parse_registry(text: &str) -> Option<RegistryFile> {
    if text.trim().is_empty() {
        return Some(RegistryFile::default());
    }
    let mut reg: RegistryFile = serde_json::from_str(text).ok()?;
    migrate_legacy_keys(&mut reg);
    Some(reg)
}

/// v0.5.0 stored network-share projects as `UNC\server\share` (it only stripped `\\?\`);
/// the key is now `\\server\share`. Rewrite old keys and, if both forms ended up in the
/// file, keep the most recently seen entry so a project never shows up twice.
fn migrate_legacy_keys(reg: &mut RegistryFile) {
    for p in &mut reg.projects {
        if let Some(rest) = p.path.strip_prefix(r"UNC\") {
            p.path = format!(r"\\{rest}");
        }
    }
    let mut kept: Vec<ProjectEntry> = Vec::with_capacity(reg.projects.len());
    for p in reg.projects.drain(..) {
        match kept
            .iter_mut()
            .find(|k| same_project_path(&k.path, &p.path))
        {
            Some(k) => {
                // Keep the most recent entry, but never lose when the project was first added
                let first_added = std::cmp::min(k.added_at.clone(), p.added_at.clone());
                if p.last_seen > k.last_seen {
                    *k = p;
                }
                k.added_at = first_added;
            }
            None => kept.push(p),
        }
    }
    reg.projects = kept;
}

/// Reads the registry. Missing/empty file -> empty registry; I/O error or corruption -> `Err`.
fn read_registry_at(path: &Path) -> Result<RegistryFile> {
    match read_registry_text(path)? {
        None => Ok(RegistryFile::default()),
        Some(text) => {
            parse_registry(&text).with_context(|| format!("{} is corrupted", path.display()))
        }
    }
}

/// Unused `projects.json.corrupt-<timestamp>[-n]` name next to `path`.
fn corrupt_backup_path(path: &Path) -> PathBuf {
    let stamp = Utc::now().format("%Y%m%d%H%M%S%3f");
    let mut candidate = path.with_file_name(format!("projects.json.corrupt-{stamp}"));
    let mut n = 1;
    while candidate.exists() {
        candidate = path.with_file_name(format!("projects.json.corrupt-{stamp}-{n}"));
        n += 1;
    }
    candidate
}

/// Lenient read for display purposes only (never used as the base for a write).
fn load_registry_file() -> RegistryFile {
    registry_file_path()
        .and_then(|p| read_registry_at(&p).ok())
        .unwrap_or_default()
}

/// Locked read-modify-write of the registry at `path`. `mutate` returns whether anything
/// changed. An unparseable file is moved aside to `projects.json.corrupt-<timestamp>` (kept
/// for manual recovery) instead of being silently overwritten. An I/O error while reading
/// (e.g. antivirus briefly holding the file) aborts the update and leaves the file alone.
fn update_registry_at(path: &Path, mutate: impl FnOnce(&mut RegistryFile) -> bool) -> Result<()> {
    let _lock = FileLock::acquire(path, REGISTRY_LOCK_WAIT)?;
    let parsed = read_registry_text(path)?.map(|text| parse_registry(&text));
    let mut reg = match parsed {
        None => RegistryFile::default(),
        Some(Some(r)) => r,
        Some(None) => {
            let aside = corrupt_backup_path(path);
            std::fs::rename(path, &aside).with_context(|| {
                format!(
                    "{} is corrupted and could not be moved aside",
                    path.display()
                )
            })?;
            RegistryFile::default()
        }
    };
    if mutate(&mut reg) {
        atomic_write(path, serde_json::to_string_pretty(&reg)?.as_bytes())?;
    }
    Ok(())
}

fn is_recently_seen(entry: &ProjectEntry) -> bool {
    DateTime::parse_from_rfc3339(&entry.last_seen)
        .map(|t| (Utc::now() - t.with_timezone(&Utc)).num_seconds() < LAST_SEEN_REFRESH_SECS)
        .unwrap_or(false)
}

fn upsert_project(reg: &mut RegistryFile, canon: &str) -> bool {
    let name = Path::new(canon)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Project")
        .to_string();
    let now = Utc::now().to_rfc3339();
    if let Some(existing) = reg
        .projects
        .iter_mut()
        .find(|p| same_project_path(&p.path, canon))
    {
        if is_recently_seen(existing) && existing.name == name {
            return false;
        }
        existing.last_seen = now;
        existing.name = name;
    } else {
        reg.projects.push(ProjectEntry {
            path: canon.to_string(),
            name,
            added_at: now.clone(),
            last_seen: now,
        });
    }
    true
}

/// Registers (or updates `last_seen` for) a repository in the user's global project list.
/// Skips temporary directories created during automated tests.
pub fn register_project(repo_root: &Path) -> Result<()> {
    if is_temp_path(repo_root) {
        return Ok(());
    }
    let Some(path) = registry_file_path() else {
        return Ok(());
    };
    let canon = canonical_path_string(repo_root);

    // Lock-free fast path: nothing to do when the project was seen moments ago.
    if let Ok(reg) = read_registry_at(&path)
        && reg
            .projects
            .iter()
            .any(|p| same_project_path(&p.path, &canon) && is_recently_seen(p))
    {
        return Ok(());
    }
    update_registry_at(&path, |reg| upsert_project(reg, &canon))
}

pub fn remove_project(repo_path: &str) -> Result<()> {
    let Some(path) = registry_file_path() else {
        return Ok(());
    };
    update_registry_at(&path, |reg| {
        let before = reg.projects.len();
        reg.projects
            .retain(|p| !same_project_path(&p.path, repo_path));
        reg.projects.len() != before
    })
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

        if exists
            && db_path.exists()
            && let Ok(db) = Database::open_path(&db_path)
        {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupted_registry_is_moved_aside_not_silently_lost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("projects.json");
        std::fs::write(&path, "{\"projects\": [ {\"path\": \"C:/a\"").unwrap();

        update_registry_at(&path, |reg| upsert_project(reg, "C:/work/repo")).unwrap();

        let reg = read_registry_at(&path).unwrap();
        assert_eq!(reg.projects.len(), 1);
        let backups = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .count();
        assert_eq!(backups, 1);
    }

    #[test]
    fn concurrent_registrations_do_not_lose_projects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("projects.json");
        std::thread::scope(|s| {
            for i in 0..16 {
                let path = &path;
                s.spawn(move || {
                    update_registry_at(path, |reg| upsert_project(reg, &format!("C:/repo{i}")))
                        .unwrap();
                });
            }
        });
        assert_eq!(read_registry_at(&path).unwrap().projects.len(), 16);
    }

    #[test]
    fn recently_seen_project_is_not_rewritten() {
        let mut reg = RegistryFile::default();
        assert!(upsert_project(&mut reg, "C:/work/repo"));
        assert!(!upsert_project(&mut reg, "C:/work/repo"));
    }

    #[test]
    fn read_errors_are_not_treated_as_corruption() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be: reading fails with an I/O error, not NotFound
        let path = dir.path().join("projects.json");
        std::fs::create_dir(&path).unwrap();

        assert!(update_registry_at(&path, |reg| upsert_project(reg, "C:/work/repo")).is_err());
        assert!(path.is_dir(), "registry must be left untouched");
        let moved_aside = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".corrupt-"));
        assert!(!moved_aside);
    }

    #[test]
    fn repeated_corruption_backups_never_overwrite_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("projects.json");
        for _ in 0..3 {
            std::fs::write(&path, "{ not json").unwrap();
            update_registry_at(&path, |reg| upsert_project(reg, "C:/work/repo")).unwrap();
        }
        let backups = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .count();
        assert_eq!(backups, 3);
    }

    #[test]
    fn path_case_sensitivity_follows_the_platform() {
        assert!(same_project_path("C:/Work/Repo", "C:/Work/Repo"));
        assert_eq!(
            same_project_path("/home/me/Repo", "/home/me/repo"),
            cfg!(any(windows, target_os = "macos"))
        );
    }

    #[test]
    fn legacy_unc_keys_are_migrated_and_deduplicated() {
        let text = r#"{"projects":[
            {"path":"UNC\\srv\\share\\app","name":"app","added_at":"2026-09-01T00:00:00+00:00","last_seen":"2026-11-01T00:00:00+00:00"},
            {"path":"\\\\srv\\share\\app","name":"app","added_at":"2026-11-02T00:00:00+00:00","last_seen":"2026-11-02T00:00:00+00:00"},
            {"path":"C:\\work\\other","name":"other","added_at":"2026-10-01T00:00:00+00:00","last_seen":"2026-11-01T00:00:00+00:00"}
        ]}"#;
        let reg = parse_registry(text).unwrap();
        let paths: Vec<&str> = reg.projects.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(paths, vec![r"\\srv\share\app", r"C:\work\other"]);
        // Newest activity wins, but the original "added" date survives the merge
        assert_eq!(reg.projects[0].last_seen, "2026-11-02T00:00:00+00:00");
        assert_eq!(reg.projects[0].added_at, "2026-09-01T00:00:00+00:00");
    }
}
