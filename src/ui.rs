#![cfg(feature = "gui")]

mod graph_view;
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
use crate::indexer::index_repository;
use crate::install::{AiAgentKind, generic_mcp_json_snippet, install_agent, is_agent_installed};
use crate::model::{EdgeKind, ProjectNote, SymbolKind, TokenStatsSummary};
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
    status: String,
    select_path: Option<String>,
}

pub struct TrackerTopApp {
    overview: GlobalOverview,
    selection: Selection,
    active_tab: ProjectTab,
    loaded: Option<LoadedProject>,
    connected_agents: Vec<(AiAgentKind, bool)>,
    show_agents_modal: bool,
    status_banner: String,
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
            status_banner: "Ready — 100% offline local engine".to_string(),
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
        self.status_banner = format!("Indexing {}...", repo_path.display());

        std::thread::spawn(move || {
            let msg = match index_repository(&repo_path, force) {
                Ok(stats) => BgMessage {
                    status: format!(
                        "Indexed {} files ({} symbols, {} edges) in {} ms",
                        stats.total_files, stats.total_symbols, stats.total_edges, stats.elapsed_ms
                    ),
                    select_path: if select_after {
                        // Same key the registry stores, by construction
                        Some(crate::registry::canonical_path_string(&repo_path))
                    } else {
                        None
                    },
                },
                Err(e) => BgMessage {
                    status: format!("Index error: {e}"),
                    select_path: None,
                },
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
                self.status_banner = format!(
                    "Dry-run preview (not counted in stats): {} tok vs {} baseline -> {:.1}% reduction in {} ms",
                    fmt_num(out.tokens_sent),
                    fmt_num(out.baseline_tokens),
                    pct,
                    out.latency_ms
                );
                self.preview_result = Some(out);
            }
            Err(e) => {
                self.status_banner = format!("Preview error: {e}");
            }
        }
    }

    fn reset_project_stats(&mut self, repo_path: &str) {
        let result = Database::open_for_repo(Path::new(repo_path))
            .and_then(|db| db.clear_telemetry().map(|()| db));
        match result {
            Ok(db) => {
                self.status_banner =
                    "Reset token-savings telemetry to 0 for this project.".to_string();
                self.reload_overview();
                if let Some(ref mut lp) = self.loaded {
                    lp.stats = db.get_token_stats(30).unwrap_or_default();
                }
            }
            Err(e) => self.status_banner = format!("Could not reset stats: {e:#}"),
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
                self.status_banner = msg.status;
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
                self.status_banner =
                    "Indexing stopped unexpectedly. Please try Re-Index again.".to_string();
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(debug_assertions)]
        self.tour_tick(ctx);
        self.poll_background();

        if self.last_refresh.elapsed() >= Duration::from_secs(2) && !self.bg_busy {
            self.last_refresh = Instant::now();
            self.reload_overview();
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
            self.status_banner = if wipe {
                "Removed project and deleted its local .trackertop index.".to_string()
            } else {
                "Removed project from sidebar (local .trackertop cache kept; re-appears if Claude Code opens it).".to_string()
            };
            self.reload_overview();
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

        // Status bar.
        egui::TopBottomPanel::bottom("status_bar")
            .frame(
                egui::Frame::NONE
                    .fill(T::PANEL)
                    .inner_margin(Margin::symmetric(16, 5))
                    .stroke(Stroke::new(1.0_f32, T::BORDER)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    if self.bg_busy {
                        ui.spinner();
                    }
                    ui.label(
                        RichText::new(&self.status_banner)
                            .size(11.0)
                            .color(T::TEXT_3),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let langs: Vec<String> = default_registry()
                            .languages()
                            .into_iter()
                            .map(|l| l.display_name)
                            .collect();
                        ui.label(RichText::new(langs.join(" · ")).size(11.0).color(T::TEXT_3));
                    });
                });
            });

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

impl TrackerTopApp {
    fn render_agents_modal(&mut self, ctx: &egui::Context) {
        let mut open = self.show_agents_modal;
        egui::Window::new("Connect AI Coding Agents")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .fixed_size(Vec2::new(520.0, 420.0))
            .frame(
                egui::Frame::NONE
                    .fill(T::PANEL)
                    .stroke(Stroke::new(
                        1.0_f32,
                        T::ACCENT_LINE,
                    ))
                    .corner_radius(CornerRadius::same(T::R_CARD))
                    .inner_margin(Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Connect TrackerTop once globally — every repository you open in any connected AI agent is indexed and tracked automatically.",
                    )
                    .small()
                    .color(T::TEXT_3),
                );
                ui.add_space(12.0);

                let base = match &self.selection {
                    Selection::Project(p) => PathBuf::from(p),
                    Selection::AllProjects => {
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                    }
                };

                let mut agent_to_install: Option<AiAgentKind> = None;
                for &(agent, connected) in &self.connected_agents {
                    soft_card_frame(14).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new(agent.display_name())
                                        .family(T::semibold_family())
                                        .color(Color32::WHITE),
                                );
                                ui.label(
                                    RichText::new(agent.description())
                                        .small()
                                        .color(T::TEXT_3),
                                );
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let label = if connected { "Connected ✓" } else { "Connect" };
                                let btn = egui::Button::new(RichText::new(label).family(T::semibold_family()).color(
                                    if connected {
                                        T::SUCCESS
                                    } else {
                                        Color32::WHITE
                                    },
                                ))
                                .corner_radius(CornerRadius::same(T::R_CARD))
                                .fill(if connected {
                                    T::CARD
                                } else {
                                    T::ACCENT
                                });
                                if ui.add(btn).clicked() {
                                    agent_to_install = Some(agent);
                                }
                            });
                        });
                    });
                    ui.add_space(8.0);
                }

                if let Some(agent) = agent_to_install {
                    match install_agent(agent, &base) {
                        Ok(lines) => {
                            self.connected_agents = poll_agent_statuses();
                            self.status_banner = lines.join(" | ");
                        }
                        Err(e) => {
                            self.status_banner = format!("Install error: {e}");
                        }
                    }
                }

                ui.add_space(6.0);
                soft_card_frame(14).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Other MCP Clients (VS Code, Zed, Cline, Antigravity)")
                                .family(T::semibold_family())
                                .small(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(
                                    egui::Button::new("Copy MCP JSON")
                                        .corner_radius(CornerRadius::same(T::R_CARD)),
                                )
                                .clicked()
                            {
                                ui.ctx().copy_text(generic_mcp_json_snippet());
                                self.status_banner = "Copied universal MCP JSON to clipboard!".to_string();
                            }
                        });
                    });
                });
            });
        self.show_agents_modal = open;
    }

    fn render_all_projects(&mut self, ui: &mut egui::Ui) {
        let ov = &self.overview;
        ui.columns(4, |cols| {
            glow_kpi_card(
                &mut cols[0],
                "TRACKED PROJECTS",
                &ov.projects.len().to_string(),
                &format!(
                    "{} files · {} symbols",
                    fmt_num(ov.total_files),
                    fmt_num(ov.total_symbols)
                ),
                Color32::WHITE,
                T::ACCENT,
            );
            glow_kpi_card(
                &mut cols[1],
                "TOTAL AI QUERIES",
                &fmt_num(ov.total_queries),
                &format!("Sent {} tok", fmt_num(ov.total_tokens_sent)),
                T::ACCENT,
                T::ACCENT,
            );
            glow_kpi_card(
                &mut cols[2],
                "NET TOKENS SAVED",
                &fmt_num(ov.total_tokens_saved),
                &format!("{:.1}% reduction vs raw reads", ov.savings_percent),
                T::SUCCESS,
                T::SUCCESS,
            );
            glow_kpi_card(
                &mut cols[3],
                "EST. USD SAVED",
                &format!("${:.4}", ov.est_usd_saved_sonnet),
                &format!("Sonnet · ${:.4} Opus", ov.est_usd_saved_opus),
                T::SUCCESS,
                T::SUCCESS,
            );
        });

        ui.add_space(18.0);

        // Clean Empty State when 0 projects are listed (no awkward empty table headers!)
        if ov.projects.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                soft_card_frame(20).show(ui, |ui| {
                    ui.set_max_width(540.0);
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Welcome to TrackerTop")
                            .family(T::semibold_family())
                            .size(20.0)
                            .color(Color32::WHITE),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "Add any project folder to inspect its architecture and token savings, or simply open a repository in Claude Code — TrackerTop tracks it automatically in the background.",
                        )
                        .color(T::TEXT_3),
                    );
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        ui.add_space(100.0);
                        let add = egui::Button::new(
                            RichText::new("+ Add Your First Project")
                                .family(T::semibold_family())
                                .color(Color32::WHITE),
                        )
                        .corner_radius(CornerRadius::same(T::R_CARD))
                        .fill(T::ACCENT);
                        if ui.add(add).clicked() {
                            self.pick_and_add_project();
                        }
                        ui.add_space(8.0);
                        if ui
                            .add(
                                egui::Button::new("Connect AI Agents")
                                    .corner_radius(CornerRadius::same(T::R_CARD)),
                            )
                            .clicked()
                        {
                            self.show_agents_modal = true;
                        }
                    });
                    ui.add_space(8.0);
                });
            });
            return;
        }

        let mut open_path: Option<String> = None;
        soft_card_frame(16).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new("PROJECTS BREAKDOWN")
                    .family(T::semibold_family())
                    .small()
                    .color(T::TEXT_3),
            );
            ui.add_space(10.0);
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("global_projects_grid")
                    .striped(true)
                    .min_col_width(92.0)
                    .spacing([18.0, 10.0])
                    .show(ui, |ui| {
                        for h in [
                            "Project",
                            "Files",
                            "Repo Tokens",
                            "Queries",
                            "Tokens Saved",
                            "Savings %",
                            "",
                        ] {
                            ui.label(
                                RichText::new(h)
                                    .family(T::semibold_family())
                                    .small()
                                    .color(T::TEXT_3),
                            );
                        }
                        ui.end_row();

                        for p in &ov.projects {
                            ui.label(
                                RichText::new(&p.entry.name)
                                    .family(T::semibold_family())
                                    .color(Color32::WHITE),
                            );
                            ui.label(fmt_num(p.total_files));
                            ui.label(fmt_num(p.total_repo_tokens));
                            ui.label(fmt_num(p.stats.total_queries));
                            ui.label(
                                RichText::new(fmt_num(p.stats.total_tokens_saved))
                                    .family(T::semibold_family())
                                    .color(T::SUCCESS),
                            );
                            ui.label(format!("{:.1}%", p.stats.savings_percent));
                            if ui
                                .add(
                                    egui::Button::new("Open")
                                        .corner_radius(CornerRadius::same(T::R_CARD)),
                                )
                                .clicked()
                            {
                                open_path = Some(p.entry.path.clone());
                            }
                            ui.end_row();
                        }
                    });
            });
        });

        if let Some(p) = open_path {
            self.selection = Selection::Project(p);
            self.reload_selected_project();
        }
    }

    fn render_single_project(&mut self, ui: &mut egui::Ui) {
        // Tab bar: underlined tabs with counts.
        let (bar, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 41.0), Sense::hover());
        ui.painter().line_segment(
            [bar.left_bottom(), bar.right_bottom()],
            Stroke::new(1.0_f32, T::BORDER),
        );
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(bar.shrink2(Vec2::new(20.0, 0.0))),
            |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 22.0;
                    let (files, notes) = self
                        .loaded
                        .as_ref()
                        .map(|l| {
                            (
                                Some(fmt_num(l.total_files)),
                                Some(l.notes.len().to_string()),
                            )
                        })
                        .unwrap_or((None, None));
                    for (tab, label, count) in [
                        (ProjectTab::Savings, "Savings", None),
                        (ProjectTab::Graph, "Code Graph", files),
                        (ProjectTab::Architecture, "Architecture", None),
                        (ProjectTab::Memory, "Memory", notes),
                    ] {
                        let active = self.active_tab == tab;
                        if T::tab(ui, label, count, active).clicked() {
                            self.active_tab = tab;
                        }
                    }
                });
            },
        );

        if self.loaded.is_none() {
            egui::Frame::NONE
                .inner_margin(Margin::same(20))
                .show(ui, |ui| {
                    T::card().show(ui, |ui| {
                        ui.label("This project is not indexed yet. Click Re-index in the header.");
                    });
                });
            return;
        }

        if self.active_tab == ProjectTab::Graph {
            self.render_graph_tab(ui);
            return;
        }
        egui::Frame::NONE
            .inner_margin(Margin::same(20))
            .show(ui, |ui| match self.active_tab {
                ProjectTab::Savings => self.render_savings_tab(ui),
                ProjectTab::Architecture => self.render_architecture_tab(ui),
                ProjectTab::Memory => self.render_memory_tab(ui),
                ProjectTab::Graph => {}
            });
    }

    fn render_savings_tab(&mut self, ui: &mut egui::Ui) {
        let mut run_preview_for: Option<String> = None;
        let mut reset_stats_for: Option<String> = None;
        let mut clear_preview = false;
        let Some(ref lp) = self.loaded else {
            return;
        };
        let s = &lp.stats;

        ui.columns(4, |cols| {
            glow_kpi_card(
                &mut cols[0],
                "TOKENS SAVED (RANGE)",
                &format!(
                    "{}–{}",
                    fmt_num(s.conservative_tokens_saved),
                    fmt_num(s.total_tokens_saved)
                ),
                &format!(
                    "{:.0}–{:.0}% · conservative–upper · AI agents only",
                    s.conservative_savings_percent, s.savings_percent
                ),
                T::SUCCESS,
                T::SUCCESS,
            );
            glow_kpi_card(
                &mut cols[1],
                "SENT VS BASELINE",
                &fmt_num(s.total_tokens_sent),
                &format!("vs {} raw baseline tok", fmt_num(s.total_baseline_tokens)),
                T::ACCENT,
                T::ACCENT,
            );
            glow_kpi_card(
                &mut cols[2],
                "AI QUERIES LOGGED",
                &fmt_num(s.total_queries),
                &format!(
                    "{} files · {} tok indexed",
                    fmt_num(lp.total_files),
                    fmt_num(lp.total_repo_tokens)
                ),
                Color32::WHITE,
                T::ACCENT,
            );
            glow_kpi_card(
                &mut cols[3],
                "EST. USD SAVED",
                &format!("${:.4}", s.est_usd_saved_sonnet),
                &format!("Sonnet · ${:.4} Opus", s.est_usd_saved_opus),
                T::SUCCESS,
                T::SUCCESS,
            );
        });

        ui.add_space(14.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            if s.total_queries == 0 {
                egui::Frame::NONE
                    .fill(T::CARD)
                    .stroke(Stroke::new(
                        1.0_f32,
                        T::SUCCESS,
                    ))
                    .corner_radius(CornerRadius::same(T::R_CARD))
                    .inner_margin(Margin::same(16))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new(format!(
                                "Indexed & Ready — {} files ({} symbols, {} tokens) mapped!",
                                fmt_num(lp.total_files),
                                fmt_num(lp.total_symbols),
                                fmt_num(lp.total_repo_tokens)
                            ))
                            .family(T::semibold_family())
                            .size(14.5)
                            .color(T::SUCCESS),
                        );
                        ui.add_space(5.0);
                        ui.label(
                            RichText::new(
                                "TrackerTop works immediately on any existing or new project. Open this folder in Claude Code (or Cursor / Codex) and ask a question — real token savings will stream in automatically.",
                            )
                            .color(T::TEXT_2),
                        );
                        ui.add_space(10.0);
                        let test_btn = egui::Button::new(
                            RichText::new("Preview Savings (Dry-Run)")
                                .family(T::semibold_family())
                                .color(Color32::WHITE),
                        )
                        .corner_radius(CornerRadius::same(T::R_CARD))
                        .fill(T::SUCCESS);
                        if ui
                            .add(test_btn)
                            .on_hover_text("Simulates an AI repo_map query without inflating your real stats counter")
                            .clicked()
                        {
                            run_preview_for = Some(lp.path.clone());
                        }
                    });
                ui.add_space(12.0);
            }

            // Dismissible Dry-Run Preview Card (never touches real telemetry!)
            if let Some(ref prev) = self.preview_result {
                let pct = if prev.baseline_tokens > 0 {
                    (prev.tokens_saved as f64 / prev.baseline_tokens as f64) * 100.0
                } else {
                    0.0
                };
                egui::Frame::NONE
                    .fill(T::ACCENT_SOFT)
                    .stroke(Stroke::new(
                        1.0_f32,
                        T::ACCENT_LINE,
                    ))
                    .corner_radius(CornerRadius::same(T::R_CARD))
                    .inner_margin(Margin::same(15))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "DRY-RUN PREVIEW (NOT COUNTED IN STATS): {} tok sent vs {} baseline -> {:.1}% saved ({} ms)",
                                    fmt_num(prev.tokens_sent),
                                    fmt_num(prev.baseline_tokens),
                                    pct,
                                    prev.latency_ms
                                ))
                                .family(T::semibold_family())
                                .small()
                                .color(T::ACCENT),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui
                                    .add(egui::Button::new("Close").corner_radius(CornerRadius::same(T::R_CARD)))
                                    .clicked()
                                {
                                    clear_preview = true;
                                }
                            });
                        });
                        ui.add_space(6.0);
                        egui::ScrollArea::vertical()
                            .id_salt("preview_output_scroll")
                            .max_height(150.0)
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(&prev.text)
                                        .monospace()
                                        .small()
                                        .color(T::TEXT_2),
                                );
                            });
                    });
                ui.add_space(12.0);
            }

            soft_card_frame(16).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("SAVINGS BY MCP TOOL (REAL AI USAGE)")
                            .family(T::semibold_family())
                            .small()
                            .color(T::TEXT_3),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if s.total_queries > 0 {
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("Reset Stats")
                                            .small()
                                            .color(T::DANGER),
                                    )
                                    .corner_radius(CornerRadius::same(T::R_CARD)),
                                )
                                .on_hover_text("Clear recorded telemetry back to 0 for this project")
                                .clicked()
                            {
                                reset_stats_for = Some(lp.path.clone());
                            }
                            ui.add_space(6.0);
                            if ui
                                .add(
                                    egui::Button::new("Preview Savings")
                                        .corner_radius(CornerRadius::same(T::R_CARD)),
                                )
                                .on_hover_text("Dry-run preview — does NOT add to your real token savings counter")
                                .clicked()
                            {
                                run_preview_for = Some(lp.path.clone());
                            }
                        }
                    });
                });
                ui.add_space(8.0);
                if s.by_tool.is_empty() {
                    ui.label(
                        RichText::new("Waiting for the first real AI query in this project...")
                            .color(T::TEXT_3),
                    );
                } else {
                    for t in &s.by_tool {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("{} ({}x)", t.tool, t.queries))
                                    .monospace()
                                    .family(T::semibold_family()),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "-{} tok ({:.1}%)",
                                        fmt_num(t.tokens_saved),
                                        t.savings_percent
                                    ))
                                    .family(T::semibold_family())
                                    .color(T::SUCCESS),
                                );
                            });
                        });
                        let frac = (t.savings_percent as f32 / 100.0).clamp(0.0, 1.0);
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_height(9.0)
                                .corner_radius(CornerRadius::same(5))
                                .fill(T::SUCCESS),
                        );
                        ui.add_space(8.0);
                    }
                }
            });

            ui.add_space(12.0);

            soft_card_frame(16).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("LIVE AI QUERY LOG")
                        .family(T::semibold_family())
                        .small()
                        .color(T::TEXT_3),
                );
                ui.add_space(8.0);
                if s.recent.is_empty() {
                    ui.label(
                        RichText::new(
                            "Live queries from Claude Code, Cursor, or Codex stream in here every 2 seconds.",
                        )
                        .color(T::TEXT_3),
                    );
                } else {
                    egui::Grid::new("recent_queries_grid")
                        .striped(true)
                        .spacing([16.0, 8.0])
                        .show(ui, |ui| {
                            for h in ["Tool", "Query", "Sent", "Baseline", "Saved", "Latency"] {
                                ui.label(
                                    RichText::new(h)
                                        .family(T::semibold_family())
                                        .small()
                                        .color(T::TEXT_3),
                                );
                            }
                            ui.end_row();
                            for r in &s.recent {
                                let pct = if r.baseline_tokens > 0 {
                                    (r.tokens_saved as f64 / r.baseline_tokens as f64) * 100.0
                                } else {
                                    0.0
                                };
                                ui.label(
                                    RichText::new(&r.tool)
                                        .monospace()
                                        .color(T::ACCENT),
                                );
                                ui.label(truncate_label(&r.query, 40));
                                ui.label(fmt_num(r.tokens_sent));
                                ui.label(fmt_num(r.baseline_tokens));
                                ui.label(
                                    RichText::new(format!(
                                        "{} ({:.0}%)",
                                        fmt_num(r.tokens_saved),
                                        pct
                                    ))
                                    .color(T::SUCCESS),
                                );
                                ui.label(format!("{} ms", r.latency_ms));
                                ui.end_row();
                            }
                        });
                }
            });
        });

        if clear_preview {
            self.preview_result = None;
        }
        if let Some(path) = run_preview_for {
            self.run_preview_query(&path);
        }
        if let Some(path) = reset_stats_for {
            self.reset_project_stats(&path);
        }
    }

    fn render_graph_tab(&mut self, ui: &mut egui::Ui) {
        if self.loaded.is_none() {
            return;
        }
        // Nested view of the current level (cached per project and level).
        if self.graph_layout_mode == GraphLayoutMode::Nested {
            let lp = self.loaded.as_ref().expect("checked above");
            let key = (lp.path.clone(), self.nested_level.clone());
            if self.nested_cache.as_ref().map(|(k, _)| k) != Some(&key) {
                let view = NestedGraph::new(&lp.graph)
                    .view(&self.nested_level)
                    .map_err(|e| e.to_string());
                self.nested_cache = Some((key, view));
            }
        }
        let scene_key = self.scene_key();
        if self.scene.as_ref().map(|(k, _)| k) != Some(&scene_key) {
            let lp = self.loaded.as_ref().expect("checked above");
            let scene = match self.graph_layout_mode {
                GraphLayoutMode::Nested => match self.nested_cache.as_ref().map(|(_, v)| v) {
                    Some(Ok(view)) => Some(nested_scene(lp, view)),
                    _ => None,
                },
                mode => {
                    let visible = compute_visible_mask(
                        &lp.nodes,
                        &self.graph_search,
                        &self.graph_lang_filter,
                        self.graph_code_only,
                    );
                    Some(all_files_scene(
                        lp,
                        mode == GraphLayoutMode::Folders,
                        &visible,
                    ))
                }
            };
            self.scene = scene.map(|s| (scene_key, s));
        }

        let full = ui.available_rect_before_wrap();
        let mut next_level: Option<(NestedLevel, Option<Pos2>, Option<String>, bool)> = None;

        // Toolbar.
        let bar = Rect::from_min_size(full.min, Vec2::new(full.width(), 48.0));
        ui.painter().line_segment(
            [bar.left_bottom(), bar.right_bottom()],
            Stroke::new(1.0_f32, T::BORDER),
        );
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(bar.shrink2(Vec2::new(16.0, 0.0))),
            |ui| {
                ui.horizontal_centered(|ui| {
                    let modes = [
                        GraphLayoutMode::Nested,
                        GraphLayoutMode::Clusters,
                        GraphLayoutMode::Folders,
                    ];
                    let cur = modes
                        .iter()
                        .position(|m| *m == self.graph_layout_mode)
                        .unwrap_or(0);
                    if let Some(i) = T::segmented(ui, &["Nested", "Clusters", "Folders"], cur)
                        && i != cur
                    {
                        self.graph_layout_mode = modes[i];
                        self.graph_view.begin_transition(None, None, false);
                    }
                    ui.add_space(T::S2);
                    if self.graph_layout_mode == GraphLayoutMode::Nested {
                        if let Some((_, Ok(view))) = &self.nested_cache {
                            let depth = |l: &NestedLevel| match l {
                                NestedLevel::Project => 0,
                                NestedLevel::Community(_) => 1,
                                NestedLevel::File(_) => 2,
                            };
                            let here = depth(&view.level);
                            for (i, (label, level)) in view.breadcrumbs.iter().enumerate() {
                                if i > 0 {
                                    ui.label(RichText::new("/").color(T::CONTROL_STRONG));
                                }
                                let current = i + 1 == view.breadcrumbs.len();
                                let text = if matches!(level, NestedLevel::File(_)) {
                                    T::mono(truncate_label_tail(label, 40), 12.0)
                                } else {
                                    RichText::new(truncate_label_tail(label, 40)).size(12.0)
                                };
                                let text = text.color(if current { T::TEXT } else { T::TEXT_3 });
                                let resp = ui.add(
                                    egui::Button::new(text)
                                        .fill(Color32::TRANSPARENT)
                                        .stroke(Stroke::NONE)
                                        .min_size(Vec2::new(0.0, 24.0)),
                                );
                                if resp.clicked() && !current {
                                    // Going up: the parent settles around the node we left.
                                    let focus = match (&view.level, depth(level)) {
                                        (NestedLevel::Community(c), _) => Some(c.clone()),
                                        (NestedLevel::File(f), 1) => Some(format!("f:{f}")),
                                        (NestedLevel::File(_), _) => view
                                            .breadcrumbs
                                            .get(1)
                                            .and_then(|(_, l)| match l {
                                                NestedLevel::Community(c) => Some(c.clone()),
                                                _ => None,
                                            }),
                                        _ => None,
                                    };
                                    next_level =
                                        Some((level.clone(), None, focus, depth(level) < here));
                                }
                            }
                        }
                    } else {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.graph_search)
                                .desired_width(170.0)
                                .hint_text("Filter files or symbols"),
                        );
                        let reg = default_registry();
                        egui::ComboBox::from_id_salt("lang_filter")
                            .width(130.0)
                            .selected_text(if self.graph_lang_filter == "all" {
                                "All languages".to_string()
                            } else {
                                reg.display_name_for(&self.graph_lang_filter)
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.graph_lang_filter,
                                    "all".to_string(),
                                    "All languages",
                                );
                                for l in reg.languages() {
                                    ui.selectable_value(
                                        &mut self.graph_lang_filter,
                                        l.id,
                                        l.display_name,
                                    );
                                }
                            });
                        ui.checkbox(&mut self.graph_code_only, "Code only");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let gv = &mut self.graph_view;
                        if T::toggle_chip(ui, "Badges", gv.show_badges, T::ACCENT)
                            .on_hover_text("Notes, decisions and health findings on nodes")
                            .clicked()
                        {
                            gv.show_badges = !gv.show_badges;
                        }
                        if T::toggle_chip(ui, "Trail", gv.show_trail, T::HEAT)
                            .on_hover_text("The latest agent session, step by step")
                            .clicked()
                        {
                            gv.show_trail = !gv.show_trail;
                        }
                        if T::toggle_chip(ui, "Agent heat", gv.show_heat, T::HEAT)
                            .on_hover_text(format!(
                                "Glow = agent calls naming the node (last {} days); dashed ring = uncommitted changes",
                                crate::activity::HEAT_DAYS
                            ))
                            .clicked()
                        {
                            gv.show_heat = !gv.show_heat;
                        }
                    });
                });
            },
        );

        let body = Rect::from_min_max(Pos2::new(full.left(), bar.bottom()), full.max);
        let inspector_w = 300.0_f32.min(body.width() * 0.4);
        let canvas = Rect::from_min_max(
            body.min,
            Pos2::new(body.right() - inspector_w, body.bottom()),
        );
        let side = Rect::from_min_max(Pos2::new(canvas.right(), body.top()), body.max);

        match (&self.scene, self.graph_layout_mode, &self.nested_cache) {
            (_, GraphLayoutMode::Nested, Some((_, Err(msg)))) => {
                let msg = msg.clone();
                ui.scope_builder(egui::UiBuilder::new().max_rect(canvas.shrink(24.0)), |ui| {
                    ui.colored_label(T::DANGER, msg);
                    if ui.add(T::button("Back to Project")).clicked() {
                        next_level = Some((NestedLevel::Project, None, None, true));
                    }
                });
            }
            (Some((_, scene)), _, _) => {
                let lp = self.loaded.as_ref().expect("checked above");
                let out = graph_view::show(
                    ui,
                    canvas,
                    scene,
                    &mut self.graph_view,
                    &lp.activity.session,
                );
                if let Some((level, origin)) = out.drill {
                    next_level = Some((level, Some(origin), None, false));
                }
            }
            _ => {}
        }

        ui.painter().rect_filled(side, CornerRadius::ZERO, T::PANEL);
        ui.painter().line_segment(
            [side.left_top(), side.left_bottom()],
            Stroke::new(1.0_f32, T::BORDER),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(side.shrink(T::S4)), |ui| {
            egui::ScrollArea::vertical()
                .id_salt("graph_inspector")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if let Some(action) = self.render_inspector(ui) {
                        next_level = Some(action);
                    }
                });
        });
        ui.allocate_rect(full, Sense::hover());

        if let Some((level, origin, focus, out)) = next_level {
            self.graph_layout_mode = GraphLayoutMode::Nested;
            self.nested_level = level;
            self.graph_view.begin_transition(origin, focus, out);
        }
    }

    fn scene_key(&self) -> String {
        let path = self.loaded.as_ref().map(|l| l.path.as_str()).unwrap_or("");
        let gen_ = self.loaded.as_ref().map(|l| l.activity_gen).unwrap_or(0);
        match self.graph_layout_mode {
            GraphLayoutMode::Nested => format!("{path}|nested|{:?}|{gen_}", self.nested_level),
            m => format!(
                "{path}|{m:?}|{}|{}|{}|{gen_}",
                self.graph_search.trim().to_lowercase(),
                self.graph_lang_filter,
                self.graph_code_only
            ),
        }
    }

    /// Right-hand panel: the selected node, or the project's agent activity and legend.
    /// Returns a level to open.
    fn render_inspector(
        &mut self,
        ui: &mut egui::Ui,
    ) -> Option<(NestedLevel, Option<Pos2>, Option<String>, bool)> {
        let lp = self.loaded.as_ref()?;
        let (_, scene) = self.scene.as_ref()?;
        let mut action = None;
        let selected = self
            .graph_view
            .selected
            .as_ref()
            .and_then(|id| scene.index_of(id));
        let Some(i) = selected else {
            // Overview: agent activity, communities, how to read the graph.
            let act = &lp.activity;
            T::tinted_card(T::CARD, T::BORDER).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(T::strong("Agent activity", 12.0).color(T::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("last {} days", crate::activity::HEAT_DAYS))
                                .size(11.0)
                                .color(T::TEXT_3),
                        );
                    });
                });
                ui.label(
                    RichText::new(format!(
                        "{} MCP calls, {} named a file or symbol",
                        fmt_num(act.calls),
                        fmt_num(act.attributed)
                    ))
                    .size(12.0)
                    .color(T::TEXT_3),
                );
                ui.add_space(T::S1);
                let mut hot: Vec<(&String, &usize)> = act.heat.iter().collect();
                hot.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
                let max = act.max_heat().max(1) as f32;
                if hot.is_empty() {
                    ui.label(
                        RichText::new("No agent calls named a file yet.")
                            .size(12.0)
                            .color(T::TEXT_3),
                    );
                }
                for (path, n) in hot.into_iter().take(6) {
                    let resp = ui
                        .horizontal(|ui| {
                            ui.label(T::mono(truncate_label_tail(path, 30), 11.0).color(T::TEXT_2));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| ui.label(T::mono(n.to_string(), 11.0).color(T::HEAT)),
                            );
                        })
                        .response
                        .interact(Sense::click())
                        .on_hover_text("Open this file's symbols")
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    let (bar, _) = ui
                        .allocate_exact_size(Vec2::new(ui.available_width(), 3.0), Sense::hover());
                    ui.painter()
                        .rect_filled(bar, CornerRadius::same(2), T::BORDER);
                    let mut fill = bar;
                    fill.set_width(bar.width() * (*n as f32 / max));
                    ui.painter()
                        .rect_filled(fill, CornerRadius::same(2), T::HEAT);
                    if resp.clicked() {
                        action = Some((NestedLevel::File(path.clone()), None, None, false));
                    }
                }
            });
            ui.add_space(T::S3);
            if self.graph_layout_mode == GraphLayoutMode::Nested
                && self.nested_level == NestedLevel::Project
            {
                T::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(T::strong("Communities", 12.0).color(T::TEXT));
                    for n in &scene.nodes {
                        let resp = ui
                            .horizontal(|ui| {
                                T::dot_label(
                                    ui,
                                    n.color,
                                    RichText::new(truncate_label(&n.label, 24)).size(12.0),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(
                                            T::mono(format!("{} files", n.child_count), 11.0)
                                                .color(T::TEXT_3),
                                        )
                                    },
                                );
                            })
                            .response
                            .interact(Sense::click())
                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                        if resp.clicked() {
                            action = n.drill.clone().map(|l| (l, None, None, false));
                        }
                    }
                });
                ui.add_space(T::S3);
            }
            T::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(T::strong("Reading the graph", 12.0).color(T::TEXT));
                for (c, text) in [
                    (T::NOTE, "Size = PageRank (summed for groups)"),
                    (T::HEAT, "Glow = agent calls that named it"),
                    (T::HEAT, "Dashed ring = uncommitted changes"),
                    (T::HEAT, "Numbered path = latest agent session"),
                    (T::ACCENT, "Badges: N note · D decision · ! finding"),
                ] {
                    T::dot_label(ui, c, RichText::new(text).size(12.0).color(T::TEXT_2));
                }
                ui.label(
                    RichText::new(
                        "Hover dims what is not connected. Click selects, double-click opens. Drag to pan, scroll to zoom.",
                    )
                    .size(11.0)
                    .color(T::TEXT_3),
                );
            });
            return action;
        };

        let n = &scene.nodes[i];
        let kind = match n.kind {
            graph_view::NodeKind::Community => "Community".to_string(),
            graph_view::NodeKind::File => format!("File · {}", n.detail),
            graph_view::NodeKind::Symbol => format!("Symbol · {}", n.detail),
        };
        ui.horizontal(|ui| {
            T::dot_label(ui, n.color, RichText::new(kind).size(11.0).color(T::TEXT_3));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new("×")
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::NONE),
                    )
                    .on_hover_text("Close")
                    .clicked()
                {
                    self.graph_view.selected = None;
                }
            });
        });
        ui.add(egui::Label::new(T::strong(&n.label, 16.0).color(T::TEXT)).wrap());
        if let Some(loc) = &n.location {
            ui.add(egui::Label::new(T::mono(loc, 11.0).color(T::TEXT_3)).wrap());
        }
        if !n.signature.is_empty() {
            ui.add_space(T::S1);
            egui::Frame::NONE
                .fill(T::BG)
                .stroke(Stroke::new(1.0_f32, T::BORDER))
                .corner_radius(CornerRadius::same(T::R_CONTROL))
                .inner_margin(Margin::same(10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(egui::Label::new(T::mono(&n.signature, 11.0).color(T::TEXT_2)).wrap());
                });
        }
        ui.add_space(T::S3);

        let (edges_in, edges_out) = scene.edges.iter().fold((0, 0), |(a, b), e| {
            (
                a + if e.tgt == i { e.weight } else { 0 },
                b + if e.src == i { e.weight } else { 0 },
            )
        });
        let leaving: usize = scene
            .portals
            .iter()
            .flat_map(|p| p.from.iter())
            .filter(|(k, _)| *k == i)
            .map(|(_, c)| c)
            .sum();
        let mut stats: Vec<(&str, String)> = vec![
            ("PageRank", format!("{:.4}", n.rank)),
            ("Tokens", fmt_num(n.tokens)),
        ];
        match n.kind {
            graph_view::NodeKind::Community => stats.push(("Files", fmt_num(n.child_count))),
            graph_view::NodeKind::File => stats.push(("Symbols", fmt_num(n.child_count))),
            graph_view::NodeKind::Symbol => {}
        }
        stats.push(("Edges in (here)", fmt_num(edges_in)));
        stats.push(("Edges out (here)", fmt_num(edges_out)));
        if !scene.portals.is_empty() {
            stats.push(("Edges leaving", fmt_num(leaving)));
        }
        egui::Frame::NONE
            .fill(T::BORDER)
            .corner_radius(CornerRadius::same(T::R_CARD))
            .inner_margin(Margin::same(1))
            .show(ui, |ui| {
                egui::Grid::new("inspector_stats")
                    .num_columns(2)
                    .spacing(Vec2::splat(1.0))
                    .min_col_width((ui.available_width() - 1.0) * 0.5)
                    .show(ui, |ui| {
                        for (k, (label, value)) in stats.iter().enumerate() {
                            egui::Frame::NONE
                                .fill(T::CARD)
                                .inner_margin(Margin::symmetric(10, 8))
                                .show(ui, |ui| {
                                    ui.set_width((ui.available_width()).max(100.0));
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        ui.label(RichText::new(*label).size(11.0).color(T::TEXT_3));
                                        ui.label(T::mono(value, 14.0).color(T::TEXT));
                                    });
                                });
                            if k % 2 == 1 {
                                ui.end_row();
                            }
                        }
                    });
            });
        ui.add_space(T::S3);

        // Agent activity for this node.
        let steps: Vec<usize> = lp
            .activity
            .session
            .iter()
            .enumerate()
            .filter(|(_, s)| scene.locate_step(s) == Some(graph_view::Target::Node(i)))
            .map(|(k, _)| k + 1)
            .collect();
        T::tinted_card(
            Color32::from_rgb(0x14, 0x11, 0x0E),
            Color32::from_rgb(0x3A, 0x2A, 0x1E),
        )
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(T::strong("Agent activity", 12.0).color(Color32::from_rgb(0xFF, 0xB2, 0x7F)));
            let row = |ui: &mut egui::Ui, k: &str, v: String| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(k).size(12.0).color(T::TEXT_2));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(T::mono(v, 11.0).color(T::TEXT));
                    });
                });
            };
            row(
                ui,
                &format!("Calls (last {} days)", crate::activity::HEAT_DAYS),
                fmt_num(n.heat),
            );
            row(
                ui,
                "Latest session steps",
                if steps.is_empty() {
                    "none".to_string()
                } else {
                    steps
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                },
            );
            if n.kind != graph_view::NodeKind::Symbol {
                row(
                    ui,
                    "Uncommitted changes",
                    if n.changed { "yes" } else { "no" }.to_string(),
                );
            }
        });
        ui.add_space(T::S3);

        ui.label(T::strong(format!("Memory & findings  {}", n.mem.len()), 12.0).color(T::TEXT));
        if n.mem.is_empty() {
            ui.label(
                RichText::new("No notes, decisions or health findings.")
                    .size(12.0)
                    .color(T::TEXT_3),
            );
        }
        for m in &n.mem {
            let (c, k) = match m.kind {
                graph_view::MemKind::Note => (T::NOTE, "note"),
                graph_view::MemKind::Decision => (T::ACCENT, "decision"),
                graph_view::MemKind::Finding => (T::DANGER, "finding"),
            };
            T::card().inner_margin(Margin::same(10)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    T::dot_label(
                        ui,
                        c,
                        RichText::new(if m.when.is_empty() {
                            k.to_string()
                        } else {
                            format!("{k} · {}", m.when)
                        })
                        .size(11.0)
                        .color(T::TEXT_3),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            T::mono(truncate_label_tail(&m.target, 22), 10.0).color(T::TEXT_3),
                        );
                    });
                });
                ui.add(egui::Label::new(RichText::new(&m.text).size(12.0).color(T::TEXT)).wrap());
            });
        }
        ui.add_space(T::S3);
        if let Some(level) = n.drill.clone()
            && ui.add(T::primary_button("Open level")).clicked()
        {
            action = Some((level, None, None, false));
        }
        action
    }

    fn render_architecture_tab(&mut self, ui: &mut egui::Ui) {
        let Some(ref lp) = self.loaded else {
            return;
        };

        egui::ScrollArea::vertical().show(ui, |ui| {
            // Code Health Summary Row
            ui.columns(3, |cols| {
                let bloat_col = if lp.health.bloated_files.is_empty() {
                    T::SUCCESS
                } else {
                    T::WARN
                };
                glow_kpi_card(
                    &mut cols[0],
                    "TOKEN BLOAT FILES (>=3.5K TOK)",
                    &lp.health.bloated_files.len().to_string(),
                    "Large files strain LLM context",
                    bloat_col,
                    bloat_col,
                );
                let cyc_col = if lp.health.circular_groups.is_empty() {
                    T::SUCCESS
                } else {
                    T::DANGER
                };
                glow_kpi_card(
                    &mut cols[1],
                    "CIRCULAR DEPENDENCY CYCLES",
                    &lp.health.circular_groups.len().to_string(),
                    "Strongly connected file loops",
                    cyc_col,
                    cyc_col,
                );
                glow_kpi_card(
                    &mut cols[2],
                    "ORPHAN CODE FILES",
                    &lp.health.orphan_files.len().to_string(),
                    "Zero incoming graph references",
                    T::ACCENT,
                    T::ACCENT,
                );
            });

            ui.add_space(12.0);

            if !lp.health.bloated_files.is_empty() || !lp.health.circular_groups.is_empty() {
                soft_card_frame(16).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new("CODE HEALTH DIAGNOSTICS")
                            .family(T::semibold_family())
                            .small()
                            .color(T::WARN),
                    );
                    ui.add_space(6.0);
                    for (p, toks, lines) in lp.health.bloated_files.iter().take(6) {
                        ui.label(
                            RichText::new(format!(
                                "Bloat: {p} — {} tokens ({lines} lines) — consider splitting",
                                fmt_num(*toks)
                            ))
                            .small()
                            .color(T::WARN),
                        );
                    }
                    for cyc in lp.health.circular_groups.iter().take(4) {
                        ui.label(
                            RichText::new(format!("Cycle: {}", cyc.join(" <-> ")))
                                .small()
                                .color(T::DANGER),
                        );
                    }
                });
                ui.add_space(12.0);
            }

            // Language breakdown pills
            soft_card_frame(16).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(format!(
                        "LANGUAGE BREAKDOWN  ({} files · {} symbols · {} edges)",
                        fmt_num(lp.total_files),
                        fmt_num(lp.total_symbols),
                        fmt_num(lp.total_edges)
                    ))
                    .family(T::semibold_family())
                    .small()
                    .color(T::TEXT_3),
                );
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    for (_, display, hex, files, toks) in &lp.lang_breakdown {
                        let col = parse_hex_color(hex);
                        egui::Frame::NONE
                            .fill(T::PANEL)
                            .stroke(Stroke::new(1.0_f32, T::CONTROL))
                            .corner_radius(CornerRadius::same(T::R_CONTROL))
                            .inner_margin(Margin::symmetric(10, 6))
                            .show(ui, |ui| {
                                T::dot_label(
                                    ui,
                                    col,
                                    RichText::new(display)
                                        .family(T::semibold_family())
                                        .color(T::TEXT),
                                );
                                ui.label(
                                    RichText::new(format!(
                                        "{files} files · {} tok",
                                        fmt_num(*toks)
                                    ))
                                    .small(),
                                );
                            });
                    }
                });
            });

            ui.add_space(12.0);

            // God nodes
            soft_card_frame(16).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("ARCHITECTURAL HUBS (GOD NODES)")
                        .family(T::semibold_family())
                        .small()
                        .color(T::TEXT_3),
                );
                ui.add_space(8.0);
                egui::Grid::new("god_nodes_grid")
                    .striped(true)
                    .spacing([14.0, 7.0])
                    .show(ui, |ui| {
                        for h in ["Symbol", "Kind", "File", "In", "Out", "Score"] {
                            ui.label(
                                RichText::new(h)
                                    .family(T::semibold_family())
                                    .small()
                                    .color(T::TEXT_3),
                            );
                        }
                        ui.end_row();
                        for g in &lp.god_nodes {
                            ui.label(
                                RichText::new(&g.name)
                                    .monospace()
                                    .family(T::semibold_family()),
                            );
                            ui.label(RichText::new(g.kind.as_str()).color(T::ACCENT));
                            ui.label(&g.file_path);
                            ui.label(g.in_degree.to_string());
                            ui.label(g.out_degree.to_string());
                            ui.label(format!("{:.2}", g.score));
                            ui.end_row();
                        }
                    });
            });

            ui.add_space(12.0);

            // Communities
            soft_card_frame(16).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("MODULE CLUSTERS")
                        .family(T::semibold_family())
                        .small()
                        .color(T::TEXT_3),
                );
                ui.add_space(8.0);
                for c in &lp.communities {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(format!("C{} {}", c.id, c.label))
                                .family(T::semibold_family())
                                .color(comm_color(
                                    lp,
                                    c.files.iter().min().map(|p| format!("c:{p}")).as_deref(),
                                )),
                        );
                        ui.label(
                            RichText::new(format!(
                                "({} files · {} symbols · {} tok)",
                                c.files.len(),
                                c.symbol_count,
                                fmt_num(c.total_tokens)
                            ))
                            .small()
                            .color(T::TEXT_3),
                        );
                    });
                    if !c.top_hubs.is_empty() {
                        ui.label(
                            RichText::new(format!("Hubs: {}", c.top_hubs.join(", ")))
                                .monospace()
                                .small(),
                        );
                    }
                    ui.add_space(6.0);
                }
            });
        });
    }

    fn render_memory_tab(&mut self, ui: &mut egui::Ui) {
        let mut add_note_req: Option<(String, String, String)> = None;
        let mut del_note_req: Option<(String, i64)> = None;

        let Some(ref lp) = self.loaded else {
            return;
        };
        let repo_path = lp.path.clone();

        soft_card_frame(16).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new("PERSISTENT AI MEMORY (AUTO-INJECTED ON READS)")
                    .family(T::semibold_family())
                    .small()
                    .color(T::ACCENT),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Notes saved here (or by AI agents via `remember`) are automatically attached whenever any AI reads that symbol or opens `repo_map`.",
                )
                .small()
                .color(T::TEXT_3),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label("Target:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.note_target_input)
                        .desired_width(160.0)
                        .hint_text("project, file, or symbol"),
                );
                ui.label("Note:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.note_content_input)
                        .desired_width((ui.available_width() - 110.0).max(180.0))
                        .hint_text("e.g. Coordinate system is Y-up; shader bindings 0-2 are reserved"),
                );
                let save_btn = egui::Button::new(RichText::new("Save").family(T::semibold_family()).color(Color32::WHITE))
                    .corner_radius(CornerRadius::same(T::R_CARD))
                    .fill(T::ACCENT);
                if ui.add(save_btn).clicked() && !self.note_content_input.trim().is_empty() {
                    add_note_req = Some((
                        repo_path.clone(),
                        self.note_target_input.clone(),
                        self.note_content_input.clone(),
                    ));
                }
            });
        });

        ui.add_space(12.0);

        egui::ScrollArea::vertical().show(ui, |ui| {
            if lp.notes.is_empty() {
                soft_card_frame(16).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new("No memory notes saved yet for this project.")
                            .color(T::TEXT_3),
                    );
                });
            } else {
                for note in &lp.notes {
                    soft_card_frame(14).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            egui::Frame::NONE
                                .fill(T::ACCENT_SOFT)
                                .corner_radius(CornerRadius::same(T::R_CONTROL))
                                .inner_margin(Margin::symmetric(8, 3))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(&note.target)
                                            .monospace()
                                            .small()
                                            .family(T::semibold_family())
                                            .color(T::ACCENT),
                                    );
                                });
                            ui.add_space(6.0);
                            ui.label(RichText::new(&note.content).color(Color32::WHITE));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new("Delete")
                                                .corner_radius(CornerRadius::same(T::R_CARD)),
                                        )
                                        .clicked()
                                    {
                                        del_note_req = Some((repo_path.clone(), note.id));
                                    }
                                },
                            );
                        });
                    });
                    ui.add_space(8.0);
                }
            }
        });

        if let Some((rp, target, content)) = add_note_req {
            let saved = Database::open_for_repo(Path::new(&rp))
                .and_then(|db| db.add_note(&target, &content).map(|_| db));
            match saved {
                Ok(db) => {
                    // Only clear the input once the note is really stored
                    self.note_content_input.clear();
                    self.status_banner = format!("Saved memory note for `{target}`");
                    if let Some(ref mut lp_mut) = self.loaded {
                        lp_mut.notes = db.list_notes(None).unwrap_or_default();
                    }
                }
                Err(e) => self.status_banner = format!("Could not save note: {e:#}"),
            }
        }
        if let Some((rp, id)) = del_note_req
            && let Ok(db) = Database::open_for_repo(Path::new(&rp))
        {
            let _ = db.delete_note(id);
            if let Some(ref mut lp_mut) = self.loaded {
                lp_mut.notes = db.list_notes(None).unwrap_or_default();
            }
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
                    .filter(|s| s.kind != SymbolKind::File && s.kind != SymbolKind::Impl)
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

    let total_symbols = cg
        .symbols
        .values()
        .filter(|s| s.kind != SymbolKind::File && s.kind != SymbolKind::Impl)
        .count();
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

fn compute_visible_mask(
    nodes: &[GraphNodeVis],
    search: &str,
    lang_filter: &str,
    code_only: bool,
) -> Vec<bool> {
    let q = search.trim().to_ascii_lowercase();
    nodes
        .iter()
        .map(|n| {
            if code_only && matches!(n.language.as_str(), "toml" | "markdown") {
                return false;
            }
            if lang_filter != "all" && n.language != lang_filter {
                return false;
            }
            if q.is_empty() {
                return true;
            }
            n.id.to_ascii_lowercase().contains(&q)
                || n.symbols
                    .iter()
                    .any(|(name, _, _, _)| name.to_ascii_lowercase().contains(&q))
        })
        .collect()
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

fn note_when(created_at: &str) -> String {
    created_at.chars().take(10).collect()
}

fn note_item(n: &ProjectNote) -> MemItem {
    let kind = if n.kind == "note" {
        MemKind::Note
    } else {
        MemKind::Decision
    };
    let text = if n.reason.trim().is_empty() {
        n.content.clone()
    } else {
        format!("{} ({}: {})", n.content, n.kind, n.reason)
    };
    MemItem {
        kind,
        text,
        target: n.target.clone(),
        when: note_when(&n.created_at),
    }
}

/// Notes, decisions and health findings attached to a file.
fn file_mem(lp: &LoadedProject, path: &str) -> Vec<MemItem> {
    let mut out: Vec<MemItem> = lp
        .notes
        .iter()
        .filter(|n| note_target_matches(&n.target, path))
        .map(note_item)
        .collect();
    if let Some(fs) = lp.findings.get(path) {
        out.extend(fs.iter().map(|f| MemItem {
            kind: MemKind::Finding,
            text: f.clone(),
            target: path.to_string(),
            when: String::new(),
        }));
    }
    out
}

fn comm_color(lp: &LoadedProject, cid: Option<&str>) -> Color32 {
    cid.and_then(|c| lp.community_color.get(c))
        .map(|&i| T::community_color(i))
        .unwrap_or(T::NOTE)
}

/// Scene for one nested level. Counts come from the view; nothing is added or dropped.
fn nested_scene(lp: &LoadedProject, view: &NestedView) -> Scene {
    let ng = NestedGraph::new(&lp.graph);
    let act = &lp.activity;
    let mut nodes = Vec::with_capacity(view.nodes.len());
    for nn in &view.nodes {
        let node = match nn.kind {
            NestedKind::Community => {
                let files = lp
                    .community_files
                    .get(&nn.id)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                SceneNode {
                    id: nn.id.clone(),
                    label: nn.label.clone(),
                    kind: NodeKind::Community,
                    color: comm_color(lp, Some(&nn.id)),
                    group: 0,
                    rank: files
                        .iter()
                        .map(|f| lp.file_rank.get(f).copied().unwrap_or(0.0))
                        .sum(),
                    tokens: nn.tokens,
                    child_count: nn.child_count,
                    location: None,
                    detail: String::new(),
                    signature: String::new(),
                    drill: nn.drill.clone(),
                    heat: files
                        .iter()
                        .map(|f| act.heat.get(f).copied().unwrap_or(0))
                        .sum(),
                    changed: files.iter().any(|f| act.changed.contains(f)),
                    mem: files.iter().flat_map(|f| file_mem(lp, f)).collect(),
                }
            }
            NestedKind::File => {
                let path = nn.id.strip_prefix("f:").unwrap_or(&nn.id);
                SceneNode {
                    id: nn.id.clone(),
                    label: nn.label.clone(),
                    kind: NodeKind::File,
                    color: comm_color(lp, ng.community_id_of(path)),
                    group: 0,
                    rank: lp.file_rank.get(path).copied().unwrap_or(0.0),
                    tokens: nn.tokens,
                    child_count: nn.child_count,
                    location: Some(path.to_string()),
                    detail: default_registry().display_name_for(&nn.detail),
                    signature: String::new(),
                    drill: nn.drill.clone(),
                    heat: act.heat.get(path).copied().unwrap_or(0),
                    changed: act.changed.contains(path),
                    mem: file_mem(lp, path),
                }
            }
            NestedKind::Symbol => {
                let sid = nn.id.strip_prefix("s:").unwrap_or(&nn.id);
                let sym = lp.graph.symbols.get(sid);
                let file = sym.map(|s| s.file_path.as_str());
                SceneNode {
                    id: nn.id.clone(),
                    label: nn.label.clone(),
                    kind: NodeKind::Symbol,
                    color: comm_color(lp, file.and_then(|f| ng.community_id_of(f))),
                    group: 0,
                    rank: lp.graph.pagerank.get(sid).copied().unwrap_or(0.0),
                    tokens: nn.tokens,
                    child_count: 0,
                    location: nn.location.clone(),
                    detail: nn.detail.clone(),
                    signature: sym.map(|s| s.signature.clone()).unwrap_or_default(),
                    drill: None,
                    heat: act.symbol_heat.get(sid).copied().unwrap_or(0),
                    changed: false,
                    mem: lp
                        .notes
                        .iter()
                        .filter(|n| note_target_matches(&n.target, &nn.label))
                        .map(note_item)
                        .collect(),
                }
            }
        };
        nodes.push(node);
    }
    let edges: Vec<SceneEdge> = view
        .edges
        .iter()
        .map(|e| SceneEdge {
            src: e.src,
            tgt: e.tgt,
            weight: e.weight,
        })
        .collect();
    let portals: Vec<Portal> = view
        .external
        .iter()
        .map(|g| {
            let color = match &g.level {
                Some(NestedLevel::Community(c)) => comm_color(lp, Some(c)),
                Some(NestedLevel::File(f)) => comm_color(lp, ng.community_id_of(f)),
                _ => T::NOTE,
            };
            Portal {
                label: g.label.clone(),
                color,
                count: g.count,
                level: g.level.clone(),
                from: g.from.clone(),
            }
        })
        .collect();

    // Where trail steps land on this level.
    let node_ix: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();
    let portal_of = |level: &NestedLevel| {
        portals
            .iter()
            .position(|p| p.level.as_ref() == Some(level))
            .map(Target::Portal)
    };
    let mut locate: HashMap<String, Target> = HashMap::new();
    for step in &act.session {
        for f in &step.files {
            let cid = ng.community_id_of(f);
            let t = match &view.level {
                NestedLevel::Project => cid.and_then(|c| node_ix.get(c)).map(|&i| Target::Node(i)),
                NestedLevel::Community(c) if cid == Some(c.as_str()) => node_ix
                    .get(format!("f:{f}").as_str())
                    .map(|&i| Target::Node(i)),
                NestedLevel::Community(_) => {
                    cid.and_then(|c| portal_of(&NestedLevel::Community(c.to_string())))
                }
                NestedLevel::File(p) if p == f => None,
                NestedLevel::File(_) => portal_of(&NestedLevel::File(f.clone())),
            };
            if let Some(t) = t {
                locate.insert(f.clone(), t);
            }
        }
        if let (NestedLevel::File(_), Some(sid)) = (&view.level, &step.symbol)
            && let Some(&i) = node_ix.get(format!("s:{sid}").as_str())
        {
            locate.insert(sid.clone(), Target::Node(i));
        }
    }

    let project_name = Path::new(&lp.path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Project")
        .to_string();
    let (title, hint) = match &view.level {
        NestedLevel::Project => (
            project_name,
            format!(
                "{} communities · double-click one to open it",
                fmt_num(view.nodes.len())
            ),
        ),
        NestedLevel::Community(_) => (
            view.breadcrumbs
                .last()
                .map(|b| b.0.clone())
                .unwrap_or_default(),
            format!(
                "{} files · pills lead to the communities these edges reach",
                fmt_num(view.nodes.len())
            ),
        ),
        NestedLevel::File(p) => (
            p.clone(),
            format!(
                "{} symbols · pills are the files these edges reach",
                fmt_num(view.nodes.len())
            ),
        ),
    };
    Scene::new(
        title,
        hint,
        NestedGraph::coverage_line(view),
        nodes,
        edges,
        portals,
        Vec::new(),
        false,
        locate,
    )
}

/// Scene with every visible file, clustered by community or by folder.
fn all_files_scene(lp: &LoadedProject, by_folder: bool, visible: &[bool]) -> Scene {
    let ng = NestedGraph::new(&lp.graph);
    let act = &lp.activity;
    let mut new_ix: HashMap<usize, usize> = HashMap::new();
    let mut group_ix: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<(String, Color32)> = Vec::new();
    let mut nodes = Vec::new();
    let mut locate = HashMap::new();
    for (i, gn) in lp.nodes.iter().enumerate() {
        if !visible.get(i).copied().unwrap_or(false) {
            continue;
        }
        let cid = ng.community_id_of(&gn.id);
        let color = comm_color(lp, cid);
        let (gkey, glabel, gcolor) = if by_folder {
            (gn.group_folder.clone(), gn.group_folder.clone(), T::TEXT_3)
        } else {
            let c = cid.unwrap_or("").to_string();
            let label = cid
                .and_then(|c| lp.community_label.get(c))
                .cloned()
                .unwrap_or_default();
            (c, label, color)
        };
        let next = groups.len();
        let g = *group_ix.entry(gkey).or_insert_with(|| {
            groups.push((glabel, gcolor));
            next
        });
        new_ix.insert(i, nodes.len());
        locate.insert(gn.id.clone(), Target::Node(nodes.len()));
        nodes.push(SceneNode {
            id: format!("f:{}", gn.id),
            label: gn.label.clone(),
            kind: NodeKind::File,
            color,
            group: g,
            rank: gn.rank,
            tokens: gn.tokens,
            child_count: gn.symbols.len(),
            location: Some(gn.id.clone()),
            detail: default_registry().display_name_for(&gn.language),
            signature: String::new(),
            drill: Some(NestedLevel::File(gn.id.clone())),
            heat: act.heat.get(&gn.id).copied().unwrap_or(0),
            changed: act.changed.contains(&gn.id),
            mem: file_mem(lp, &gn.id),
        });
    }
    let edges: Vec<SceneEdge> = lp
        .edges
        .iter()
        .filter_map(|e| {
            Some(SceneEdge {
                src: *new_ix.get(&e.src)?,
                tgt: *new_ix.get(&e.tgt)?,
                weight: 1,
            })
        })
        .collect();
    let hidden = lp.nodes.len() - nodes.len();
    let coverage = format!(
        "{} of {} files{} · {} file-to-file edges between shown files",
        fmt_num(nodes.len()),
        fmt_num(lp.nodes.len()),
        if hidden > 0 {
            format!(" ({} hidden by filters)", fmt_num(hidden))
        } else {
            String::new()
        },
        fmt_num(edges.len())
    );
    let title = if by_folder {
        "All files by folder"
    } else {
        "All files by community"
    };
    Scene::new(
        title.to_string(),
        "Scroll to zoom · drag to pan · labels appear as you zoom in".to_string(),
        coverage,
        nodes,
        edges,
        Vec::new(),
        groups,
        true,
        locate,
    )
}

fn parse_hex_color(hex: &str) -> Color32 {
    let h = hex.trim_start_matches('#');
    if h.len() == 6
        && let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&h[0..2], 16),
            u8::from_str_radix(&h[2..4], 16),
            u8::from_str_radix(&h[4..6], 16),
        )
    {
        return Color32::from_rgb(r, g, b);
    }
    T::TEXT_3
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
