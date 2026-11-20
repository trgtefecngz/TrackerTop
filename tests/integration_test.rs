use serde_json::json;
use tempfile::TempDir;
use trackertop::context::TrackerEngine;
use trackertop::indexer::index_repository;
use trackertop::mcp::handle_jsonrpc_message;
use trackertop::report::generate_markdown_report;

#[test]
fn parses_rust_html_wgsl_powershell_and_tracks_token_savings() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // 1. Rust files
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        r#"
pub mod engine;

/// Public greeting entry point.
pub fn run_app(name: &str) -> String {
    let cfg = engine::EngineConfig { label: name.to_string() };
    engine::execute(&cfg)
}

#[test]
fn test_run_app() {
    assert!(!run_app("demo").is_empty());
}
"#,
    )
    .unwrap();

    std::fs::write(
        root.join("src/engine.rs"),
        r#"
pub struct EngineConfig {
    pub label: String,
}

pub fn execute(cfg: &EngineConfig) -> String {
    format!("running {}", cfg.label)
}
"#,
    )
    .unwrap();

    // 2. WGSL shader
    std::fs::create_dir_all(root.join("shaders")).unwrap();
    std::fs::write(
        root.join("shaders/scene.wgsl"),
        r#"
struct CameraUniform {
    view_proj: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> camera: CameraUniform;

fn compute_light(n: vec3<f32>) -> f32 {
    return max(dot(n, vec3<f32>(0.0, 1.0, 0.0)), 0.1);
}

@vertex
fn vs_main(@location(0) pos: vec3<f32>) -> @builtin(position) vec4<f32> {
    return camera.view_proj * vec4<f32>(pos, 1.0);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    let l = compute_light(vec3<f32>(0.0, 1.0, 0.0));
    return vec4<f32>(l, l, l, 1.0);
}
"#,
    )
    .unwrap();

    // 3. HTML host page referencing WGSL shader
    std::fs::write(
        root.join("index.html"),
        r#"<!DOCTYPE html>
<html>
<body>
  <canvas id="gpu-canvas"></canvas>
  <button id="render-btn" onclick="startRender()">Render</button>
  <script>
    async function startRender() {
      const wgsl = await fetch("shaders/scene.wgsl").then(r => r.text());
      return wgsl;
    }
  </script>
</body>
</html>
"#,
    )
    .unwrap();

    // 4. PowerShell scripts with dot-sourcing & custom Verb-Noun calls
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(
        root.join("scripts/common.ps1"),
        r#"
# Common deployment helper
function Invoke-BuildCheck {
    param(
        [string]$Target,
        [switch]$Release
    )
    Write-Host "Checking $Target"
}
"#,
    )
    .unwrap();

    std::fs::write(
        root.join("scripts/deploy.ps1"),
        r#"
. "$PSScriptRoot\common.ps1"

function Start-Pipeline {
    param([string]$Env)
    Invoke-BuildCheck -Target $Env
}
"#,
    )
    .unwrap();

    // Run index
    let stats = index_repository(root, false).unwrap();
    assert_eq!(stats.total_files, 6);
    assert!(stats.total_symbols >= 15);
    assert!(stats.total_edges >= 10);

    // Second incremental run must not re-parse unchanged files. (Files written within the
    // last second may be re-hashed once: their mtime is not trusted yet, see racy handling.)
    let stats2 = index_repository(root, false).unwrap();
    assert_eq!(stats2.parsed_files, 0);

    // Once every file's mtime is safely in the past, the mtime+size fast path must skip ALL
    // of them again (no re-hashing of unchanged files on every run).
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let files = walk_files(root);
    assert_eq!(
        files.len(),
        6,
        "fixture must contain exactly the 6 indexed files"
    );
    for entry in files {
        std::fs::File::options()
            .write(true)
            .open(&entry)
            .unwrap()
            .set_modified(past)
            .unwrap();
    }
    index_repository(root, false).unwrap(); // records the now-trustworthy mtimes
    let stats3 = index_repository(root, false).unwrap();
    assert_eq!(stats3.parsed_files, 0);
    assert_eq!(stats3.skipped_files, 6);

    // Query via TrackerEngine & verify MCP JSON-RPC responses
    let engine = TrackerEngine::new(root);

    let init_resp = handle_jsonrpc_message(
        &engine,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
    )
    .unwrap();
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "trackertop");

    let tools_resp = handle_jsonrpc_message(
        &engine,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .unwrap();
    assert!(tools_resp["result"]["tools"].as_array().unwrap().len() >= 8);

    // Test WGSL lookup
    let wgsl_res = engine.find_symbol("fs_main", 5, 600).unwrap();
    assert!(wgsl_res.text.contains("fs_main"));
    assert!(wgsl_res.text.contains("shaders/scene.wgsl"));

    // Test PowerShell call graph
    let ps_res = engine.call_graph("Start-Pipeline", 2, 800).unwrap();
    assert!(ps_res.text.contains("Invoke-BuildCheck"));

    // Test Rust impact analysis & linked test discovery
    let impact = engine.impact_analysis("execute", 3, 800).unwrap();
    assert!(impact.text.contains("run_app"));

    // Test MCP tool call for task_context
    let pack_resp = handle_jsonrpc_message(
        &engine,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "task_context",
                "arguments": { "task": "EngineConfig execute", "max_tokens": 900 }
            }
        }),
    )
    .unwrap();
    let pack_text = pack_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(pack_text.contains("EngineConfig"));
    assert!(pack_text.contains("[ttop:task_context"));

    // Verify telemetry logged token savings
    let token_stats = engine.token_stats().unwrap();
    assert_eq!(token_stats.total_queries, 4);
    assert!(token_stats.total_baseline_tokens > 0);

    // Generate concise Markdown report
    let md_path = generate_markdown_report(root).unwrap();
    assert!(md_path.exists());
    let md = std::fs::read_to_string(&md_path).unwrap();
    assert!(md.contains("TrackerTop"));
    assert!(md.contains("Architectural Hubs"));
}

#[test]
fn open_language_registry_accepts_custom_community_language() {
    use std::sync::Arc;
    use trackertop::indexer::index_repository_with_registry;
    use trackertop::model::{Symbol, SymbolKind};
    use trackertop::parser::{
        LanguageExtractor, LanguageRegistry, ParsedFile, file_symbol_id, make_file_symbol,
        make_symbol_id,
    };

    struct ToyLangExtractor;
    impl LanguageExtractor for ToyLangExtractor {
        fn id(&self) -> &'static str {
            "toylang"
        }
        fn display_name(&self) -> &'static str {
            "ToyLang"
        }
        fn extensions(&self) -> &'static [&'static str] {
            &["toy"]
        }
        fn color_hex(&self) -> &'static str {
            "#f472b6"
        }
        fn parse(&self, file_path: &str, source: &str) -> anyhow::Result<ParsedFile> {
            let mut out = ParsedFile::default();
            let fid = file_symbol_id(file_path);
            out.symbols.push(make_file_symbol(file_path, source));
            for (idx, line) in source.lines().enumerate() {
                if let Some(name) = line.trim().strip_prefix("def ") {
                    let line_no = idx + 1;
                    out.symbols.push(Symbol {
                        id: make_symbol_id(file_path, name, line_no),
                        file_path: file_path.to_string(),
                        name: name.to_string(),
                        qualified_name: name.to_string(),
                        kind: SymbolKind::Function,
                        signature: format!("def {name}"),
                        start_line: line_no,
                        end_line: line_no,
                        token_count: 3,
                        visibility: "pub".to_string(),
                        doc_summary: String::new(),
                        parent_id: Some(fid.clone()),
                    });
                }
            }
            Ok(out)
        }
    }

    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("hello.toy"), "def greet_world\n").unwrap();

    let mut reg = LanguageRegistry::default_builtin();
    reg.register(Arc::new(ToyLangExtractor));

    let stats = index_repository_with_registry(tmp.path(), false, &reg).unwrap();
    assert_eq!(stats.total_files, 1);
    assert_eq!(stats.total_symbols, 2); // file + greet_world
}

#[test]
fn v04_smart_grep_batch_memory_health_and_rust_wgsl_bridge() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("shaders")).unwrap();
    std::fs::write(
        root.join("shaders/sky.wgsl"),
        r#"
@fragment
fn fs_sky() -> @location(0) vec4<f32> {
    return vec4<f32>(0.2, 0.5, 0.9, 1.0);
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/renderer.rs"),
        r#"
pub fn load_sky_shader() -> &'static str {
    include_str!("../shaders/sky.wgsl")
}

pub fn render_frame() -> &'static str {
    let tag = "SKY_RENDER_PASS";
    let _ = load_sky_shader();
    tag
}
"#,
    )
    .unwrap();

    index_repository(root, false).unwrap();
    let engine = TrackerEngine::new(root);

    // 1. Rust <-> WGSL asset bridge should connect renderer.rs -> shaders/sky.wgsl
    let path_res = engine
        .shortest_path("src/renderer.rs", "shaders/sky.wgsl", 600)
        .unwrap();
    assert!(path_res.text.contains("sky.wgsl"));

    // 2. smart_grep should tag enclosing symbol `render_frame`
    let grep_res = engine.smart_grep("SKY_RENDER_PASS", None, 800).unwrap();
    assert!(grep_res.text.contains("render_frame"));
    assert!(grep_res.text.contains("SKY_RENDER_PASS"));

    // 3. Persistent AI memory auto-injects into read_symbol
    engine
        .remember("render_frame", "Must run after shadow pass")
        .unwrap();
    let batch_res = engine
        .read_symbols_batch(
            &["render_frame".to_string(), "fs_sky".to_string()],
            false,
            1200,
        )
        .unwrap();
    assert!(batch_res.text.contains("Must run after shadow pass"));
    assert!(batch_res.text.contains("fs_sky"));

    // 4. Health check runs cleanly
    let health = engine.health_check(800).unwrap();
    assert!(health.text.contains("# CODE HEALTH CHECK"));
}

#[test]
fn v05_global_languages_typescript_python_and_go() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // TypeScript
    std::fs::write(
        root.join("app.ts"),
        r#"
import { helper } from "./util";

export class ApiClient {
    async fetchUser(id: string) {
        return helper(id);
    }
}

export const formatTitle = (name: string) => {
    return helper(name);
};
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("util.ts"),
        "export function helper(v: string) { return v.trim(); }\n",
    )
    .unwrap();

    // Python
    std::fs::write(
        root.join("service.py"),
        r#"
from models import User

class AuthService:
    """Handles login tokens."""
    def verify_user(self, uid: str) -> User:
        return build_user(uid)

def build_user(uid: str) -> User:
    return User(uid)

def test_verify_user():
    assert build_user("1") is not None
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("models.py"),
        "class User:\n    def __init__(self, uid: str):\n        self.uid = uid\n",
    )
    .unwrap();

    // Go
    std::fs::write(
        root.join("server.go"),
        r#"
package main

type Server struct {
    Addr string
}

func (s *Server) Start() error {
    return bootCluster()
}

func bootCluster() error {
    return nil
}
"#,
    )
    .unwrap();

    let stats = index_repository(root, false).unwrap();
    assert_eq!(stats.total_files, 5);
    assert!(stats.total_symbols >= 12);

    let engine = TrackerEngine::new(root);
    let ts_call = engine.call_graph("fetchUser", 2, 800).unwrap();
    assert!(ts_call.text.contains("helper"));

    let py_impact = engine.impact_analysis("build_user", 2, 800).unwrap();
    assert!(py_impact.text.contains("test_verify_user"));

    let go_call = engine.call_graph("Server::Start", 2, 800).unwrap();
    assert!(go_call.text.contains("bootCluster"));
}

/// The MCP engine caches the built graph between calls; it must still see edits made
/// in this process AND re-indexes performed by another process (GUI / second MCP server).
#[test]
fn graph_cache_never_serves_stale_symbols() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn alpha_fn() -> u8 { 1 }\n").unwrap();

    let engine = TrackerEngine::new(root);
    let found = |e: &TrackerEngine, name: &str| {
        !e.read_symbol(name, false, 0)
            .unwrap()
            .text
            .contains("NOT FOUND IN INDEX")
    };

    assert!(found(&engine, "alpha_fn"));
    assert!(!found(&engine, "beta_fn"));
    // Served from cache: still correct
    assert!(found(&engine, "alpha_fn"));

    // Edit in place (different size, so detection does not depend on mtime resolution)
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha_fn() -> u8 { 1 }\npub fn beta_fn() -> u8 { 2 }\n",
    )
    .unwrap();
    assert!(found(&engine, "beta_fn"));

    // Another engine (stands in for another process) re-indexes first
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha_fn() -> u8 { 1 }\npub fn beta_fn() -> u8 { 2 }\npub fn gamma_fn() -> u8 { 3 }\n",
    )
    .unwrap();
    let other = TrackerEngine::new(root);
    assert!(found(&other, "gamma_fn"));
    // `engine` sees no file changes itself, but the generation moved: must reload
    assert!(found(&engine, "gamma_fn"));
}

/// A file whose timestamp changes but whose content does not (git checkout, no-op save)
/// must be re-hashed once and then skipped again, without bumping the graph generation.
#[test]
fn touched_but_unchanged_files_are_not_rehashed_every_run() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let file = root.join("src/lib.rs");
    std::fs::write(&file, "pub fn stable_fn() -> u8 { 1 }\n").unwrap();

    let first = index_repository(root, false).unwrap();
    assert_eq!(first.total_files, 1);

    // Touch to a different, settled (past) timestamp without changing the content
    let touched_at = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(touched_at)
        .unwrap();

    let touched = index_repository(root, false).unwrap();
    assert_eq!(touched.parsed_files, 0);
    assert_eq!(
        touched.generation, first.generation,
        "content unchanged: no graph rebuild"
    );

    let again = index_repository(root, false).unwrap();
    assert_eq!(
        again.skipped_files, 1,
        "refreshed mtime lets the fast path skip the file"
    );
}

fn write_call_graph_repo(root: &std::path::Path, extra_fns: usize) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    let mut b = String::from("pub fn leaf() -> u8 { 1 }\n");
    for k in 0..extra_fns {
        b.push_str(&format!("pub fn extra_{k}() -> u8 {{ leaf() }}\n"));
    }
    std::fs::write(root.join("src/b.rs"), b).unwrap();
    let mut a = String::from("mod b;\npub fn root() -> u8 { b::leaf() }\n");
    for k in 0..extra_fns {
        a.push_str(&format!("pub fn call_{k}() -> u8 {{ b::extra_{k}() }}\n"));
    }
    std::fs::write(root.join("src/lib.rs"), a).unwrap();
}

/// A run that wrote file rows but died before rebuilding edges must not lose those edges
/// forever: the next (otherwise no-op) run notices and repairs them.
#[test]
fn interrupted_edge_rebuild_is_repaired_on_next_run() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write_call_graph_repo(root, 3);
    let full = index_repository(root, false).unwrap();
    assert!(full.total_edges > 0);

    // Simulate the crash window: file rows advanced, edges wiped and never rebuilt.
    // White-box on purpose; the row-count asserts make a schema change fail loudly here
    // instead of silently turning this into a no-op test.
    let conn = open_existing_index(root);
    let bumped = conn
        .execute(
            "UPDATE meta SET value = value + 1 WHERE key = 'files_generation'",
            [],
        )
        .unwrap();
    assert_eq!(
        bumped, 1,
        "meta.files_generation row missing: schema changed?"
    );
    let wiped = conn.execute("DELETE FROM edges", []).unwrap();
    assert_eq!(
        wiped, full.total_edges,
        "edges table out of sync with stats"
    );
    drop(conn);

    let repaired = index_repository(root, false).unwrap();
    assert_eq!(repaired.parsed_files, 0);
    assert_eq!(repaired.total_edges, full.total_edges);
}

/// End-to-end convergence check: several indexers racing while files change must end on the
/// same edge set as a clean full rebuild.
///
/// Scheduling decides which interleavings actually happen, so this test alone does not prove
/// the stale-snapshot guard; `db::tests::stale_edge_snapshot_cannot_overwrite_newer_state`
/// (deterministic) and `interrupted_edge_rebuild_is_repaired_on_next_run` do.
#[test]
fn concurrent_indexers_never_leave_stale_edges() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    write_call_graph_repo(&root, 1);
    index_repository(&root, false).unwrap();

    let errors = std::sync::Mutex::new(Vec::<String>::new());
    std::thread::scope(|s| {
        for _ in 0..4 {
            let root = root.clone();
            let errors = &errors;
            s.spawn(move || {
                for _ in 0..6 {
                    if let Err(e) = index_repository(&root, false) {
                        errors.lock().unwrap().push(format!("{e:#}"));
                    }
                }
            });
        }
        for n in 2..8 {
            write_call_graph_repo(&root, n);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });

    // The only acceptable failure is the documented retryable "index kept changing" error.
    for e in errors.into_inner().unwrap() {
        assert!(
            e.contains("kept changing"),
            "unexpected indexing error: {e}"
        );
    }

    let settled = index_repository(&root, false).unwrap();
    let settled_edges = edge_set(&root);
    let clean = index_repository(&root, true).unwrap();
    assert_eq!(settled.total_symbols, clean.total_symbols);
    // Compare the actual edges, not just their count: a wrong set with the same size must fail.
    assert_eq!(settled_edges, edge_set(&root));
    assert_eq!(settled_edges.len(), clean.total_edges);
}

/// Opens the index of an already-indexed repo WITHOUT creating it, so a moved or renamed
/// store fails with a clear message instead of an empty database and "no such table".
fn open_existing_index(root: &std::path::Path) -> rusqlite::Connection {
    let path = root.join(".trackertop/index.db");
    assert!(
        path.is_file(),
        "index store not found at {}",
        path.display()
    );
    rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap()
}

/// Sorted `(source, target, kind)` rows of the persisted edge set.
fn edge_set(root: &std::path::Path) -> Vec<(String, String, String)> {
    let conn = open_existing_index(root);
    let mut stmt = conn
        .prepare("SELECT source_id, target_id, kind FROM edges")
        .unwrap();
    let mut rows: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows.sort();
    rows
}

/// "Racy" edit: two same-size versions of a file written within the same second share the
/// same (second-resolution) mtime and size. The second version must still be indexed.
#[test]
fn same_second_same_size_edit_is_not_missed() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let file = root.join("src/lib.rs");
    // A stamp that is never "in the past" during this test, so both writes are racy no matter
    // how slow the machine is (a stamp of `now` would depend on the first index finishing
    // within a second).
    let stamp = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    let write_with_stamp = |body: &str| {
        std::fs::write(&file, body).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
    };

    write_with_stamp("pub fn alpha_v1() -> u8 { 1 }\n");
    index_repository(root, false).unwrap();
    // Same length, same mtime: invisible to a naive mtime+size fast path
    write_with_stamp("pub fn omega_v2() -> u8 { 2 }\n");
    let second = index_repository(root, false).unwrap();

    assert_eq!(second.parsed_files, 1, "racy edit was skipped");
    let engine = TrackerEngine::new(root);
    assert!(
        !engine
            .read_symbol("omega_v2", false, 0)
            .unwrap()
            .text
            .contains("NOT FOUND IN INDEX")
    );
}

/// Once a file's mtime is safely in the past, the mtime+size fast path skips it again
/// (racy handling must not turn into re-hashing every file on every run).
#[test]
fn settled_files_take_the_fast_path_again() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let file = root.join("src/lib.rs");
    std::fs::write(&file, "pub fn settled_fn() -> u8 { 1 }\n").unwrap();
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(past)
        .unwrap();

    index_repository(root, false).unwrap();
    let again = index_repository(root, false).unwrap();
    assert_eq!(again.skipped_files, 1);
}

/// All regular files under `root`, excluding TrackerTop's own `.trackertop/` store.
fn walk_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|n| n == ".trackertop") {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// Ground truth for smart_grep: an independent scan of every file on disk whose type the
/// registry indexes, splitting on '\n' and dropping a trailing '\r' (no `str::lines`).
fn naive_grep(root: &std::path::Path, pattern: &str) -> Vec<(String, usize)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                if !p.file_name().unwrap().to_string_lossy().starts_with('.') {
                    walk(&p, out);
                }
            } else {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(root, &mut files);
    let pat = pattern.to_lowercase();
    let registry = trackertop::parser::default_registry();
    let mut hits = Vec::new();
    for f in files {
        if registry.extractor_for_path(&f).is_none() {
            continue;
        }
        let rel = f
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(&f).unwrap();
        for (i, raw) in text.split('\n').enumerate() {
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            if line.to_lowercase().contains(&pat) {
                hits.push((rel.clone(), i + 1));
            }
        }
    }
    hits.sort();
    hits
}

#[test]
fn smart_grep_matches_a_plain_scan_and_states_its_coverage() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src/ui")).unwrap();
    std::fs::write(
        root.join("src/a.rs"),
        "// Needle in a comment\r\npub fn find_needle() -> &'static str {\r\n    \"NEEDLE (in a string)\"\r\n}\r\nfn other() { let needle = needle_twice(needle); }\r\n",
    )
    .unwrap();
    let many: String = (0..20)
        .map(|i| format!("export const needle{i} = () => {i};\n"))
        .collect();
    std::fs::write(root.join("src/b.ts"), many).unwrap();
    std::fs::write(
        root.join("src/ui/c.tsx"),
        "export function Banner() {\n  return <p>ÇÖĞÜŞ a.b (needle)</p>;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("NOTES.unindexed"),
        "needle in docs, not indexed\n",
    )
    .unwrap();

    index_repository(root, true).unwrap();
    let db = trackertop::db::Database::open_for_repo(root).unwrap();
    let graph = trackertop::graph::CodeGraph::load(&db).unwrap();
    let indexed: Vec<&String> = graph.files.keys().collect();

    for pattern in [
        "needle",
        "NEEDLE",
        "çöğüş",
        "a.b",
        "(",
        "static str",
        "zzz-absent",
    ] {
        let scan = trackertop::context::grep_scan(root, &indexed, pattern, None);
        let mut got: Vec<(String, usize)> = scan
            .files
            .iter()
            .flat_map(|f| f.lines.iter().map(move |(n, _)| (f.path.clone(), *n)))
            .collect();
        got.sort();
        assert_eq!(got, naive_grep(root, pattern), "pattern {pattern:?}");
        assert!(scan.unreadable.is_empty());
        assert_eq!(scan.files_searched, 3, "pattern {pattern:?}");
    }

    // Rendered output: caps are announced and totals match the ground truth.
    let engine = TrackerEngine::new(root);
    let out = engine.smart_grep("needle", None, 20_000).unwrap().text;
    let truth = naive_grep(root, "needle").len();
    assert!(
        out.contains(&format!("{truth} matching lines in 3 files")),
        "{out}"
    );
    assert!(
        out.contains("(showing 12 of 20 matches in this file; 8 more not shown)"),
        "{out}"
    );
    assert!(
        out.contains("Not searched: files outside the index"),
        "{out}"
    );
    let absent = engine.smart_grep("zzz-absent", None, 2_000).unwrap().text;
    assert!(absent.contains("No matches found in the searched files."));
    assert!(absent.contains("0 matching lines in 0 files"), "{absent}");
    let filtered = engine
        .smart_grep("needle", Some("src/ui"), 2_000)
        .unwrap()
        .text;
    assert!(
        filtered.contains("(2 more excluded by file_filter)"),
        "{filtered}"
    );
}

#[test]
fn lookups_distinguish_not_indexed_from_not_found() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn present() {}\n").unwrap();
    std::fs::write(root.join("data.unindexed"), "# readme\n").unwrap();
    index_repository(root, true).unwrap();
    let engine = TrackerEngine::new(root);

    let readme = engine.file_outline("data.unindexed", 800).unwrap().text;
    assert!(
        readme.contains("NOT INDEXED: `data.unindexed` exists"),
        "{readme}"
    );
    let missing = engine.file_outline("src/nope.rs", 800).unwrap().text;
    assert!(
        missing.contains("NOT FOUND: no file `src/nope.rs`"),
        "{missing}"
    );
    let sym = engine.read_symbol("absent_fn", true, 800).unwrap().text;
    assert!(sym.contains("NOT FOUND IN INDEX"), "{sym}");
    assert!(sym.contains("1 indexed files"), "{sym}");
    let batch = engine
        .read_symbols_batch(
            &["present".to_string(), "absent_fn".to_string()],
            false,
            2_000,
        )
        .unwrap()
        .text;
    assert!(batch.contains("pub fn present"), "{batch}");
    assert!(
        batch.contains("NOT FOUND IN INDEX: no symbol matches `absent_fn`"),
        "{batch}"
    );
}

#[test]
fn outlines_state_whether_spans_come_from_a_syntax_tree() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn precise() {}\n").unwrap();
    std::fs::write(root.join("src/a.ts"), "export function tsFn() {}\n").unwrap();
    std::fs::write(
        root.join("run.ps1"),
        "function Invoke-Thing {\n  Write-Host 1\n}\n",
    )
    .unwrap();
    index_repository(root, true).unwrap();
    let engine = TrackerEngine::new(root);
    for path in ["src/lib.rs", "src/a.ts"] {
        let out = engine.file_outline(path, 800).unwrap().text;
        assert!(out.contains("; tree-sitter)"), "{out}");
    }
    let ps = engine.file_outline("run.ps1", 800).unwrap().text;
    assert!(ps.contains("heuristic extractor"), "{ps}");
    let sym = engine.read_symbol("Invoke-Thing", false, 800).unwrap().text;
    assert!(sym.contains("Span: heuristic extractor"), "{sym}");
    let precise = engine.read_symbol("precise", false, 800).unwrap().text;
    assert!(!precise.contains("Span:"), "{precise}");
}

#[test]
fn run_checks_summarizes_a_real_cargo_test_run() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"rc_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn passes() { assert_eq!(super::add(1, 2), 3); }\n    #[test]\n    fn fails() { assert_eq!(super::add(2, 2), 5); }\n}\n",
    )
    .unwrap();
    let engine = TrackerEngine::new(root);

    let out = engine.run_checks("test", None, 600, 4_000).unwrap().text;
    assert!(out.contains("RESULT: FAILED (exit 101)"), "{out}");
    assert!(out.contains("1 passed, 1 failed"), "{out}");
    assert!(out.contains("- tests::fails"), "{out}");
    assert!(out.contains("lib.rs:8:"), "{out}");
    assert!(!out.contains("- tests::passes"), "{out}");

    let filtered = engine
        .run_checks("test", Some("passes"), 600, 4_000)
        .unwrap()
        .text;
    assert!(filtered.contains("RESULT: PASSED"), "{filtered}");
    assert!(filtered.contains("`cargo test passes`"), "{filtered}");

    // Ambiguous extra args and unknown kinds are errors, not empty results.
    assert!(engine.run_checks("all", Some("x"), 60, 1_000).is_err());
    let resp = handle_jsonrpc_message(
        &engine,
        &json!({"jsonrpc":"2.0","id":9,"method":"tools/call",
                "params":{"name":"run_checks","arguments":{"kind":"bogus"}}}),
    )
    .unwrap();
    assert_eq!(resp["result"]["isError"], json!(true), "{resp}");
}

#[test]
fn run_checks_says_when_no_command_is_detected() {
    let tmp = TempDir::new().unwrap();
    let engine = TrackerEngine::new(tmp.path());
    let out = engine.run_checks("test", None, 60, 1_000).unwrap().text;
    assert!(out.contains("test: NOT DETECTED"), "{out}");
}

fn git_in(root: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(root)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn changes_since_maps_working_tree_diffs_to_symbols() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha() -> i32 {\n    1\n}\n\npub fn beta() -> i32 {\n    2\n}\n\npub fn gamma() -> i32 {\n    3\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("src/keep.rs"), "pub fn keep() {}\n").unwrap();
    git_in(root, &["init", "-q"]);
    git_in(root, &["add", "."]);
    git_in(root, &["commit", "-q", "-m", "init"]);

    // beta edited, gamma deleted, delta added; one untracked file
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha() -> i32 {\n    1\n}\n\npub fn beta() -> i32 {\n    20 + 2\n}\n\npub fn delta() -> i32 {\n    4\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("src/fresh.rs"), "pub fn fresh() {}\n").unwrap();
    let engine = TrackerEngine::new(root);

    let out = engine.changes_since("HEAD", &[], &[], 4_000).unwrap().text;
    assert!(out.contains("## src/lib.rs (modified"), "{out}");
    assert!(out.contains("Touched: beta [fn] L5-7"), "{out}");
    assert!(out.contains("Added symbols: delta"), "{out}");
    assert!(out.contains("Removed symbols: gamma"), "{out}");
    assert!(out.contains("+    20 + 2"), "{out}");
    assert!(out.contains("-    2"), "{out}");
    assert!(
        out.contains("## Untracked new files (1)\n- src/fresh.rs"),
        "{out}"
    );
    assert!(!out.contains("keep.rs"), "{out}");
    assert!(!out.contains(".trackertop"), "{out}");

    let beta = engine
        .changes_since("HEAD", &[], &["beta".to_string()], 4_000)
        .unwrap()
        .text;
    assert!(beta.contains("+    20 + 2"), "{beta}");
    assert!(!beta.contains("delta"), "only hunks touching beta: {beta}");
    let alpha = engine
        .changes_since("HEAD", &[], &["alpha".to_string()], 4_000)
        .unwrap()
        .text;
    assert!(
        alpha.contains("No changes touch the requested symbols"),
        "{alpha}"
    );
    let gone = engine
        .changes_since("HEAD", &[], &["gamma".to_string()], 4_000)
        .unwrap()
        .text;
    assert!(gone.contains("SYMBOL `gamma` NOT FOUND IN INDEX"), "{gone}");

    let scoped = engine
        .changes_since("HEAD", &["src/keep.rs".to_string()], &[], 4_000)
        .unwrap()
        .text;
    assert!(
        scoped.contains("No changes since that commit in the requested scope"),
        "{scoped}"
    );

    let bad = engine
        .changes_since("no-such-ref", &[], &[], 1_000)
        .unwrap_err();
    assert!(
        format!("{bad:#}").contains("does not name a commit"),
        "{bad:#}"
    );
    let plain = TempDir::new().unwrap();
    let err = TrackerEngine::new(plain.path())
        .changes_since("HEAD", &[], &[], 1_000)
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("not a git repository"),
        "{err:#}"
    );
}

#[test]
fn project_playbook_is_served_over_mcp() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("package.json"),
        r#"{"scripts":{"test":"vitest run"},"engines":{"node":">=20"}}"#,
    )
    .unwrap();
    std::fs::write(tmp.path().join("CLAUDE.md"), "rules\n").unwrap();
    let engine = TrackerEngine::new(tmp.path());
    let resp = handle_jsonrpc_message(
        &engine,
        &json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
                "params":{"name":"project_playbook","arguments":{}}}),
    )
    .unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("READ FIRST (instructions for agents): CLAUDE.md"),
        "{text}"
    );
    assert!(
        text.contains("test: `npm run test` [detected from package.json scripts.test]"),
        "{text}"
    );
    assert!(
        text.contains("Node >=20 [detected from package.json engines.node]"),
        "{text}"
    );
}

#[test]
fn decisions_and_dismissed_leads_surface_when_code_is_read() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn run() {}\n\npub fn run_checks() {}\n",
    )
    .unwrap();
    index_repository(root, true).unwrap();
    let engine = TrackerEngine::new(root);

    let saved = engine
        .remember_kind(
            "run",
            "keep run() synchronous",
            "decision",
            "callers hold a lock",
        )
        .unwrap();
    assert!(saved.contains("Saved decision #"), "{saved}");
    engine
        .remember_kind(
            "src/lib.rs",
            "possible race in run_checks",
            "dismissed",
            "single-threaded by design, verified",
        )
        .unwrap();
    assert!(engine.remember_kind("run", "x", "decision", "").is_err());

    let run = engine.read_symbol("run", false, 2_000).unwrap().text;
    assert!(
        run.contains("Decision #1 [run]: keep run() synchronous (why: callers hold a lock)"),
        "{run}"
    );
    let checks = engine.read_symbol("run_checks", false, 2_000).unwrap().text;
    assert!(
        !checks.contains("keep run() synchronous"),
        "note on `run` leaked: {checks}"
    );
    assert!(
        checks.contains("Dismissed lead #2 [src/lib.rs]: possible race in run_checks"),
        "{checks}"
    );
    let outline = engine.file_outline("src/lib.rs", 2_000).unwrap().text;
    assert!(outline.contains("Decision #1 [run]"), "{outline}");
    assert!(outline.contains("Dismissed lead #2"), "{outline}");

    let recalled = engine.recall(Some("dismissed")).unwrap();
    assert!(recalled.contains("# SAVED MEMORY NOTES (1)"), "{recalled}");
    assert!(root.join(".trackertop/notes/00001-decision.md").is_file());
}

#[test]
fn mcp_calls_are_tagged_with_the_client_name() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "pub fn tagged() {}\n").unwrap();
    let engine = TrackerEngine::new(tmp.path());
    handle_jsonrpc_message(
        &engine,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"clientInfo":{"name":"claude-code\n<script>","version":"1"}}}),
    )
    .unwrap();
    handle_jsonrpc_message(
        &engine,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                "params":{"name":"read_symbol","arguments":{"symbol":"tagged"}}}),
    )
    .unwrap();
    let stats = engine.token_stats().unwrap();
    assert_eq!(stats.total_queries, 1);
    assert_eq!(stats.recent[0].source, "mcp:claude-codescript");
    assert_eq!(stats.recent[0].tool, "read_symbol");

    // A CLI engine on the same repo is recorded separately, not as agent usage
    let cli = TrackerEngine::new(tmp.path());
    cli.read_symbol("tagged", false, 500).unwrap();
    let stats = cli.token_stats().unwrap();
    assert_eq!(stats.total_queries, 1);
    assert!(
        stats
            .by_source
            .iter()
            .any(|s| s.source == "cli" && s.queries == 1)
    );
}

/// The matryoshka view must account for every symbol edge at every level of a real index:
/// each edge touching a scope is drawn (inside an aggregated edge), hidden or external,
/// checked against an independent count from the raw edge list.
#[test]
fn nested_graph_accounts_for_every_edge_on_a_real_index() {
    use std::collections::HashSet;
    use trackertop::db::Database;
    use trackertop::graph::CodeGraph;
    use trackertop::model::EdgeKind;
    use trackertop::nested::{NestedGraph, NestedLevel, NestedView};

    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write_call_graph_repo(root, 3);
    std::fs::create_dir_all(root.join("tools")).unwrap();
    std::fs::write(
        root.join("tools/cli.rs"),
        "pub fn main_tool() -> u8 { crate::root() + crate::b::leaf() }\n",
    )
    .unwrap();
    index_repository(root, false).unwrap();
    let db = Database::open_for_repo(root).unwrap();
    let cg = CodeGraph::load(&db).unwrap();
    let ng = NestedGraph::new(&cg);

    let resolved: Vec<(String, String)> = cg
        .edges
        .iter()
        .filter(|e| e.kind != EdgeKind::Contains)
        .filter_map(|e| {
            let s = cg.symbols.get(&e.source_id)?;
            let t = cg.symbols.get(&e.target_id)?;
            Some((s.file_path.clone(), t.file_path.clone()))
        })
        .collect();
    assert!(
        !resolved.is_empty(),
        "fixture must produce cross-symbol edges"
    );
    let accounted = |v: &NestedView| {
        v.edges.iter().map(|e| e.weight).sum::<usize>() + v.hidden_edges + v.external_edges
    };

    let project = ng.view(&NestedLevel::Project).unwrap();
    assert_eq!(accounted(&project), resolved.len());
    assert_eq!(project.truncated, 0);
    let total_files: usize = project.nodes.iter().map(|n| n.child_count).sum();
    assert_eq!(
        total_files,
        cg.files.len(),
        "every file sits in exactly one community"
    );

    for c in &project.nodes {
        let level = c.drill.clone().unwrap();
        let cv = ng.view(&level).unwrap();
        let files: HashSet<String> = cv
            .nodes
            .iter()
            .map(|n| n.id.trim_start_matches("f:").to_string())
            .collect();
        let touching = resolved
            .iter()
            .filter(|(a, b)| files.contains(a) || files.contains(b))
            .count();
        assert_eq!(accounted(&cv), touching, "community {}", c.id);
        for f in &cv.nodes {
            let fv = ng.view(f.drill.as_ref().unwrap()).unwrap();
            let path = f.id.trim_start_matches("f:");
            let touching = resolved
                .iter()
                .filter(|(a, b)| a == path || b == path)
                .count();
            assert_eq!(accounted(&fv), touching, "file {path}");
            for s in &fv.nodes {
                let loc = s.location.as_deref().unwrap();
                assert!(loc.starts_with(&format!("{path}:")), "{loc}");
            }
        }
    }
}
