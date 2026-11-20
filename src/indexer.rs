use crate::db::Database;
use crate::model::{Edge, EdgeKind, FileRecord, RawReference, Symbol, SymbolKind};
use crate::parser::{LanguageExtractor, LanguageRegistry, default_registry, file_symbol_id};
use crate::registry::register_project;
use crate::tokens::count_tokens;
use anyhow::{Result, bail};
use ignore::WalkBuilder;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct IndexStats {
    pub total_files: usize,
    pub parsed_files: usize,
    pub skipped_files: usize,
    pub deleted_files: usize,
    pub total_symbols: usize,
    pub total_edges: usize,
    pub total_repo_tokens: usize,
    /// `Database::index_generation` after this run; changes whenever the graph data changed.
    pub generation: i64,
    pub elapsed_ms: u128,
}

/// Refuses to index the user's home directory or a filesystem root.
///
/// A globally registered MCP server has no `--repo` and indexes its working directory, so
/// an AI agent started from `~` (or `C:\`) would otherwise walk the entire disk and drop a
/// `.trackertop/` folder there.
pub fn ensure_indexable_root(repo_root: &Path) -> Result<()> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    check_indexable_root(repo_root, home.as_deref())
}

fn check_indexable_root(repo_root: &Path, home: Option<&Path>) -> Result<()> {
    let canon = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    // Show `C:\Users\me`, not the Windows-internal `\\?\C:\Users\me`.
    let shown = display_path(&canon);
    let hint = "Open your AI agent inside a project folder, or pass `--repo <project-path>`.";
    let Some(parent) = canon.parent() else {
        bail!("TrackerTop refuses to index `{shown}` because it is a filesystem root. {hint}");
    };
    if let Some(home) = home {
        let home_canon = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        if home_canon == canon {
            bail!(
                "TrackerTop refuses to index `{shown}` because it is your home directory. {hint}"
            );
        }
        // Catch-all user folders where terminals are often opened by accident. Only the folder
        // itself is refused (projects *inside* Desktop etc. are fine), and only when it is not
        // a git repository on its own. Name comparison only: no extra filesystem calls on the
        // hot path (this runs on every DB open).
        let under_home = parent == home_canon || parent == home_canon.join("OneDrive");
        if under_home && let Some(dir_name) = canon.file_name().and_then(|n| n.to_str()) {
            let user_folder = ["Desktop", "Downloads", "Documents"]
                .into_iter()
                .find(|n| n.eq_ignore_ascii_case(dir_name));
            if let Some(name) = user_folder
                && !canon.join(".git").exists()
            {
                bail!(
                    "TrackerTop refuses to index `{shown}` because it is your {name} folder, \
                     not a project. {hint}"
                );
            }
        }
    }
    Ok(())
}

fn display_path(path: &Path) -> String {
    let s = path.display().to_string();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

/// Runs an incremental index of `repo_root` using the default `LanguageRegistry`.
/// Only new or changed files (detected via mtime + blake3 hash) are re-parsed.
pub fn index_repository(repo_root: &Path, force: bool) -> Result<IndexStats> {
    index_repository_with_registry(repo_root, force, default_registry())
}

/// Runs an incremental index using any custom `LanguageRegistry` (open core for community languages).
pub fn index_repository_with_registry(
    repo_root: &Path,
    force: bool,
    registry: &LanguageRegistry,
) -> Result<IndexStats> {
    let start = Instant::now();
    ensure_indexable_root(repo_root)?;
    let _ = register_project(repo_root);
    let mut db = Database::open_for_repo(repo_root)?;
    let existing = if force {
        HashMap::new()
    } else {
        db.load_file_map()?
    };

    let discovered = discover_supported_files(repo_root, registry);
    let current_paths: HashSet<String> = discovered.iter().map(|(rel, _, _)| rel.clone()).collect();

    // Detect deleted files
    let deleted: Vec<String> = existing
        .keys()
        .filter(|p| !current_paths.contains(*p))
        .cloned()
        .collect();

    // Determine which files need parsing (in parallel)
    struct Candidate {
        rel_path: String,
        abs_path: PathBuf,
        extractor: Arc<dyn LanguageExtractor>,
        mtime: i64,
        size: u64,
    }

    let mut candidates = Vec::new();
    let mut skipped_files = 0usize;

    for (rel_path, abs_path, extractor) in discovered {
        let meta = match std::fs::metadata(&abs_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let size = meta.len();
        // Skip unusually massive generated files (> 2 MB) to keep indexing snappy
        if size > 2 * 1024 * 1024 {
            continue;
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        if let Some(prev) = existing.get(&rel_path)
            && prev.mtime_secs == mtime
            && prev.size_bytes == size
        {
            skipped_files += 1;
            continue;
        }
        candidates.push(Candidate {
            rel_path,
            abs_path,
            extractor,
            mtime,
            size,
        });
    }

    // Read, hash, and parse changed candidates in parallel with rayon
    let parsed_batch: Vec<(FileRecord, Vec<Symbol>, Vec<RawReference>)> = candidates
        .into_par_iter()
        .filter_map(|cand| {
            let source = std::fs::read_to_string(&cand.abs_path).ok()?;
            let hash = blake3::hash(source.as_bytes()).to_hex().to_string();

            if let Some(prev) = existing.get(&cand.rel_path)
                && prev.hash == hash
            {
                return None;
            }

            let parsed = cand.extractor.parse(&cand.rel_path, &source).ok()?;
            let line_count = source.lines().count().max(1);
            let token_count = count_tokens(&source);
            let rec = FileRecord {
                path: cand.rel_path,
                language: cand.extractor.id().to_string(),
                hash,
                mtime_secs: cand.mtime,
                size_bytes: cand.size,
                line_count,
                token_count,
            };
            Some((rec, parsed.symbols, parsed.refs))
        })
        .collect();

    let parsed_files = parsed_batch.len();
    let deleted_files = deleted.len();
    let changed = force || parsed_files > 0 || deleted_files > 0 || existing.is_empty();

    if deleted_files > 0 {
        db.delete_files(&deleted)?;
    }
    if parsed_files > 0 {
        db.upsert_parsed_files(&parsed_batch)?;
    }

    let (total_files, total_symbols, total_edges, total_repo_tokens, generation) = if changed {
        let all_files = db.load_all_files()?;
        let all_symbols = db.load_all_symbols()?;
        let all_refs = db.load_all_raw_refs()?;
        let mut resolved = resolve_edges(&all_files, &all_symbols, &all_refs);
        let indexed_paths: HashSet<String> = all_files.iter().map(|f| f.path.clone()).collect();
        resolved.extend(crate::git::extract_cochange_edges(
            repo_root,
            &indexed_paths,
        ));
        let edge_count = resolved.len();
        db.replace_edges(&resolved)?;
        let generation = db.bump_index_generation()?;
        let tokens: usize = all_files.iter().map(|f| f.token_count).sum();
        (
            all_files.len(),
            all_symbols.len(),
            edge_count,
            tokens,
            generation,
        )
    } else {
        // Nothing changed: aggregate in SQL instead of loading every row.
        let (files, symbols, edges, tokens) = db.index_totals()?;
        (files, symbols, edges, tokens, db.index_generation()?)
    };

    Ok(IndexStats {
        total_files,
        parsed_files,
        skipped_files,
        deleted_files,
        total_symbols,
        total_edges,
        total_repo_tokens,
        generation,
        elapsed_ms: start.elapsed().as_millis(),
    })
}

/// Discovers supported files respecting `.gitignore`, skipping `.trackertop/` and `.git/`.
fn discover_supported_files(
    repo_root: &Path,
    registry: &LanguageRegistry,
) -> Vec<(String, PathBuf, Arc<dyn LanguageExtractor>)> {
    let mut results = Vec::new();
    let walker = WalkBuilder::new(repo_root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                ".git" | ".trackertop" | "target" | "node_modules" | ".venv" | "dist" | "build"
            )
        })
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(extractor) = registry.extractor_for_path(path) else {
            continue;
        };
        let Ok(rel) = path.strip_prefix(repo_root) else {
            continue;
        };
        let norm = normalize_rel_path(rel);
        // Skip generated report artifacts if placed inside repo
        if norm.ends_with("GRAPH_REPORT.md") || norm.starts_with(".trackertop/") {
            continue;
        }
        results.push((norm, path.to_path_buf(), extractor));
    }
    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}

pub fn normalize_rel_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string()
}

/// Resolves raw intra-file and cross-file references into directed graph edges.
fn resolve_edges(
    files: &[FileRecord],
    symbols: &[Symbol],
    raw_refs: &[(String, RawReference)],
) -> Vec<Edge> {
    let mut edge_map: HashMap<(String, String, EdgeKind), f32> = HashMap::new();

    // 1. Structural CONTAINS edges from parent_id
    for sym in symbols {
        if let Some(ref pid) = sym.parent_id {
            edge_map.insert((pid.clone(), sym.id.clone(), EdgeKind::Contains), 1.0);
        }
    }

    // Lookup indexes for fast resolution
    let mut file_by_path: HashMap<String, String> = HashMap::new();
    let mut file_by_basename: HashMap<String, Vec<String>> = HashMap::new();
    let mut file_by_stem: HashMap<String, Vec<String>> = HashMap::new();

    for f in files {
        let fid = file_symbol_id(&f.path);
        file_by_path.insert(f.path.clone(), fid.clone());
        if let Some(base) = f.path.rsplit('/').next() {
            file_by_basename
                .entry(base.to_ascii_lowercase())
                .or_default()
                .push(fid.clone());
            let stem = base.split('.').next().unwrap_or(base).to_ascii_lowercase();
            file_by_stem.entry(stem).or_default().push(fid.clone());
        }
    }

    let mut by_qname: HashMap<String, Vec<&Symbol>> = HashMap::new();
    let mut by_name: HashMap<String, Vec<&Symbol>> = HashMap::new();
    let mut by_name_ci: HashMap<String, Vec<&Symbol>> = HashMap::new();

    for sym in symbols {
        if sym.kind == SymbolKind::File || sym.kind == SymbolKind::Impl {
            continue;
        }
        by_qname
            .entry(sym.qualified_name.clone())
            .or_default()
            .push(sym);
        by_name.entry(sym.name.clone()).or_default().push(sym);
        by_name_ci
            .entry(sym.name.to_ascii_lowercase())
            .or_default()
            .push(sym);
    }

    // 2. Resolve each raw reference
    for (src_file, rref) in raw_refs {
        let target = rref.target_name.trim();
        if target.is_empty() {
            continue;
        }

        let mut resolved_targets: Vec<String> = Vec::new();

        match rref.kind {
            EdgeKind::Imports => {
                // a) Rust `mod::foo` -> try `<dir>/foo.rs` or `<dir>/foo/mod.rs`
                if let Some(mod_name) = target.strip_prefix("mod::") {
                    let dir = parent_dir(src_file);
                    let candidates = [
                        join_rel(&dir, &format!("{mod_name}.rs")),
                        join_rel(&dir, &format!("{mod_name}/mod.rs")),
                        format!("src/{mod_name}.rs"),
                        format!("src/{mod_name}/mod.rs"),
                    ];
                    for c in &candidates {
                        if let Some(fid) = file_by_path.get(c) {
                            resolved_targets.push(fid.clone());
                            break;
                        }
                    }
                }
                // b) Rust `crate::a::b::Sym`
                else if let Some(rest) = target
                    .strip_prefix("crate::")
                    .or_else(|| target.strip_prefix("super::"))
                {
                    let segs: Vec<&str> = rest.split("::").collect();
                    // Try symbol resolution on last segment
                    if let Some(&last) = segs.last()
                        && let Some(syms) = by_name.get(last)
                        && let Some(best) = pick_best_symbol(syms, src_file, &segs)
                    {
                        resolved_targets.push(best.id.clone());
                    }
                    // Also link to module file if found
                    if !segs.is_empty() {
                        let mod_path = format!("src/{}.rs", segs[0]);
                        let mod_dir_path = format!("src/{}/mod.rs", segs[0]);
                        if let Some(fid) = file_by_path
                            .get(&mod_path)
                            .or_else(|| file_by_path.get(&mod_dir_path))
                        {
                            resolved_targets.push(fid.clone());
                        }
                        if segs.len() >= 2 {
                            let sub = format!("src/{}/{}.rs", segs[0], segs[1]);
                            if let Some(fid) = file_by_path.get(&sub) {
                                resolved_targets.push(fid.clone());
                            }
                        }
                    }
                }
                // c) File path import (HTML script/link, PS dot-source, WGSL import)
                else {
                    let norm = target
                        .replace('\\', "/")
                        .trim_start_matches("./")
                        .to_string();
                    let rel_from_dir = join_rel(&parent_dir(src_file), &norm);
                    if let Some(fid) = file_by_path
                        .get(&norm)
                        .or_else(|| file_by_path.get(&rel_from_dir))
                    {
                        resolved_targets.push(fid.clone());
                    } else if let Some(base) = norm.rsplit('/').next() {
                        let key = base.to_ascii_lowercase();
                        if let Some(fids) = file_by_basename
                            .get(&key)
                            .or_else(|| file_by_stem.get(&key))
                            && let Some(first) = fids.first()
                        {
                            resolved_targets.push(first.clone());
                        }
                    }
                }
            }
            EdgeKind::Calls
            | EdgeKind::Tests
            | EdgeKind::Implements
            | EdgeKind::UsesType
            | EdgeKind::References => {
                // Direct file path reference (e.g., from Markdown `src/graph.rs`)
                if let Some(fid) = file_by_path.get(target) {
                    resolved_targets.push(fid.clone());
                } else if let Some(syms) = by_qname.get(target) {
                    if let Some(best) = pick_nearest_symbol(syms, src_file) {
                        resolved_targets.push(best.id.clone());
                    }
                } else if let Some(syms) = by_name.get(target) {
                    if let Some(best) = pick_nearest_symbol(syms, src_file) {
                        resolved_targets.push(best.id.clone());
                    }
                } else if let Some(last) = target.rsplit("::").next() {
                    if let Some(syms) = by_name.get(last) {
                        if let Some(best) = pick_nearest_symbol(syms, src_file) {
                            resolved_targets.push(best.id.clone());
                        }
                    } else if let Some(syms) = by_name_ci.get(&last.to_ascii_lowercase()) {
                        // PowerShell is case-insensitive
                        if let Some(best) = pick_nearest_symbol(syms, src_file) {
                            resolved_targets.push(best.id.clone());
                        }
                    }
                }
            }
            EdgeKind::Contains | EdgeKind::CoChanges => {}
        }

        let weight = match rref.kind {
            EdgeKind::Calls => 1.5,
            EdgeKind::Implements => 1.8,
            EdgeKind::Imports => 1.2,
            EdgeKind::Tests => 1.0,
            EdgeKind::UsesType => 0.9,
            EdgeKind::References => 0.6,
            EdgeKind::Contains => 1.0,
            EdgeKind::CoChanges => 1.1,
        };

        for tgt in resolved_targets {
            if tgt == rref.source_symbol_id {
                continue;
            }
            edge_map
                .entry((rref.source_symbol_id.clone(), tgt, rref.kind))
                .and_modify(|w| *w = w.max(weight))
                .or_insert(weight);
        }
    }

    edge_map
        .into_iter()
        .map(|((source_id, target_id, kind), weight)| Edge {
            source_id,
            target_id,
            kind,
            weight,
        })
        .collect()
}

fn parent_dir(rel_path: &str) -> String {
    match rel_path.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn pick_nearest_symbol<'a>(syms: &[&'a Symbol], src_file: &str) -> Option<&'a Symbol> {
    if syms.len() == 1 {
        return Some(syms[0]);
    }
    // Prefer same file, then public symbols, then shortest path distance
    if let Some(same) = syms.iter().find(|s| s.file_path == src_file) {
        return Some(*same);
    }
    if let Some(pub_sym) = syms.iter().find(|s| s.visibility.starts_with("pub")) {
        return Some(*pub_sym);
    }
    syms.first().copied()
}

fn pick_best_symbol<'a>(syms: &[&'a Symbol], src_file: &str, segs: &[&str]) -> Option<&'a Symbol> {
    if segs.len() >= 2 {
        let hint = segs[segs.len() - 2];
        if let Some(matched) = syms.iter().find(|s| s.file_path.contains(hint)) {
            return Some(*matched);
        }
    }
    pick_nearest_symbol(syms, src_file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_home_directory_and_filesystem_root() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        assert!(check_indexable_root(home.path(), Some(home.path())).is_err());
        assert!(check_indexable_root(&project, Some(home.path())).is_ok());

        let root = home.path().ancestors().last().unwrap();
        assert!(check_indexable_root(root, None).is_err());
    }

    #[test]
    fn refuses_bare_desktop_but_allows_projects_inside_it() {
        let home = tempfile::tempdir().unwrap();
        let desktop = home.path().join("Desktop");
        let od_downloads = home.path().join("OneDrive").join("Downloads");
        let project = desktop.join("MyApp");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&od_downloads).unwrap();

        assert!(check_indexable_root(&desktop, Some(home.path())).is_err());
        assert!(check_indexable_root(&od_downloads, Some(home.path())).is_err());
        assert!(check_indexable_root(&project, Some(home.path())).is_ok());

        // A Desktop that is itself a git repository is a deliberate choice
        std::fs::create_dir_all(desktop.join(".git")).unwrap();
        assert!(check_indexable_root(&desktop, Some(home.path())).is_ok());
    }
}
