#![cfg(feature = "gui")]

mod architecture;
mod graph_tab;
mod graph_view;
mod journal_view;
mod memory_tab;
mod projects;
mod savings;
mod status_bar;
mod theme;
#[cfg(debug_assertions)]
mod tour;

use self::graph_view::{
    GraphViewState, MemItem, MemKind, NodeKind, Portal, Scene, SceneEdge, SceneNode, Target,
};
use self::theme as T;
use crate::activity::{AgentActivity, QueryResolver, build_activity};
use crate::context::TrackerEngine;
use crate::db::{Database, note_target_matches};
use crate::graph::{CodeGraph, CommunitySummary, HealthReport, ScoredNode};
use crate::indexer::{IndexProgress, index_repository_with_progress};
use crate::install::{AiAgentKind, generic_mcp_json_snippet, install_agent, is_agent_installed};
use crate::model::{EdgeKind, ProjectNote, TokenStatsSummary};
use crate::nested::{NestedGraph, NestedKind, NestedLevel, NestedView};
use crate::parser::default_registry;
use crate::registry::{
    GlobalOverview, load_global_overview, register_project, remove_project_with_cache,
};
use anyhow::Result;
use eframe::egui::{
    self, Color32, CornerRadius, Margin, Pos2, Rect, RichText, Sense, Stroke, Vec2,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Selection {
    AllProjects,
    Project(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectTab {
    Savings,
    Graph,
    Architecture,
    Memory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphLayoutMode {
    Folders,
    Clusters,
    /// Matryoshka drill-down: project → communities → files → symbols (`crate::nested`).
    Nested,
}

struct GraphNodeVis {
    id: String,
    label: String,
    group_folder: String,
    language: String,
    tokens: usize,
    rank: f64,
    degree: usize,
    symbols: Vec<(String, String, usize, String)>,
}

struct GraphEdgeVis {
    src: usize,
    tgt: usize,
}

struct LoadedProject {
    path: String,
    total_files: usize,
    total_symbols: usize,
    total_edges: usize,
    total_repo_tokens: usize,
    lang_breakdown: Vec<(String, String, String, usize, usize)>,
    god_nodes: Vec<ScoredNode>,
    communities: Vec<CommunitySummary>,
    health: HealthReport,
    notes: Vec<ProjectNote>,
    stats: TokenStatsSummary,
    nodes: Vec<GraphNodeVis>,
    edges: Vec<GraphEdgeVis>,
    /// Agent calls and uncommitted changes; `activity_gen` bumps when they change.
    activity: AgentActivity,
    activity_gen: u64,
    /// Summed symbol PageRank per file.
    file_rank: HashMap<String, f64>,
    /// Community id -> palette index (project-level order), label, files.
    community_color: HashMap<String, usize>,
    community_label: HashMap<String, String>,
    community_files: HashMap<String, Vec<String>>,
    /// Health findings per file (large file, dependency cycle, orphan).
    findings: HashMap<String, Vec<String>>,
    /// Kept for the nested (matryoshka) view, which aggregates edges per level on demand.
    graph: CodeGraph,
}

struct BgMessage {
    /// The indexed project, as the registry stores it.
    path: String,
    /// The error text when the run failed.
    error: Option<String>,
    select_path: Option<String>,
}

pub struct TrackerTopApp {
    overview: GlobalOverview,
    selection: Selection,
    active_tab: ProjectTab,
    loaded: Option<LoadedProject>,
    connected_agents: Vec<(AiAgentKind, bool)>,
    show_agents_modal: bool,
    /// A user action's result, shown in the status bar for a few seconds.
    notice: Option<status_bar::Notice>,
    /// Status bar data for the selected project, re-read with the regular refresh.
    status: status_bar::StatusInfo,
    /// The running index job: its live progress and the project it indexes.
    index_progress: Option<std::sync::Arc<IndexProgress>>,
    indexing_path: Option<String>,
    /// The last index failure: (project path, error), cleared by a successful run.
    index_error: Option<(String, String)>,
    last_refresh: Instant,
    bg_rx: Option<Receiver<BgMessage>>,
    bg_busy: bool,

    // AI Memory form inputs
    note_target_input: String,
    note_content_input: String,

    // Dry-run preview state
    preview_result: Option<crate::context::QueryOutput>,

    // Graph view state. Nested is the default layout.
    graph_search: String,
    graph_lang_filter: String,
    graph_code_only: bool,
    graph_layout_mode: GraphLayoutMode,
    graph_view: GraphViewState,
    /// The scene being drawn, keyed by project, layout, level, filters and activity.
    scene: Option<(String, Scene)>,
    last_activity_check: Instant,
    #[cfg(debug_assertions)]
    tour: Option<tour::Tour>,
    /// Where the status bar drew the latest agent call (the tour clicks it).
    #[cfg(debug_assertions)]
    agent_action_rect: Option<egui::Rect>,

    // Nested (matryoshka) view state: the level being shown and its cached view, keyed by
    // (project path, level). An error (e.g. a group vanished after re-indexing) is shown,
    // never an empty canvas.
    nested_level: NestedLevel,
    nested_cache: Option<((String, NestedLevel), Result<NestedView, String>)>,
}

impl TrackerTopApp {
    pub fn new(initial_repo: Option<PathBuf>) -> Self {
        if let Some(ref p) = initial_repo
            && p.join(".trackertop").join("index.db").exists()
        {
            let _ = register_project(p);
        }
        let overview = load_global_overview();
        let selection = if let Some(ref p) = initial_repo {
            let s = p.to_string_lossy().to_string();
            if overview
                .projects
                .iter()
                .any(|pr| pr.entry.path.eq_ignore_ascii_case(&s))
            {
                Selection::Project(s)
            } else if let Some(first) = overview.projects.first() {
                Selection::Project(first.entry.path.clone())
            } else {
                Selection::AllProjects
            }
        } else if let Some(first) = overview.projects.first() {
            Selection::Project(first.entry.path.clone())
        } else {
            Selection::AllProjects
        };

        let mut app = Self {
            overview,
            selection,
            active_tab: ProjectTab::Savings,
            loaded: None,
            connected_agents: poll_agent_statuses(),
            show_agents_modal: false,
            notice: None,
            status: status_bar::StatusInfo::default(),
            index_progress: None,
            indexing_path: None,
            index_error: None,
            last_refresh: Instant::now(),
            bg_rx: None,
            bg_busy: false,
            note_target_input: "project".to_string(),
            note_content_input: String::new(),
            preview_result: None,
            graph_search: String::new(),
            graph_lang_filter: "all".to_string(),
            graph_code_only: false,
            graph_layout_mode: GraphLayoutMode::Nested,
            graph_view: GraphViewState::default(),
            scene: None,
            last_activity_check: Instant::now(),
            #[cfg(debug_assertions)]
            tour: tour::Tour::from_env(),
            #[cfg(debug_assertions)]
            agent_action_rect: None,
            nested_level: NestedLevel::Project,
            nested_cache: None,
        };
        app.reload_selected_project();
        app
    }

    fn reload_overview(&mut self) {
        self.overview = load_global_overview();
        self.connected_agents = poll_agent_statuses();
    }

    fn reload_selected_project(&mut self) {
        self.refresh_status();
        let Selection::Project(ref path) = self.selection else {
            self.loaded = None;
            return;
        };
        self.graph_view.selected = None;
        self.preview_result = None;
        self.nested_cache = None;
        self.scene = None;
        // A nested level belongs to one project: switching projects starts at its root.
        // A re-index of the same project keeps the level (a stale one shows an error).
        if self.loaded.as_ref().map(|lp| lp.path.as_str()) != Some(path.as_str()) {
            self.nested_level = NestedLevel::Project;
            self.graph_view = GraphViewState::default();
        }
        self.loaded = load_project_details(Path::new(path)).ok();
    }

    /// Reloads agent activity; a change bumps `activity_gen`, which rebuilds the scene.
    fn refresh_activity(&mut self) {
        let Some(lp) = self.loaded.as_mut() else {
            return;
        };
        let Ok(db) = Database::open_path(&Path::new(&lp.path).join(".trackertop").join("index.db"))
        else {
            return;
        };
        let act = load_activity(&db, &lp.graph, Path::new(&lp.path));
        if activity_fingerprint(&act) != activity_fingerprint(&lp.activity) {
            lp.activity = act;
            lp.activity_gen += 1;
        }
    }

    fn start_bg_index(&mut self, repo_path: PathBuf, force: bool, select_after: bool) {
        if self.bg_busy {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.bg_rx = Some(rx);
        self.bg_busy = true;
        // Same key the registry stores, by construction
        let key = crate::registry::canonical_path_string(&repo_path);
        let progress = std::sync::Arc::new(IndexProgress::default());
        self.index_progress = Some(progress.clone());
        self.indexing_path = Some(key.clone());
        self.notice = None;

        std::thread::spawn(move || {
            let result = index_repository_with_progress(&repo_path, force, &progress);
            let ok = result.is_ok();
            let msg = BgMessage {
                error: result.err().map(|e| format!("{e:#}")),
                select_path: (ok && select_after).then(|| key.clone()),
                path: key,
            };
            let _ = tx.send(msg);
        });
    }

    fn run_preview_query(&mut self, repo_path: &str) {
        let engine = TrackerEngine::new(Path::new(repo_path));
        engine.set_source("ui");
        match engine.preview_repo_map(900) {
            Ok(out) => {
                let pct = if out.baseline_tokens > 0 {
                    (out.tokens_saved as f64 / out.baseline_tokens as f64) * 100.0
                } else {
                    0.0
                };
                self.notify(format!(
                    "Dry-run preview (not counted in stats): {} tok vs {} baseline -> {:.1}% reduction in {} ms",
                    fmt_num(out.tokens_sent),
                    fmt_num(out.baseline_tokens),
                    pct,
                    out.latency_ms
                ));
                self.preview_result = Some(out);
            }
            Err(e) => self.notify_error(format!("Preview error: {e}")),
        }
    }

    fn reset_project_stats(&mut self, repo_path: &str) {
        let result = Database::open_for_repo(Path::new(repo_path))
            .and_then(|db| db.clear_telemetry().map(|()| db));
        match result {
            Ok(db) => {
                self.notify("Reset token-savings telemetry to 0 for this project.");
                self.reload_overview();
                if let Some(ref mut lp) = self.loaded {
                    lp.stats = db.get_token_stats(30).unwrap_or_default();
                }
            }
            Err(e) => self.notify_error(format!("Could not reset stats: {e:#}")),
        }
    }

    fn poll_background(&mut self) {
        let Some(ref rx) = self.bg_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(msg) => {
                self.bg_busy = false;
                self.bg_rx = None;
                self.index_progress = None;
                self.indexing_path = None;
                match msg.error {
                    Some(e) => self.index_error = Some((msg.path, e)),
                    None => {
                        if self
                            .index_error
                            .as_ref()
                            .is_some_and(|(p, _)| p.eq_ignore_ascii_case(&msg.path))
                        {
                            self.index_error = None;
                        }
                    }
                }
                if let Some(p) = msg.select_path {
                    self.selection = Selection::Project(p);
                }
                self.reload_overview();
                self.reload_selected_project();
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                // The worker died (panicked) without reporting: unlock the UI instead of
                // leaving Re-Index / Add Project disabled forever.
                self.bg_busy = false;
                self.bg_rx = None;
                self.index_progress = None;
                if let Some(p) = self.indexing_path.take() {
                    self.index_error = Some((
                        p,
                        "Indexing stopped unexpectedly. Please try Re-index again.".to_string(),
                    ));
                }
                self.reload_overview();
            }
        }
    }

    fn pick_and_add_project(&mut self) {
        if self.bg_busy {
            return;
        }
        if let Some(folder) = rfd::FileDialog::new()
            .set_title("Select Project Folder to Track")
            .pick_folder()
        {
            self.start_bg_index(folder, false, true);
        }
    }
}

impl eframe::App for TrackerTopApp {
    #[cfg(debug_assertions)]
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.tour_input(raw_input);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(debug_assertions)]
        self.tour_tick(ctx);
        self.poll_background();

        if self.last_refresh.elapsed() >= Duration::from_secs(2) && !self.bg_busy {
            self.last_refresh = Instant::now();
            self.reload_overview();
            self.refresh_status();
            if let Selection::Project(ref p) = self.selection {
                if let Some(ref mut lp) = self.loaded {
                    if let Ok(db) =
                        Database::open_path(&Path::new(p).join(".trackertop").join("index.db"))
                    {
                        if let Ok(stats) = db.get_token_stats(30) {
                            lp.stats = stats;
                        }
                        if let Ok(notes) = db.list_notes(None) {
                            lp.notes = notes;
                        }
                    }
                } else {
                    self.reload_selected_project();
                }
            }
        }
        if self.last_activity_check.elapsed() >= Duration::from_secs(15) && !self.bg_busy {
            self.last_activity_check = Instant::now();
            self.refresh_activity();
        }
        ctx.request_repaint_after(Duration::from_millis(if self.bg_busy { 100 } else { 750 }));

        // Left sidebar: brand, workspace, projects, agent status.
        let mut to_select: Option<String> = None;
        let mut to_remove: Option<(String, bool)> = None;
        egui::SidePanel::left("projects_sidebar")
            .resizable(true)
            .default_width(232.0)
            .min_width(200.0)
            .frame(
                egui::Frame::NONE
                    .fill(T::PANEL)
                    .inner_margin(Margin::symmetric(10, 14))
                    .stroke(Stroke::new(1.0_f32, T::BORDER)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
                    let p = ui.painter();
                    p.circle_stroke(r.center(), 10.0, Stroke::new(1.0_f32, T::CONTROL_STRONG));
                    p.circle_stroke(r.center(), 5.5, Stroke::new(1.5_f32, T::ACCENT));
                    p.circle_filled(r.center(), 2.0, T::ACCENT);
                    ui.label(T::strong("TrackerTop", 14.0).color(T::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            T::mono(concat!("v", env!("CARGO_PKG_VERSION")), 11.0)
                                .color(T::TEXT_3),
                        );
                    });
                });
                ui.add_space(T::S4);

                let all_selected = self.selection == Selection::AllProjects;
                if sidebar_row(
                    ui,
                    all_selected,
                    None,
                    "Global workspace",
                    &self.overview.projects.len().to_string(),
                )
                .clicked()
                {
                    self.selection = Selection::AllProjects;
                    self.loaded = None;
                }
                ui.add_space(T::S4);

                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    T::section_label(ui, "Projects");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(!self.bg_busy, T::button("+ Add"))
                            .on_hover_text("Pin a folder and index it")
                            .clicked()
                        {
                            self.pick_and_add_project();
                        }
                    });
                });
                ui.add_space(T::S1);

                let footer_h = 74.0;
                egui::ScrollArea::vertical()
                    .max_height((ui.available_height() - footer_h).max(80.0))
                    .show(ui, |ui| {
                        if self.overview.projects.is_empty() {
                            T::card().show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.label(T::strong("No projects yet", 12.0).color(T::TEXT));
                                ui.add_space(T::S1);
                                ui.label(
                                    RichText::new(
                                        "Add a folder above, or open any repo in Claude Code: it appears here automatically.",
                                    )
                                    .size(12.0)
                                    .color(T::TEXT_3),
                                );
                            });
                        }
                        for snap in &self.overview.projects {
                            let is_sel = matches!(&self.selection, Selection::Project(p) if p.eq_ignore_ascii_case(&snap.entry.path));
                            let resp = sidebar_row(
                                ui,
                                is_sel,
                                Some(if is_sel { T::ACCENT } else { T::CONTROL_STRONG }),
                                &snap.entry.name,
                                &fmt_num(snap.total_files),
                            );
                            let resp = resp.on_hover_text(format!(
                                "{}\n{} files · {} tok{}\nRight-click to remove",
                                snap.entry.path,
                                fmt_num(snap.total_files),
                                fmt_num(snap.total_repo_tokens),
                                if snap.stats.total_queries > 0 {
                                    format!(
                                        " · saved {} tok ({:.0}%)",
                                        fmt_num(snap.stats.total_tokens_saved),
                                        snap.stats.savings_percent
                                    )
                                } else {
                                    String::new()
                                }
                            ));
                            if resp.clicked() {
                                to_select = Some(snap.entry.path.clone());
                            }
                            resp.context_menu(|ui| {
                                if ui.button("Remove from list").clicked() {
                                    to_remove = Some((snap.entry.path.clone(), false));
                                    ui.close_menu();
                                }
                                if ui
                                    .button("Remove and delete local .trackertop cache")
                                    .clicked()
                                {
                                    to_remove = Some((snap.entry.path.clone(), true));
                                    ui.close_menu();
                                }
                            });
                        }
                    });

                // Agent status card, pinned to the bottom.
                {
                    ui.add_space((ui.available_height() - 60.0).max(0.0));
                    let connected = self.connected_agents.iter().filter(|(_, ok)| *ok).count();
                    let resp = T::card()
                        .inner_margin(Margin::symmetric(12, 10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.vertical(|ui| {
                            T::dot_label(
                                ui,
                                if connected > 0 { T::SUCCESS } else { T::CONTROL_STRONG },
                                T::strong(
                                    if connected > 0 {
                                        format!("{connected} AI agent(s) connected")
                                    } else {
                                        "No AI agent connected".to_string()
                                    },
                                    12.0,
                                )
                                .color(T::TEXT),
                            );
                            ui.label(
                                RichText::new("Manage MCP connections")
                                    .size(11.0)
                                    .color(T::TEXT_3),
                            );
                            });
                        })
                        .response
                        .interact(Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.clicked() {
                        self.show_agents_modal = true;
                    }
                }
            });

        if let Some((rem, wipe)) = to_remove {
            let _ = remove_project_with_cache(&rem, wipe);
            if matches!(&self.selection, Selection::Project(p) if p.eq_ignore_ascii_case(&rem)) {
                self.selection = Selection::AllProjects;
                self.loaded = None;
            }
            self.notify(if wipe {
                "Removed project and deleted its local .trackertop index."
            } else {
                "Removed project from sidebar (local .trackertop cache kept; re-appears if Claude Code opens it)."
            });
            self.reload_overview();
            self.refresh_status();
        }
        if let Some(sel) = to_select {
            self.selection = Selection::Project(sel);
            self.reload_selected_project();
        }

        // Header: project, branch-free path, index summary, search-free actions.
        egui::TopBottomPanel::top("top_bar")
            .exact_height(52.0)
            .frame(
                egui::Frame::NONE
                    .fill(T::BG)
                    .inner_margin(Margin::symmetric(20, 0))
                    .stroke(Stroke::new(1.0_f32, T::BORDER)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    match &self.selection {
                        Selection::AllProjects => {
                            ui.label(T::strong("Global workspace", 14.0).color(T::TEXT));
                            ui.label(
                                RichText::new(format!("{} projects", self.overview.projects.len()))
                                    .size(12.0)
                                    .color(T::TEXT_3),
                            );
                        }
                        Selection::Project(path) => {
                            let name = Path::new(path)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or(path);
                            ui.label(T::strong(name, 14.0).color(T::TEXT));
                            ui.label(T::mono(path, 11.0).color(T::TEXT_3));
                            if let Some(lp) = &self.loaded {
                                ui.label(
                                    RichText::new(format!(
                                        "· {} files · {} symbols",
                                        fmt_num(lp.total_files),
                                        fmt_num(lp.total_symbols)
                                    ))
                                    .size(12.0)
                                    .color(T::TEXT_3),
                                );
                            }
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Selection::Project(ref p) = self.selection.clone()
                            && ui
                                .add_enabled(!self.bg_busy, T::button("Re-index"))
                                .on_hover_text("Rebuild this project's index from scratch")
                                .clicked()
                        {
                            self.start_bg_index(PathBuf::from(p), true, false);
                        }
                        let connected = self.connected_agents.iter().filter(|(_, ok)| *ok).count();
                        if connected == 0
                            && ui.add(T::primary_button("Connect AI agents")).clicked()
                        {
                            self.show_agents_modal = true;
                        }
                    });
                });
            });

        // Status bar: index state and the agent's latest call (`ui/status_bar.rs`).
        self.render_status_bar(ctx);

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(T::BG).inner_margin(Margin::ZERO))
            .show(ctx, |ui| match &self.selection {
                Selection::AllProjects => {
                    egui::Frame::NONE
                        .inner_margin(Margin::same(20))
                        .show(ui, |ui| self.render_all_projects(ui));
                }
                Selection::Project(_) => self.render_single_project(ui),
            });

        if self.show_agents_modal {
            self.render_agents_modal(ctx);
        }
    }
}

pub fn run_desktop_app(initial_repo: Option<PathBuf>) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("TrackerTop — AI Codebase Graph & Token Savings")
            .with_icon(std::sync::Arc::new(build_app_icon()))
            .with_inner_size([1300.0, 820.0])
            .with_min_inner_size([900.0, 580.0]),
        ..Default::default()
    };
    eframe::run_native(
        "TrackerTop",
        options,
        Box::new(move |cc| {
            T::install(&cc.egui_ctx);
            Ok(Box::new(TrackerTopApp::new(initial_repo)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI error: {e}"))
}

/// Procedurally renders a crisp 64x64 anti-aliased TrackerTop node-graph "T" icon
/// so the native window & taskbar display a custom brand emblem across Windows, macOS, and Linux.
fn build_app_icon() -> egui::IconData {
    const S: usize = 64;
    let mut rgba = vec![0u8; S * S * 4];

    // (x, y, radius, rgb)
    type GlowNode = (f32, f32, f32, (f32, f32, f32));
    let nodes: [GlowNode; 4] = [
        (17.0, 20.0, 6.2, (56.0, 189.0, 248.0)),  // top-left cyan
        (32.0, 20.0, 7.2, (110.0, 231.0, 183.0)), // top-center emerald hub
        (47.0, 20.0, 6.2, (129.0, 140.0, 248.0)), // top-right indigo
        (32.0, 46.0, 6.8, (52.0, 211.0, 153.0)),  // bottom-center emerald
    ];
    let segments: [((f32, f32), (f32, f32)); 2] = [
        ((17.0, 20.0), (47.0, 20.0)), // horizontal T bar
        ((32.0, 20.0), (32.0, 46.0)), // vertical T stem
    ];

    for y in 0..S {
        for x in 0..S {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;
            // Rounded square SDF
            let cx = (fx - 32.0).abs() - 20.0;
            let cy = (fy - 32.0).abs() - 20.0;
            let sdf = cx.max(0.0).hypot(cy.max(0.0)) + cx.max(cy).min(0.0) - 10.0;
            if sdf > 1.0 {
                continue;
            }
            let bg_alpha = (1.0 - sdf).clamp(0.0, 1.0);

            // Deep midnight rounded background with soft top-left glow
            let g_dist = ((fx - 18.0).powi(2) + (fy - 16.0).powi(2)).sqrt() / 48.0;
            let glow = (-g_dist * g_dist * 2.2).exp() * 0.28;
            let mut r = 11.0 + (56.0 - 11.0) * glow;
            let mut g = 17.0 + (189.0 - 17.0) * glow;
            let mut b = 32.0 + (248.0 - 32.0) * glow;

            // Subtle glowing border
            if sdf > -2.0 && sdf <= 0.5 {
                let edge_t = (1.0 - (sdf + 0.75).abs() / 1.4).clamp(0.0, 1.0);
                r += (56.0 - r) * edge_t * 0.55;
                g += (189.0 - g) * edge_t * 0.55;
                b += (248.0 - b) * edge_t * 0.55;
            }

            // Draw T-bar edges
            for &((x1, y1), (x2, y2)) in &segments {
                let vx = x2 - x1;
                let vy = y2 - y1;
                let len2 = (vx * vx + vy * vy).max(1.0);
                let t = (((fx - x1) * vx + (fy - y1) * vy) / len2).clamp(0.0, 1.0);
                let px = x1 + t * vx;
                let py = y1 + t * vy;
                let d = (fx - px).hypot(fy - py);
                if d < 4.5 {
                    let a = ((3.4 - d) / 1.5).clamp(0.0, 1.0);
                    r = r * (1.0 - a) + 125.0 * a;
                    g = g * (1.0 - a) + 211.0 * a;
                    b = b * (1.0 - a) + 252.0 * a;
                }
            }

            // Draw glowing graph nodes
            for &(nx, ny, nr, (cr, cg, cb)) in &nodes {
                let d = (fx - nx).hypot(fy - ny);
                if d < nr + 5.0 {
                    let halo = (-(d * d) / (nr * nr * 2.2)).exp() * 0.45;
                    r += (cr - r) * halo;
                    g += (cg - g) * halo;
                    b += (cb - b) * halo;
                }
                if d < nr + 1.0 {
                    let a = (nr + 0.5 - d).clamp(0.0, 1.0);
                    r = r * (1.0 - a) + cr * a;
                    g = g * (1.0 - a) + cg * a;
                    b = b * (1.0 - a) + cb * a;
                }
            }

            let idx = (y * S + x) * 4;
            rgba[idx] = r.clamp(0.0, 255.0) as u8;
            rgba[idx + 1] = g.clamp(0.0, 255.0) as u8;
            rgba[idx + 2] = b.clamp(0.0, 255.0) as u8;
            rgba[idx + 3] = (bg_alpha * 255.0) as u8;
        }
    }

    egui::IconData {
        rgba,
        width: S as u32,
        height: S as u32,
    }
}

fn poll_agent_statuses() -> Vec<(AiAgentKind, bool)> {
    AiAgentKind::all()
        .into_iter()
        .map(|a| (a, is_agent_installed(a)))
        .collect()
}

fn load_project_details(repo_root: &Path) -> Result<LoadedProject> {
    let db = Database::open_for_repo(repo_root)?;
    let cg = CodeGraph::load(&db)?;
    let stats = db.get_token_stats(30)?;
    let notes = db.list_notes(None)?;
    let health = cg.health_report();
    let activity = load_activity(&db, &cg, repo_root);
    let mut findings: HashMap<String, Vec<String>> = HashMap::new();
    for (path, tokens, lines) in &health.bloated_files {
        findings.entry(path.clone()).or_default().push(format!(
            "Large file: {} tokens, {} lines",
            fmt_num(*tokens),
            fmt_num(*lines)
        ));
    }
    for group in &health.circular_groups {
        for path in group {
            findings
                .entry(path.clone())
                .or_default()
                .push(format!("In a dependency cycle of {} files", group.len()));
        }
    }
    for path in &health.orphan_files {
        findings
            .entry(path.clone())
            .or_default()
            .push("No incoming references (orphan)".to_string());
    }
    let (community_color, community_label, community_files) = {
        let ng = NestedGraph::new(&cg);
        let pv = ng.view(&NestedLevel::Project)?;
        let color: HashMap<String, usize> = pv
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.clone(), i))
            .collect();
        let label: HashMap<String, String> = pv
            .nodes
            .iter()
            .map(|n| (n.id.clone(), n.label.clone()))
            .collect();
        let mut files: HashMap<String, Vec<String>> = HashMap::new();
        for path in cg.files.keys() {
            if let Some(c) = ng.community_id_of(path) {
                files.entry(c.to_string()).or_default().push(path.clone());
            }
        }
        (color, label, files)
    };
    let reg = default_registry();

    let mut file_ranks: HashMap<String, f64> = HashMap::new();
    for s in cg.symbols.values() {
        let pr = *cg.pagerank.get(&s.id).unwrap_or(&0.0);
        *file_ranks.entry(s.file_path.clone()).or_insert(0.0) += pr;
    }

    let mut lang_map: HashMap<String, (usize, usize)> = HashMap::new();
    for f in cg.files.values() {
        let e = lang_map.entry(f.language.clone()).or_insert((0, 0));
        e.0 += 1;
        e.1 += f.token_count;
    }
    let mut lang_breakdown: Vec<(String, String, String, usize, usize)> = lang_map
        .into_iter()
        .map(|(id, (fc, tc))| {
            let disp = reg.display_name_for(&id);
            let hex = reg.color_for(&id).to_string();
            (id, disp, hex, fc, tc)
        })
        .collect();
    lang_breakdown.sort_by_key(|l| std::cmp::Reverse(l.4));

    let mut sorted_files: Vec<_> = cg.files.values().collect();
    sorted_files.sort_by(|a, b| a.path.cmp(&b.path));

    let mut idx_by_path = HashMap::new();
    let mut nodes = Vec::with_capacity(sorted_files.len());

    for (i, f) in sorted_files.into_iter().enumerate() {
        idx_by_path.insert(f.path.clone(), i);
        let mut syms: Vec<(String, String, usize, String)> = cg
            .symbols_by_file
            .get(&f.path)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| cg.symbols.get(id))
                    .filter(|s| s.kind.is_counted())
                    .map(|s| {
                        (
                            s.qualified_name.clone(),
                            s.kind.as_str().to_string(),
                            s.start_line,
                            s.signature.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        syms.sort_by_key(|s| s.2);

        let rank = *file_ranks.get(&f.path).unwrap_or(&0.01);
        nodes.push(GraphNodeVis {
            id: f.path.clone(),
            label: f.path.rsplit('/').next().unwrap_or(&f.path).to_string(),
            group_folder: folder_bucket(&f.path),
            language: f.language.clone(),
            tokens: f.token_count,
            rank,
            degree: 0,
            symbols: syms,
        });
    }

    let mut edge_seen = HashSet::new();
    let mut edges = Vec::new();
    for e in &cg.edges {
        if e.kind == EdgeKind::Contains {
            continue;
        }
        let (Some(s), Some(t)) = (cg.symbols.get(&e.source_id), cg.symbols.get(&e.target_id))
        else {
            continue;
        };
        if s.file_path == t.file_path {
            continue;
        }
        if let (Some(&si), Some(&ti)) =
            (idx_by_path.get(&s.file_path), idx_by_path.get(&t.file_path))
            && edge_seen.insert((si, ti))
        {
            nodes[si].degree += 1;
            nodes[ti].degree += 1;
            edges.push(GraphEdgeVis { src: si, tgt: ti });
        }
    }

    let total_symbols = cg.symbols.values().filter(|s| s.kind.is_counted()).count();
    let total_repo_tokens = cg.files.values().map(|f| f.token_count).sum();

    Ok(LoadedProject {
        path: repo_root.to_string_lossy().to_string(),
        total_files: cg.files.len(),
        total_symbols,
        total_edges: cg.edges.len(),
        total_repo_tokens,
        lang_breakdown,
        god_nodes: cg.god_nodes(15, false),
        communities: cg.communities(),
        health,
        notes,
        stats,
        nodes,
        edges,
        activity,
        activity_gen: 0,
        file_rank: file_ranks,
        community_color,
        community_label,
        community_files,
        findings,
        graph: cg,
    })
}

fn folder_bucket(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    match parts.len() {
        0 | 1 => "root".to_string(),
        2 => parts[0].to_string(),
        _ => format!("{}/{}", parts[0], parts[1]),
    }
}

fn soft_card_frame(_radius: u8) -> egui::Frame {
    T::card()
}

fn glow_kpi_card(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    sub: &str,
    text_col: Color32,
    _accent: Color32,
) {
    T::kpi_card(ui, label, value, sub, text_col);
}

/// A sidebar row: optional status dot, label, right-aligned mono count.
fn sidebar_row(
    ui: &mut egui::Ui,
    selected: bool,
    dot: Option<Color32>,
    label: &str,
    count: &str,
) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    let p = ui.painter_at(rect);
    let fill = if selected {
        T::HOVER
    } else if resp.hovered() {
        Color32::from_rgb(0x14, 0x18, 0x1D)
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(rect, CornerRadius::same(T::R_CONTROL), fill);
    let mut x = rect.left() + 10.0;
    if let Some(c) = dot {
        p.circle_filled(Pos2::new(x + 3.0, rect.center().y), 3.0, c);
        x += 14.0;
    }
    let cg = p.layout_no_wrap(count.to_string(), T::mono_font(11.0), T::TEXT_3);
    let max_w = (rect.right() - 10.0 - cg.size().x - 8.0 - x).max(20.0);
    let color = if selected { T::TEXT } else { T::TEXT_2 };
    let g = p.layout(label.to_string(), T::ui_font(13.0), color, max_w);
    let g = if g.rows.len() > 1 {
        p.layout_no_wrap(truncate_label(label, 22), T::ui_font(13.0), color)
    } else {
        g
    };
    p.galley(Pos2::new(x, rect.center().y - g.size().y * 0.5), g, color);
    p.galley(
        Pos2::new(
            rect.right() - 10.0 - cg.size().x,
            rect.center().y - cg.size().y * 0.5,
        ),
        cg,
        T::TEXT_3,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Agent activity for a project: MCP telemetry plus uncommitted changes.
fn load_activity(db: &Database, cg: &CodeGraph, repo_root: &Path) -> AgentActivity {
    let rows = db.agent_telemetry(5000).unwrap_or_default();
    let resolver = QueryResolver::new(&cg.files, &cg.symbols);
    build_activity(
        &rows,
        &resolver,
        crate::git::working_tree_changes(repo_root),
        chrono::Utc::now(),
    )
}

/// Cheap fingerprint to tell whether reloaded activity differs from what is shown.
fn activity_fingerprint(a: &AgentActivity) -> (usize, usize, usize, Option<String>, usize) {
    (
        a.calls,
        a.attributed,
        a.session.len(),
        a.session.last().map(|s| s.timestamp.clone()),
        a.changed.len(),
    )
}

fn fmt_num(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

/// Shortens a label from the front, keeping its distinctive end (`...kuzgun/tests/a`,
/// `...App::render`). Cutting the end made sibling paths identical.
fn truncate_label_tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let keep = max.saturating_sub(3);
        let tail: String = s.chars().skip(count - keep).collect();
        format!("...{tail}")
    }
}

fn truncate_label(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(3)).collect();
        t.push_str("...");
        t
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_label_tail;

    #[test]
    fn tail_truncation_keeps_sibling_paths_distinct() {
        let a = truncate_label_tail("tools/kuzgun/tests/fixtures/alpha.rs", 20);
        let b = truncate_label_tail("tools/kuzgun/tests/fixtures/beta.rs", 20);
        assert_ne!(a, b);
        assert!(a.ends_with("alpha.rs") && a.starts_with("..."));
        assert_eq!(a.chars().count(), 20);
        assert_eq!(truncate_label_tail("short", 20), "short");
        // Multi-byte characters are cut on char boundaries.
        assert_eq!(truncate_label_tail("çğüşöıçğüşö", 6), "...üşö");
    }
}
