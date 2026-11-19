use crate::db::Database;
use crate::graph::CodeGraph;
use crate::model::{EdgeKind, LanguageInfo, SymbolKind};
use crate::parser::default_registry;
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Serialize)]
struct HtmlGraphPayload {
    repo_name: String,
    total_files: usize,
    total_symbols: usize,
    total_edges: usize,
    total_repo_tokens: usize,
    languages: Vec<LanguageInfo>,
    nodes: Vec<HtmlNode>,
    links: Vec<HtmlLink>,
    god_nodes: Vec<crate::graph::ScoredNode>,
    communities: Vec<crate::graph::CommunitySummary>,
    stats: crate::model::TokenStatsSummary,
}

#[derive(Serialize)]
struct HtmlNode {
    id: String,
    label: String,
    language: String,
    community: usize,
    tokens: usize,
    lines: usize,
    symbol_count: usize,
    rank: f64,
    symbols: Vec<HtmlSymbolPreview>,
}

#[derive(Serialize)]
struct HtmlSymbolPreview {
    name: String,
    kind: String,
    line: usize,
    signature: String,
}

#[derive(Serialize)]
struct HtmlLink {
    source: String,
    target: String,
    kind: String,
    weight: f32,
}

pub struct ReportPaths {
    pub html_path: PathBuf,
    pub markdown_path: PathBuf,
}

/// Generates both `.trackertop/report.html` (offline interactive graph + token savings dashboard)
/// and `.trackertop/GRAPH_REPORT.md` (concise architecture + savings summary).
pub fn generate_reports(repo_root: &Path) -> Result<ReportPaths> {
    let db = Database::open_for_repo(repo_root)?;
    let cg = CodeGraph::load(&db)?;
    let stats = db.get_token_stats(30)?;

    let out_dir = repo_root.join(".trackertop");
    std::fs::create_dir_all(&out_dir)?;

    let repo_name = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Repository")
        .to_string();

    // Build file-level graph projection for clean visual readability
    let mut file_ranks: HashMap<String, f64> = HashMap::new();
    for s in cg.symbols.values() {
        let pr = *cg.pagerank.get(&s.id).unwrap_or(&0.0);
        *file_ranks.entry(s.file_path.clone()).or_insert(0.0) += pr;
    }

    let mut nodes: Vec<HtmlNode> = cg
        .files
        .values()
        .map(|f| {
            let mut syms: Vec<HtmlSymbolPreview> = cg
                .symbols_by_file
                .get(&f.path)
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| cg.symbols.get(id))
                        .filter(|s| s.kind != SymbolKind::File && s.kind != SymbolKind::Impl)
                        .map(|s| HtmlSymbolPreview {
                            name: s.qualified_name.clone(),
                            kind: s.kind.as_str().to_string(),
                            line: s.start_line,
                            signature: s.signature.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            syms.sort_by_key(|s| s.line);
            let symbol_count = syms.len();
            syms.truncate(30);

            HtmlNode {
                id: f.path.clone(),
                label: f.path.rsplit('/').next().unwrap_or(&f.path).to_string(),
                language: f.language.clone(),
                community: *cg.file_communities.get(&f.path).unwrap_or(&0),
                tokens: f.token_count,
                lines: f.line_count,
                symbol_count,
                rank: *file_ranks.get(&f.path).unwrap_or(&0.01),
                symbols: syms,
            }
        })
        .collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));

    // Aggregate symbol edges up to file-to-file edges
    let mut link_agg: HashMap<(String, String, EdgeKind), f32> = HashMap::new();
    for e in &cg.edges {
        if e.kind == EdgeKind::Contains {
            continue;
        }
        let (Some(src), Some(tgt)) = (cg.symbols.get(&e.source_id), cg.symbols.get(&e.target_id))
        else {
            continue;
        };
        if src.file_path == tgt.file_path {
            continue;
        }
        *link_agg
            .entry((src.file_path.clone(), tgt.file_path.clone(), e.kind))
            .or_insert(0.0) += e.weight;
    }

    let links: Vec<HtmlLink> = link_agg
        .into_iter()
        .map(|((source, target, kind), weight)| HtmlLink {
            source,
            target,
            kind: kind.as_str().to_string(),
            weight,
        })
        .collect();

    let total_symbols = cg
        .symbols
        .values()
        .filter(|s| s.kind != SymbolKind::File && s.kind != SymbolKind::Impl)
        .count();
    let total_repo_tokens: usize = cg.files.values().map(|f| f.token_count).sum();
    let god_nodes = cg.god_nodes(15, false);
    let communities = cg.communities();

    // 1. Write GRAPH_REPORT.md
    let md_path = out_dir.join("GRAPH_REPORT.md");
    let md_content = build_markdown_report(
        &repo_name,
        cg.files.len(),
        total_symbols,
        cg.edges.len(),
        total_repo_tokens,
        &god_nodes,
        &communities,
        &stats,
    );
    std::fs::write(&md_path, md_content)
        .with_context(|| format!("failed to write {}", md_path.display()))?;

    // 2. Write self-contained interactive report.html
    let payload = HtmlGraphPayload {
        repo_name,
        total_files: cg.files.len(),
        total_symbols,
        total_edges: cg.edges.len(),
        total_repo_tokens,
        languages: default_registry().languages(),
        nodes,
        links,
        god_nodes,
        communities,
        stats,
    };
    let json_data = serde_json::to_string(&payload)?.replace("</script>", "<\\/script>");
    let html_content = HTML_TEMPLATE.replace("__TRACKERTOP_DATA__", &json_data);
    let html_path = out_dir.join("report.html");
    std::fs::write(&html_path, html_content)
        .with_context(|| format!("failed to write {}", html_path.display()))?;

    Ok(ReportPaths {
        html_path,
        markdown_path: md_path,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_markdown_report(
    repo_name: &str,
    total_files: usize,
    total_symbols: usize,
    total_edges: usize,
    total_repo_tokens: usize,
    god_nodes: &[crate::graph::ScoredNode],
    communities: &[crate::graph::CommunitySummary],
    stats: &crate::model::TokenStatsSummary,
) -> String {
    let mut md = String::new();
    md.push_str(&format!("# TrackerTop Report — {repo_name}\n\n"));
    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n|---|---|\n");
    md.push_str(&format!("| Indexed Files | {total_files} |\n"));
    md.push_str(&format!("| Extracted Symbols | {total_symbols} |\n"));
    md.push_str(&format!("| Graph Edges | {total_edges} |\n"));
    md.push_str(&format!("| Total Repo Tokens | {total_repo_tokens} |\n"));
    md.push_str(&format!("| AI Queries Logged | {} |\n", stats.total_queries));
    md.push_str(&format!(
        "| Net Tokens Saved | **{} ({:.1}%)** |\n",
        stats.total_tokens_saved, stats.savings_percent
    ));
    md.push_str(&format!(
        "| Est. Savings (Sonnet / Opus) | ${:.4} / ${:.4} |\n\n",
        stats.est_usd_saved_sonnet, stats.est_usd_saved_opus
    ));

    md.push_str("## Architectural Hubs (God Nodes)\n\n");
    md.push_str("| Symbol | Kind | File | In | Out | Score |\n|---|---|---|---:|---:|---:|\n");
    for g in god_nodes {
        md.push_str(&format!(
            "| `{}` | {} | `{}` | {} | {} | {:.2} |\n",
            g.name,
            g.kind.as_str(),
            g.file_path,
            g.in_degree,
            g.out_degree,
            g.score
        ));
    }

    md.push_str("\n## Module Clusters (Communities)\n\n");
    for c in communities {
        md.push_str(&format!(
            "- **C{} `{}`** — {} files, {} symbols, {} tokens | Hubs: `{}`\n",
            c.id,
            c.label,
            c.files.len(),
            c.symbol_count,
            c.total_tokens,
            c.top_hubs.join("`, `")
        ));
    }

    if !stats.by_tool.is_empty() {
        md.push_str("\n## Token Savings by Tool\n\n");
        md.push_str("| Tool | Queries | Sent | Baseline | Saved | Savings % |\n|---|---:|---:|---:|---:|---:|\n");
        for t in &stats.by_tool {
            md.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {:.1}% |\n",
                t.tool,
                t.queries,
                t.tokens_sent,
                t.baseline_tokens,
                t.tokens_saved,
                t.savings_percent
            ));
        }
    }

    md
}

const HTML_TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8" />
<meta name="viewport" content="width=device-width, initial-scale=1.0" />
<title>TrackerTop — AI Codebase Graph & Token Savings</title>
<style>
  :root {
    --bg: #0b0f17;
    --panel: #121826;
    --panel-2: #182032;
    --border: #24304a;
    --text: #e6edf7;
    --muted: #8b9bb8;
    --accent: #38bdf8;
    --green: #34d399;
    --amber: #fbbf24;
    --purple: #a78bfa;
    --orange: #fb923c;
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body {
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Inter, Roboto, sans-serif;
    background: var(--bg);
    color: var(--text);
    height: 100vh;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 20px;
    background: var(--panel);
    border-bottom: 1px solid var(--border);
    gap: 16px;
    flex-wrap: wrap;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 10px;
    font-weight: 700;
    font-size: 16px;
    letter-spacing: 0.3px;
  }
  .badge {
    font-size: 11px;
    background: rgba(52, 211, 153, 0.14);
    color: var(--green);
    border: 1px solid rgba(52, 211, 153, 0.35);
    padding: 2px 8px;
    border-radius: 999px;
  }
  .kpis {
    display: flex;
    gap: 12px;
    flex-wrap: wrap;
  }
  .kpi {
    background: var(--panel-2);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 6px 12px;
    min-width: 110px;
  }
  .kpi-label { font-size: 10px; color: var(--muted); text-transform: uppercase; letter-spacing: 0.5px; }
  .kpi-val { font-size: 15px; font-weight: 700; margin-top: 2px; }
  .kpi-val.saved { color: var(--green); }
  .main {
    flex: 1;
    display: grid;
    grid-template-columns: 1fr 380px;
    min-height: 0;
  }
  .canvas-wrap {
    position: relative;
    overflow: hidden;
    background: radial-gradient(circle at 50% 45%, #11192b 0%, #090d14 100%);
  }
  canvas { display: block; width: 100%; height: 100%; cursor: grab; }
  canvas:active { cursor: grabbing; }
  .toolbar {
    position: absolute;
    top: 12px;
    left: 12px;
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
    background: rgba(18, 24, 38, 0.9);
    backdrop-filter: blur(8px);
    padding: 8px;
    border-radius: 10px;
    border: 1px solid var(--border);
  }
  .toolbar input, .toolbar select, .toolbar button {
    background: var(--panel-2);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 5px 10px;
    font-size: 12px;
    outline: none;
  }
  .toolbar button { cursor: pointer; }
  .toolbar button:hover { border-color: var(--accent); }
  .legend {
    position: absolute;
    bottom: 12px;
    left: 12px;
    display: flex;
    gap: 12px;
    background: rgba(18, 24, 38, 0.88);
    padding: 6px 12px;
    border-radius: 8px;
    border: 1px solid var(--border);
    font-size: 11px;
    color: var(--muted);
  }
  .dot { display: inline-block; width: 9px; height: 9px; border-radius: 50%; margin-right: 5px; vertical-align: middle; }
  aside {
    background: var(--panel);
    border-left: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    min-height: 0;
  }
  .tabs {
    display: flex;
    border-bottom: 1px solid var(--border);
  }
  .tab {
    flex: 1;
    padding: 10px;
    text-align: center;
    font-size: 12px;
    font-weight: 600;
    color: var(--muted);
    background: transparent;
    border: none;
    cursor: pointer;
    border-bottom: 2px solid transparent;
  }
  .tab.active { color: var(--accent); border-bottom-color: var(--accent); background: rgba(56,189,248,0.06); }
  .tab-body { flex: 1; overflow-y: auto; padding: 14px; font-size: 13px; }
  .card {
    background: var(--panel-2);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 10px 12px;
    margin-bottom: 10px;
  }
  .card h3 { font-size: 12px; color: var(--muted); text-transform: uppercase; margin-bottom: 8px; letter-spacing: 0.5px; }
  .sym-row {
    padding: 5px 0;
    border-bottom: 1px solid rgba(36, 48, 74, 0.6);
    font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
    font-size: 11.5px;
  }
  .sym-row:last-child { border-bottom: none; }
  .kind-pill {
    display: inline-block;
    font-size: 10px;
    padding: 1px 5px;
    border-radius: 4px;
    background: rgba(56, 189, 248, 0.15);
    color: var(--accent);
    margin-right: 6px;
  }
  .bar-bg { height: 7px; background: #0c1220; border-radius: 4px; overflow: hidden; margin-top: 4px; }
  .bar-fill { height: 100%; background: linear-gradient(90deg, var(--green), var(--accent)); border-radius: 4px; }
  .muted { color: var(--muted); font-size: 11.5px; }
  .mono { font-family: ui-monospace, SFMono-Regular, Consolas, monospace; }
</style>
</head>
<body>
<header>
  <div class="brand">
    <span>⚡ TrackerTop</span>
    <span class="badge" id="repoName"></span>
  </div>
  <div class="kpis" id="kpiBar"></div>
</header>

<div class="main">
  <div class="canvas-wrap">
    <canvas id="graphCanvas"></canvas>
    <div class="toolbar">
      <input id="searchBox" type="text" placeholder="Filter file or symbol..." />
      <select id="langFilter">
        <option value="all">All Languages</option>
      </select>
      <select id="colorMode">
        <option value="language">Color: Language</option>
        <option value="community">Color: Cluster</option>
      </select>
      <button id="resetViewBtn">Reset View</button>
    </div>
    <div class="legend" id="langLegend"></div>
  </div>

  <aside>
    <div class="tabs">
      <button class="tab active" data-tab="savings">Token Savings</button>
      <button class="tab" data-tab="inspector">Inspector</button>
      <button class="tab" data-tab="hubs">God Nodes</button>
    </div>
    <div class="tab-body" id="tabBody"></div>
  </aside>
</div>

<script>
const DATA = __TRACKERTOP_DATA__;

const LANG_COLORS = Object.fromEntries((DATA.languages || []).map(l => [l.id, l.color_hex]));
const CLUSTER_COLORS = ['#38bdf8','#34d399','#fb923c','#a78bfa','#f472b6','#fbbf24','#22d3ee','#f87171'];

const langSelect = document.getElementById('langFilter');
const langLegend = document.getElementById('langLegend');
for (const l of (DATA.languages || [])) {
  const opt = document.createElement('option');
  opt.value = l.id;
  opt.textContent = l.display_name;
  langSelect.appendChild(opt);
  langLegend.innerHTML += `<span><i class="dot" style="background:${l.color_hex}"></i>${l.display_name}</span>`;
}

document.getElementById('repoName').textContent = DATA.repo_name;

function fmt(n) { return Number(n || 0).toLocaleString(); }

// KPI Bar
const kpis = [
  { label: 'Files', val: fmt(DATA.total_files) },
  { label: 'Symbols', val: fmt(DATA.total_symbols) },
  { label: 'Edges', val: fmt(DATA.total_edges) },
  { label: 'Repo Tokens', val: fmt(DATA.total_repo_tokens) },
  { label: 'Tokens Saved', val: `${fmt(DATA.stats.total_tokens_saved)} (${DATA.stats.savings_percent.toFixed(1)}%)`, cls: 'saved' },
  { label: 'Est. USD (Sonnet)', val: `$${DATA.stats.est_usd_saved_sonnet.toFixed(4)}`, cls: 'saved' }
];
document.getElementById('kpiBar').innerHTML = kpis.map(k =>
  `<div class="kpi"><div class="kpi-label">${k.label}</div><div class="kpi-val ${k.cls||''}">${k.val}</div></div>`
).join('');

// Force simulation state
const canvas = document.getElementById('graphCanvas');
const ctx = canvas.getContext('2d');
let W = 800, H = 600;
let cam = { x: 0, y: 0, zoom: 1 };
let selectedNode = null;
let hoveredNode = null;
let dragNode = null;
let isPanning = false;
let panStart = { x: 0, y: 0 };

const nodes = DATA.nodes.map((n, idx) => {
  const angle = (idx / Math.max(1, DATA.nodes.length)) * Math.PI * 2;
  const r = 160 + (idx % 5) * 35;
  return {
    ...n,
    x: Math.cos(angle) * r,
    y: Math.sin(angle) * r,
    vx: 0,
    vy: 0,
    radius: Math.max(8, Math.min(26, 8 + Math.sqrt(n.tokens) * 0.14 + n.rank * 35))
  };
});
const nodeById = new Map(nodes.map(n => [n.id, n]));
const links = DATA.links
  .map(l => ({ ...l, src: nodeById.get(l.source), tgt: nodeById.get(l.target) }))
  .filter(l => l.src && l.tgt);

function resize() {
  const rect = canvas.parentElement.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  W = rect.width;
  H = rect.height;
  canvas.width = W * dpr;
  canvas.height = H * dpr;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
}
window.addEventListener('resize', resize);
resize();

function screenToWorld(sx, sy) {
  return { x: (sx - W / 2) / cam.zoom - cam.x, y: (sy - H / 2) / cam.zoom - cam.y };
}

function visibleNodes() {
  const q = document.getElementById('searchBox').value.trim().toLowerCase();
  const lf = document.getElementById('langFilter').value;
  return nodes.filter(n => {
    if (lf !== 'all' && n.language !== lf) return false;
    if (!q) return true;
    return n.id.toLowerCase().includes(q) || n.symbols.some(s => s.name.toLowerCase().includes(q));
  });
}

function stepPhysics() {
  const repulse = 2600;
  for (let i = 0; i < nodes.length; i++) {
    const a = nodes[i];
    // gentle gravity to center
    a.vx += -a.x * 0.002;
    a.vy += -a.y * 0.002;
    for (let j = i + 1; j < nodes.length; j++) {
      const b = nodes[j];
      let dx = b.x - a.x;
      let dy = b.y - a.y;
      let distSq = dx * dx + dy * dy + 25;
      let f = repulse / distSq;
      let dist = Math.sqrt(distSq);
      let fx = (dx / dist) * f;
      let fy = (dy / dist) * f;
      a.vx -= fx; a.vy -= fy;
      b.vx += fx; b.vy += fy;
    }
  }
  for (const l of links) {
    const dx = l.tgt.x - l.src.x;
    const dy = l.tgt.y - l.src.y;
    const dist = Math.sqrt(dx * dx + dy * dy) || 1;
    const targetDist = 115;
    const f = (dist - targetDist) * 0.015;
    const fx = (dx / dist) * f;
    const fy = (dy / dist) * f;
    l.src.vx += fx; l.src.vy += fy;
    l.tgt.vx -= fx; l.tgt.vy -= fy;
  }
  for (const n of nodes) {
    if (n === dragNode) continue;
    n.vx *= 0.86;
    n.vy *= 0.86;
    n.x += Math.max(-12, Math.min(12, n.vx));
    n.y += Math.max(-12, Math.min(12, n.vy));
  }
}

function draw() {
  stepPhysics();
  ctx.clearRect(0, 0, W, H);
  ctx.save();
  ctx.translate(W / 2, H / 2);
  ctx.scale(cam.zoom, cam.zoom);
  ctx.translate(cam.x, cam.y);

  const visSet = new Set(visibleNodes().map(n => n.id));
  const colorMode = document.getElementById('colorMode').value;

  // Draw links
  for (const l of links) {
    if (!visSet.has(l.src.id) || !visSet.has(l.tgt.id)) continue;
    const hl = selectedNode && (l.src === selectedNode || l.tgt === selectedNode);
    ctx.strokeStyle = hl ? 'rgba(56, 189, 248, 0.8)' : 'rgba(148, 163, 184, 0.22)';
    ctx.lineWidth = hl ? 1.8 : 1;
    ctx.beginPath();
    ctx.moveTo(l.src.x, l.src.y);
    ctx.lineTo(l.tgt.x, l.tgt.y);
    ctx.stroke();
  }

  // Draw nodes
  for (const n of nodes) {
    if (!visSet.has(n.id)) continue;
    const col = colorMode === 'community'
      ? CLUSTER_COLORS[n.community % CLUSTER_COLORS.length]
      : (LANG_COLORS[n.language] || '#94a3b8');
    const isSel = n === selectedNode || n === hoveredNode;

    ctx.beginPath();
    ctx.arc(n.x, n.y, n.radius, 0, Math.PI * 2);
    ctx.fillStyle = col;
    ctx.globalAlpha = isSel ? 1.0 : 0.86;
    ctx.fill();
    ctx.globalAlpha = 1.0;

    if (isSel) {
      ctx.strokeStyle = '#ffffff';
      ctx.lineWidth = 2.5;
      ctx.stroke();
    }

    ctx.fillStyle = '#e6edf7';
    ctx.font = `${isSel ? '600 ' : ''}11px system-ui, sans-serif`;
    ctx.textAlign = 'center';
    ctx.fillText(n.label, n.x, n.y + n.radius + 13);
  }

  ctx.restore();
  requestAnimationFrame(draw);
}
requestAnimationFrame(draw);

// Pointer interactions
function pickNode(sx, sy) {
  const w = screenToWorld(sx, sy);
  for (let i = nodes.length - 1; i >= 0; i--) {
    const n = nodes[i];
    const dx = w.x - n.x, dy = w.y - n.y;
    if (dx * dx + dy * dy <= (n.radius + 4) ** 2) return n;
  }
  return null;
}

canvas.addEventListener('mousedown', e => {
  const rect = canvas.getBoundingClientRect();
  const sx = e.clientX - rect.left, sy = e.clientY - rect.top;
  const hit = pickNode(sx, sy);
  if (hit) {
    dragNode = hit;
    selectedNode = hit;
    switchTab('inspector');
  } else {
    isPanning = true;
    panStart = { x: e.clientX - cam.x * cam.zoom, y: e.clientY - cam.y * cam.zoom };
  }
});

window.addEventListener('mousemove', e => {
  const rect = canvas.getBoundingClientRect();
  const sx = e.clientX - rect.left, sy = e.clientY - rect.top;
  if (dragNode) {
    const w = screenToWorld(sx, sy);
    dragNode.x = w.x; dragNode.y = w.y;
    dragNode.vx = 0; dragNode.vy = 0;
  } else if (isPanning) {
    cam.x = (e.clientX - panStart.x) / cam.zoom;
    cam.y = (e.clientY - panStart.y) / cam.zoom;
  } else {
    hoveredNode = pickNode(sx, sy);
  }
});

window.addEventListener('mouseup', () => { dragNode = null; isPanning = false; });

canvas.addEventListener('wheel', e => {
  e.preventDefault();
  const factor = e.deltaY < 0 ? 1.12 : 0.89;
  cam.zoom = Math.max(0.25, Math.min(3.5, cam.zoom * factor));
}, { passive: false });

document.getElementById('resetViewBtn').addEventListener('click', () => {
  cam = { x: 0, y: 0, zoom: 1 };
});

// Sidebar tabs
let currentTab = 'savings';
document.querySelectorAll('.tab').forEach(btn => {
  btn.addEventListener('click', () => switchTab(btn.dataset.tab));
});

function switchTab(name) {
  currentTab = name;
  document.querySelectorAll('.tab').forEach(b => b.classList.toggle('active', b.dataset.tab === name));
  renderSidebar();
}

function renderSidebar() {
  const body = document.getElementById('tabBody');
  if (currentTab === 'savings') {
    const s = DATA.stats;
    let html = `
      <div class="card">
        <h3>Cumulative AI Token Savings</h3>
        <div style="font-size:24px;font-weight:700;color:var(--green)">${fmt(s.total_tokens_saved)} tokens saved</div>
        <div class="muted" style="margin-top:4px">${s.savings_percent.toFixed(1)}% reduction across ${fmt(s.total_queries)} AI queries</div>
        <div class="muted" style="margin-top:6px">Sent: <b>${fmt(s.total_tokens_sent)}</b> tok vs Baseline: <b>${fmt(s.total_baseline_tokens)}</b> tok</div>
        <div class="muted" style="margin-top:4px">Est. USD Saved: <b>$${s.est_usd_saved_sonnet.toFixed(4)}</b> (Sonnet) · <b>$${s.est_usd_saved_opus.toFixed(4)}</b> (Opus)</div>
      </div>`;

    if (s.by_tool.length > 0) {
      html += `<div class="card"><h3>Savings by MCP Tool</h3>`;
      for (const t of s.by_tool) {
        html += `<div style="margin-bottom:10px">
          <div style="display:flex;justify-content:space-between;font-size:12px">
            <span class="mono">${t.tool} (${t.queries}x)</span>
            <span style="color:var(--green)">-${fmt(t.tokens_saved)} (${t.savings_percent.toFixed(0)}%)</span>
          </div>
          <div class="bar-bg"><div class="bar-fill" style="width:${Math.min(100, t.savings_percent)}%"></div></div>
        </div>`;
      }
      html += `</div>`;
    }

    html += `<div class="card"><h3>Recent Queries</h3>`;
    if (s.recent.length === 0) {
      html += `<div class="muted">No AI queries logged yet. Run <code>trackertop query ...</code> or connect Claude Code via MCP!</div>`;
    } else {
      for (const r of s.recent.slice(0, 12)) {
        const pct = r.baseline_tokens > 0 ? ((r.tokens_saved / r.baseline_tokens) * 100).toFixed(0) : 0;
        html += `<div class="sym-row">
          <div><span class="kind-pill">${r.tool}</span><b>${escapeHtml(r.query)}</b></div>
          <div class="muted">${fmt(r.tokens_sent)} sent / ${fmt(r.baseline_tokens)} base → <span style="color:var(--green)">saved ${fmt(r.tokens_saved)} (${pct}%)</span> · ${r.latency_ms}ms</div>
        </div>`;
      }
    }
    html += `</div>`;
    body.innerHTML = html;
  } else if (currentTab === 'inspector') {
    if (!selectedNode) {
      body.innerHTML = `<div class="card"><div class="muted">Click any file node in the graph to inspect its extracted symbols, token cost, and dependencies.</div></div>`;
      return;
    }
    const n = selectedNode;
    let html = `<div class="card">
      <h3>${escapeHtml(n.id)}</h3>
      <div class="muted">Language: <b>${n.language}</b> · Lines: <b>${n.lines}</b> · Full File Tokens: <b>${fmt(n.tokens)}</b> · Cluster C${n.community}</div>
    </div>`;
    html += `<div class="card"><h3>Extracted Symbols (${n.symbol_count})</h3>`;
    for (const sym of n.symbols) {
      html += `<div class="sym-row"><span class="kind-pill">${sym.kind}</span><b>${escapeHtml(sym.name)}</b> <span class="muted">L${sym.line}</span><div class="muted">${escapeHtml(sym.signature)}</div></div>`;
    }
    html += `</div>`;
    body.innerHTML = html;
  } else {
    let html = `<div class="card"><h3>Top Architectural Hubs (God Nodes)</h3>`;
    for (const g of DATA.god_nodes) {
      html += `<div class="sym-row">
        <span class="kind-pill">${g.kind}</span><b>${escapeHtml(g.name)}</b>
        <div class="muted">${escapeHtml(g.file_path)} · in:${g.in_degree} out:${g.out_degree}</div>
      </div>`;
    }
    html += `</div><div class="card"><h3>Module Clusters (${DATA.communities.length})</h3>`;
    for (const c of DATA.communities) {
      html += `<div class="sym-row"><b>C${c.id} ${escapeHtml(c.label)}</b> <span class="muted">(${c.files.length}f, ${fmt(c.total_tokens)} tok)</span>
        <div class="muted">${escapeHtml(c.top_hubs.join(', '))}</div></div>`;
    }
    html += `</div>`;
    body.innerHTML = html;
  }
}

function escapeHtml(str) {
  return String(str).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
}

renderSidebar();
</script>
</body>
</html>
"##;
