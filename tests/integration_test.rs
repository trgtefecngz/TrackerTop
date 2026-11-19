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

    // Second incremental run should skip all unchanged files
    let stats2 = index_repository(root, false).unwrap();
    assert_eq!(stats2.parsed_files, 0);
    assert_eq!(stats2.skipped_files, 6);

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
    let path_res = engine.shortest_path("src/renderer.rs", "shaders/sky.wgsl", 600).unwrap();
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
        .read_symbols_batch(&["render_frame".to_string(), "fs_sky".to_string()], false, 1200)
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
