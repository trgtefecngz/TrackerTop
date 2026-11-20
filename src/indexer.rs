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

/// Revision of what the extractors produce (symbols, raw references). Bump it whenever an
/// extractor change alters their output for unchanged source: an index written by another
/// revision is re-parsed in full on its next run, instead of keeping stale edges for
/// every file whose content did not change. (1: v0.8, Rust macro-argument and method calls;
/// 2: Rust type uses inside function bodies; 3: Python on tree-sitter;
/// 4: Go on tree-sitter; 5: Python/Go syntax-error recovery.)
pub const EXTRACTOR_REVISION: i64 = 5;

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

use crate::fsutil::display_path;

/// Sentinel stored instead of a real mtime when it cannot be trusted; it never equals a real
/// file's mtime, so the next run re-reads (and re-hashes) that file.
const UNTRUSTED_MTIME: i64 = 0;

/// The mtime to record for a file that was stat'ed (`mtime`, `size`) and then read
/// (`read_len` bytes). Returns [`UNTRUSTED_MTIME`] when the fast path could later skip a real
/// change ("racy" files, same idea as git's racy-git handling):
/// - the file changed while it was being read (size/mtime differ afterwards, or the bytes
///   read do not match the stat'ed size): the stored hash may describe a half-written file;
/// - its mtime is not at least a second in the past (git's racy rule): a further same-size
///   edit in the same second would keep the same (second-resolution) mtime and size and be
///   skipped forever. Future mtimes (clock skew, archives) are included; they are simply
///   re-hashed until they are in the past, which costs a hash but can never miss an edit.
fn trustworthy_mtime(abs_path: &Path, mtime: i64, size: u64, read_len: u64) -> i64 {
    if read_len != size {
        return UNTRUSTED_MTIME;
    }
    let Ok(after) = std::fs::metadata(abs_path) else {
        return UNTRUSTED_MTIME;
    };
    let mtime_after = after
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(UNTRUSTED_MTIME);
    if after.len() != size || mtime_after != mtime {
        return UNTRUSTED_MTIME;
    }
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if mtime >= now - 1 {
        return UNTRUSTED_MTIME;
    }
    mtime
}

/// Runs an incremental index of `repo_root` using the default `LanguageRegistry`.
/// Only new or changed files (detected via mtime + blake3 hash) are re-parsed.
pub fn index_repository(repo_root: &Path, force: bool) -> Result<IndexStats> {
    index_repository_with_registry(repo_root, force, default_registry())
}

/// Indexes a temporary snapshot (for example a review's base commit checked out in a
/// worktree) without adding it to the global project list shown in the desktop app.
pub fn index_snapshot(root: &Path) -> Result<IndexStats> {
    index_with(root, false, default_registry(), false)
}

/// Runs an incremental index using any custom `LanguageRegistry` (open core for community languages).
pub fn index_repository_with_registry(
    repo_root: &Path,
    force: bool,
    registry: &LanguageRegistry,
) -> Result<IndexStats> {
    index_with(repo_root, force, registry, true)
}

fn index_with(
    repo_root: &Path,
    force: bool,
    registry: &LanguageRegistry,
    register: bool,
) -> Result<IndexStats> {
    let start = Instant::now();
    ensure_indexable_root(repo_root)?;
    if register {
        let _ = register_project(repo_root);
    }
    let mut db = Database::open_for_repo(repo_root)?;
    // Rows parsed by another extractor revision describe the code as that extractor saw
    // it: re-parse everything, or unchanged files keep missing (or wrong) edges forever.
    let outdated = db.extractor_revision()? != EXTRACTOR_REVISION;
    let reparse_all = force || outdated;
    // Loaded even when re-parsing everything, so deleted files are still detected.
    let existing = db.load_file_map()?;

    // A snapshot lives under the project's git-ignored `.trackertop/`: ignore rules from
    // directories above it would hide every file, so only its own `.gitignore`s apply.
    let discovered = discover_supported_files(repo_root, registry, register);
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

        // Never take the fast path on an untrusted record: an mtime that cannot be read here
        // falls back to the same value as the UNTRUSTED_MTIME sentinel and would otherwise
        // "match" it.
        if !reparse_all
            && let Some(prev) = existing.get(&rel_path)
            && prev.mtime_secs != UNTRUSTED_MTIME
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

    enum Outcome {
        Parsed(FileRecord, Vec<Symbol>, Vec<RawReference>),
        /// Timestamp changed but content did not (e.g. `git checkout`, editor save without
        /// edits): only the stored mtime/size need refreshing, otherwise the file would be
        /// re-read and re-hashed on every later run.
        Touched(String, i64, u64),
    }

    // Read, hash, and parse changed candidates in parallel with rayon
    let outcomes: Vec<Outcome> = candidates
        .into_par_iter()
        .filter_map(|cand| {
            let source = std::fs::read_to_string(&cand.abs_path).ok()?;
            let hash = blake3::hash(source.as_bytes()).to_hex().to_string();
            let recorded_mtime =
                trustworthy_mtime(&cand.abs_path, cand.mtime, cand.size, source.len() as u64);

            if !reparse_all
                && let Some(prev) = existing.get(&cand.rel_path)
                && prev.hash == hash
            {
                return Some(Outcome::Touched(cand.rel_path, recorded_mtime, cand.size));
            }

            let parsed = cand.extractor.parse(&cand.rel_path, &source).ok()?;
            let line_count = source.lines().count().max(1);
            let token_count = count_tokens(&source);
            let rec = FileRecord {
                path: cand.rel_path,
                language: cand.extractor.id().to_string(),
                hash,
                mtime_secs: recorded_mtime,
                size_bytes: cand.size,
                line_count,
                token_count,
            };
            Some(Outcome::Parsed(rec, parsed.symbols, parsed.refs))
        })
        .collect();

    let mut parsed_batch: Vec<(FileRecord, Vec<Symbol>, Vec<RawReference>)> = Vec::new();
    let mut touched: Vec<(String, i64, u64)> = Vec::new();
    for outcome in outcomes {
        match outcome {
            Outcome::Parsed(rec, syms, refs) => parsed_batch.push((rec, syms, refs)),
            Outcome::Touched(path, mtime, size) => touched.push((path, mtime, size)),
        }
    }
    if !touched.is_empty() {
        db.update_file_stats(&touched)?;
    }

    let parsed_files = parsed_batch.len();
    // A full re-parse must not keep rows it could not refresh (unreadable, too large or
    // unparsable now): they were produced by another extractor revision or are stale, and
    // the index is about to be marked current. Without rows the file reads as not indexed.
    let mut deleted = deleted;
    if reparse_all {
        let refreshed: HashSet<&str> = parsed_batch
            .iter()
            .map(|(r, _, _)| r.path.as_str())
            .collect();
        let gone: HashSet<&str> = deleted.iter().map(String::as_str).collect();
        let stale: Vec<String> = existing
            .keys()
            .filter(|p| !refreshed.contains(p.as_str()) && !gone.contains(p.as_str()))
            .cloned()
            .collect();
        deleted.extend(stale);
    }
    let deleted_files = deleted.len();

    if deleted_files > 0 {
        db.delete_files(&deleted)?;
    }
    if parsed_files > 0 {
        db.upsert_parsed_files(&parsed_batch)?;
    }

    // Edges lag behind the file rows when a previous run crashed or lost a race between its
    // upsert and its edge rebuild: repair them even if THIS run saw no file changes.
    let edges_stale = db.edges_generation()? != db.files_generation()?;
    let changed =
        reparse_all || parsed_files > 0 || deleted_files > 0 || existing.is_empty() || edges_stale;

    let (total_files, total_symbols, total_edges, total_repo_tokens, generation) = if changed {
        const MAX_ATTEMPTS: usize = 5;
        let mut attempt = 0;
        // Git co-change edges depend only on git history and the set of indexed paths, not on
        // file contents, so a retry reuses them unless the path set itself changed.
        let mut cochange: Option<(HashSet<String>, Vec<Edge>)> = None;
        loop {
            attempt += 1;
            // Snapshot marker first, then the data: if another process writes file rows
            // in between, `replace_edges_if_current` refuses and we re-resolve.
            let built_from = db.files_generation()?;
            let all_files = db.load_all_files()?;
            let all_symbols = db.load_all_symbols()?;
            let all_refs = db.load_all_raw_refs()?;
            let mut resolved = resolve_edges(&all_files, &all_symbols, &all_refs);
            let indexed_paths: HashSet<String> = all_files.iter().map(|f| f.path.clone()).collect();
            let reusable = cochange
                .as_ref()
                .is_some_and(|(paths, _)| *paths == indexed_paths);
            if !reusable {
                let edges = crate::git::extract_cochange_edges(repo_root, &indexed_paths);
                cochange = Some((indexed_paths, edges));
            }
            if let Some((_, edges)) = &cochange {
                resolved.extend(edges.iter().cloned());
            }
            if let Some(committed) = db.replace_edges_if_current(&resolved, built_from)? {
                let tokens: usize = all_files.iter().map(|f| f.token_count).sum();
                break (
                    all_files.len(),
                    all_symbols.len(),
                    committed.edge_count,
                    tokens,
                    committed.generation,
                );
            }
            if attempt >= MAX_ATTEMPTS {
                bail!(
                    "the index kept changing while rebuilding the graph \
                     (another TrackerTop process is indexing); please retry"
                );
            }
        }
    } else {
        // Nothing changed: aggregate in SQL instead of loading every row.
        let (files, symbols, edges, tokens) = db.index_totals()?;
        (files, symbols, edges, tokens, db.index_generation()?)
    };
    // Recorded only once every file was re-parsed and the edges committed: an interrupted
    // run re-parses everything again next time.
    if outdated {
        db.set_extractor_revision(EXTRACTOR_REVISION)?;
    }

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
    parent_ignores: bool,
) -> Vec<(String, PathBuf, Arc<dyn LanguageExtractor>)> {
    let mut results = Vec::new();
    let walker = WalkBuilder::new(repo_root)
        .parents(parent_ignores)
        .require_git(parent_ignores)
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
                // A qualified Rust call (`Type::f`, `module::f`) binds only inside that type or
                // module. Falling back to the bare name would tie `Stroke::new` (an external
                // type) to whatever `new` this project defines: no edge beats a wrong edge.
                let is_call = matches!(rref.kind, EdgeKind::Calls | EdgeKind::Tests);
                let is_rust = src_file.ends_with(".rs");
                let rust_qualified = is_rust && is_call && target.contains("::");
                // `.name` is a Rust method call on a value (see `extract_callable_name`)
                let method_call = is_rust && is_call && target.starts_with('.');
                if method_call || (is_call && !target.contains("::")) {
                    let name = target.trim_start_matches('.');
                    let syms = by_name.get(name).map(Vec::as_slice).unwrap_or_default();
                    let syms = if syms.is_empty() {
                        by_name_ci
                            .get(&name.to_ascii_lowercase())
                            .map(Vec::as_slice)
                            .unwrap_or_default()
                    } else {
                        syms
                    };
                    let candidates = call_candidates(syms, method_call, is_rust);
                    if let Some(best) = pick_nearest_symbol(&candidates, src_file) {
                        resolved_targets.push(best.id.clone());
                    }
                } else if rust_qualified {
                    if let Some(best) =
                        resolve_qualified_rust_call(target, src_file, &by_qname, &by_name)
                    {
                        resolved_targets.push(best.id.clone());
                    }
                }
                // A Rust type use names a type: never a function, module or doc section
                // that happens to share the name (an external type gets no edge)
                else if is_rust && rref.kind == EdgeKind::UsesType {
                    let types: Vec<&Symbol> = by_name
                        .get(target)
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                        .iter()
                        .copied()
                        .filter(|s| {
                            matches!(
                                s.kind,
                                SymbolKind::Struct
                                    | SymbolKind::Enum
                                    | SymbolKind::Trait
                                    | SymbolKind::TypeAlias
                                    | SymbolKind::Class
                            )
                        })
                        .collect();
                    if let Some(best) = pick_nearest_symbol(&types, src_file) {
                        resolved_targets.push(best.id.clone());
                    }
                }
                // Direct file path reference (e.g., from Markdown `src/graph.rs`)
                else if let Some(fid) = file_by_path.get(target) {
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

/// Which of the symbols sharing a callee's name a call can reach. Modules, doc sections and
/// the like are never call targets (`pub mod review` must not catch a call to `review()`).
/// A Rust method call on a value (`x.flush()`) only reaches methods: when the project has
/// no method of that name it is a call into a library type, and gets no edge. For other
/// languages, if no callable matches, the previous behaviour (any symbol) is kept.
fn call_candidates<'a>(syms: &[&'a Symbol], method_call: bool, is_rust: bool) -> Vec<&'a Symbol> {
    if method_call {
        return syms
            .iter()
            .copied()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
    }
    let callable: Vec<&Symbol> = syms
        .iter()
        .copied()
        .filter(|s| {
            matches!(
                s.kind,
                SymbolKind::Function
                    | SymbolKind::Macro
                    | SymbolKind::Struct
                    | SymbolKind::Enum
                    | SymbolKind::Class
                    | SymbolKind::ShaderEntry
            )
            // A bare Rust `f()` cannot call a method: methods need a receiver (`x.f()`,
            // resolved above) or a path (`Type::f()`, `Self::f()`)
            || (s.kind == SymbolKind::Method && !is_rust)
        })
        .collect();
    if callable.is_empty() && !is_rust {
        syms.to_vec()
    } else {
        callable
    }
}

/// Resolves a Rust callee kept qualified by the extractor (see `extract_callable_name`):
/// - `Type::f` (capitalised qualifier) needs a symbol whose qualified name is exactly `Type::f`;
/// - `module::f` needs a symbol `f` defined in that module: a file `module.rs`, `module/mod.rs`
///   or a file directly inside `module/`.
///
/// Anything else (an external type, `std::fs::read`, an enum variant, a trait call such as
/// `Default::default`) resolves to nothing.
fn resolve_qualified_rust_call<'a>(
    target: &str,
    src_file: &str,
    by_qname: &HashMap<String, Vec<&'a Symbol>>,
    by_name: &HashMap<String, Vec<&'a Symbol>>,
) -> Option<&'a Symbol> {
    let (qualifier, func) = target.rsplit_once("::")?;
    if qualifier.chars().next().is_some_and(char::is_uppercase) {
        return pick_nearest_symbol(by_qname.get(target)?, src_file);
    }
    let in_module: Vec<&Symbol> = by_name
        .get(func)?
        .iter()
        .copied()
        .filter(|s| file_is_module(&s.file_path, qualifier))
        .collect();
    pick_nearest_symbol(&in_module, src_file)
}

/// Whether `path` is the source of Rust module `module` (`.../module.rs`, `.../module/mod.rs`)
/// or a file directly inside its directory (`.../module/x.rs`).
fn file_is_module(path: &str, module: &str) -> bool {
    let mut parts = path.rsplit('/');
    let file = parts.next().unwrap_or("");
    let dir = parts.next().unwrap_or("");
    file.strip_suffix(".rs") == Some(module) || dir == module
}

fn pick_nearest_symbol<'a>(syms: &[&'a Symbol], src_file: &str) -> Option<&'a Symbol> {
    if syms.is_empty() {
        return None;
    }
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
