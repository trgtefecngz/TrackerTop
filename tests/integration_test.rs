use serde_json::json;
use tempfile::TempDir;
use trackertop::context::TrackerEngine;
use trackertop::indexer::index_repository;
use trackertop::mcp::handle_jsonrpc_message;
use trackertop::report::generate_reports;

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

    // Generate HTML + Markdown reports
    let reports = generate_reports(root).unwrap();
    assert!(reports.html_path.exists());
    assert!(reports.markdown_path.exists());
    let html = std::fs::read_to_string(&reports.html_path).unwrap();
    assert!(html.contains("TrackerTop"));
    assert!(html.contains("shaders/scene.wgsl"));
}
