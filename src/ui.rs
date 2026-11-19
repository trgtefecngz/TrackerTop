#![cfg(feature = "gui")]

use crate::context::TrackerEngine;
use crate::db::Database;
use crate::graph::{CodeGraph, CommunitySummary, ScoredNode};
use crate::indexer::index_repository;
use crate::install::{install_claude_code, is_claude_global_installed};
use crate::model::{EdgeKind, SymbolKind, TokenStatsSummary};
use crate::parser::default_registry;
use crate::registry::{GlobalOverview, load_global_overview, register_project, remove_project};
use anyhow::Result;
use eframe::egui::{
    self, Color32, CornerRadius, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2,
};
use std::collections::{BTreeMap, HashMap, HashSet};
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphLayoutMode {
    Folders,
    Clusters,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphColorMode {
    Language,
    Cluster,
}

struct GraphNodeVis {
    id: String,
    label: String,
    group_folder: String,
    language: String,
    community: usize,
    tokens: usize,
    lines: usize,
    rank: f64,
    degree: usize,
    x: f32,
    y: f32,
    radius: f32,
    symbols: Vec<(String, String, usize, String)>, // (name, kind, line, signature)
}

struct GraphIsland {
    title: String,
    center_x: f32,
    center_y: f32,
    radius: f32,
    count: usize,
    color_idx: usize,
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
    lang_breakdown: Vec<(String, String, String, usize, usize)>, // (id, display, hex, files, tokens)
    god_nodes: Vec<ScoredNode>,
    communities: Vec<CommunitySummary>,
    stats: TokenStatsSummary,
    nodes: Vec<GraphNodeVis>,
    edges: Vec<GraphEdgeVis>,
    islands: Vec<GraphIsland>,
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
    claude_connected: bool,
    status_banner: String,
    last_refresh: Instant,
    bg_rx: Option<Receiver<BgMessage>>,
    bg_busy: bool,

    // Graph view state
    cam_offset: Vec2,
    cam_zoom: f32,
    needs_fit_view: bool,
    graph_search: String,
    graph_lang_filter: String,
    graph_code_only: bool,
    graph_layout_mode: GraphLayoutMode,
    graph_color_mode: GraphColorMode,
    selected_node_idx: Option<usize>,
    dragged_node_idx: Option<usize>,
}

impl TrackerTopApp {
    pub fn new(initial_repo: Option<PathBuf>) -> Self {
        if let Some(ref p) = initial_repo {
            if p.join(".trackertop").join("index.db").exists() {
                let _ = register_project(p);
            }
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
            claude_connected: is_claude_global_installed(),
            status_banner: "Ready — 100% offline local engine".to_string(),
            last_refresh: Instant::now(),
            bg_rx: None,
            bg_busy: false,
            cam_offset: Vec2::ZERO,
            cam_zoom: 1.0,
            needs_fit_view: true,
            graph_search: String::new(),
            graph_lang_filter: "all".to_string(),
            graph_code_only: false,
            graph_layout_mode: GraphLayoutMode::Folders,
            graph_color_mode: GraphColorMode::Language,
            selected_node_idx: None,
            dragged_node_idx: None,
        };
        app.reload_selected_project();
        app
    }

    fn reload_overview(&mut self) {
        self.overview = load_global_overview();
        self.claude_connected = is_claude_global_installed();
    }

    fn reload_selected_project(&mut self) {
        let Selection::Project(ref path) = self.selection else {
            self.loaded = None;
            return;
        };
        self.selected_node_idx = None;
        self.dragged_node_idx = None;
        self.loaded = load_project_details(Path::new(path)).ok();
        self.recompute_graph_layout();
    }

    fn recompute_graph_layout(&mut self) {
        let Some(ref mut lp) = self.loaded else {
            return;
        };
        let visible = compute_visible_mask(
            &lp.nodes,
            &self.graph_search,
            &self.graph_lang_filter,
            self.graph_code_only,
        );
        lp.islands = arrange_structured_islands(&mut lp.nodes, &visible, self.graph_layout_mode);
        self.needs_fit_view = true;
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
                        stats.total_files,
                        stats.total_symbols,
                        stats.total_edges,
                        stats.elapsed_ms
                    ),
                    select_path: if select_after {
                        let s = std::fs::canonicalize(&repo_path)
                            .unwrap_or(repo_path)
                            .to_string_lossy()
                            .to_string();
                        Some(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
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

    fn run_sample_query(&mut self, repo_path: &str) {
        let engine = TrackerEngine::new(Path::new(repo_path));
        match engine.repo_map(900) {
            Ok(out) => {
                let pct = if out.baseline_tokens > 0 {
                    (out.tokens_saved as f64 / out.baseline_tokens as f64) * 100.0
                } else {
                    0.0
                };
                self.status_banner = format!(
                    "Test query complete: sent {} tok vs {} baseline -> saved {} tok ({:.1}%) in {} ms",
                    fmt_num(out.tokens_sent),
                    fmt_num(out.baseline_tokens),
                    fmt_num(out.tokens_saved),
                    pct,
                    out.latency_ms
                );
                self.reload_overview();
                if let Some(ref mut lp) = self.loaded {
                    if let Ok(db) = Database::open_path(
                        &Path::new(repo_path).join(".trackertop").join("index.db"),
                    ) {
                        if let Ok(stats) = db.get_token_stats(30) {
                            lp.stats = stats;
                        }
                    }
                }
            }
            Err(e) => {
                self.status_banner = format!("Query error: {e}");
            }
        }
    }

    fn poll_background(&mut self) {
        if let Some(ref rx) = self.bg_rx {
            if let Ok(msg) = rx.try_recv() {
                self.bg_busy = false;
                self.bg_rx = None;
                self.status_banner = msg.status;
                if let Some(p) = msg.select_path {
                    self.selection = Selection::Project(p);
                }
                self.reload_overview();
                self.reload_selected_project();
            }
        }
    }
}

impl eframe::App for TrackerTopApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_background();

        // Live auto-refresh every 2 seconds so Claude Code queries stream in automatically
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
                    }
                } else {
                    self.reload_selected_project();
                }
            }
        }
        ctx.request_repaint_after(Duration::from_millis(if self.bg_busy { 100 } else { 750 }));

        apply_dark_theme(ctx);

        // Top Bar
        egui::TopBottomPanel::top("top_bar")
            .exact_height(50.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        RichText::new("TrackerTop")
                            .strong()
                            .size(18.0)
                            .color(Color32::from_rgb(56, 189, 248)),
                    );
                    ui.separator();

                    match &self.selection {
                        Selection::AllProjects => {
                            ui.label(RichText::new("All Projects — Global Overview").strong());
                        }
                        Selection::Project(path) => {
                            let name = Path::new(path)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or(path);
                            ui.label(RichText::new(name).strong().size(15.0));
                            ui.label(
                                RichText::new(path)
                                    .small()
                                    .color(Color32::from_rgb(139, 155, 184)),
                            );
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let claude_label = if self.claude_connected {
                            "Claude Code: Connected"
                        } else {
                            "Connect Claude Code (Global)"
                        };
                        let btn = egui::Button::new(
                            RichText::new(claude_label).color(if self.claude_connected {
                                Color32::from_rgb(52, 211, 153)
                            } else {
                                Color32::WHITE
                            }),
                        )
                        .fill(if self.claude_connected {
                            Color32::from_rgba_unmultiplied(52, 211, 153, 28)
                        } else {
                            Color32::from_rgb(37, 99, 235)
                        });
                        if ui.add(btn).clicked() {
                            let base = match &self.selection {
                                Selection::Project(p) => PathBuf::from(p),
                                Selection::AllProjects => {
                                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                                }
                            };
                            match install_claude_code(&base, true) {
                                Ok(lines) => {
                                    self.claude_connected = is_claude_global_installed();
                                    self.status_banner = lines.join(" | ");
                                }
                                Err(e) => {
                                    self.status_banner = format!("Claude setup error: {e}");
                                }
                            }
                        }

                        if let Selection::Project(ref p) = self.selection.clone() {
                            if ui
                                .add_enabled(!self.bg_busy, egui::Button::new("Re-Index"))
                                .clicked()
                            {
                                self.start_bg_index(PathBuf::from(p), true, false);
                            }
                        }
                    });
                });
            });

        // Bottom Status Bar
        egui::TopBottomPanel::bottom("status_bar")
            .exact_height(26.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    if self.bg_busy {
                        ui.spinner();
                    }
                    ui.label(
                        RichText::new(&self.status_banner)
                            .small()
                            .color(Color32::from_rgb(148, 163, 184)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let langs: Vec<String> = default_registry()
                            .languages()
                            .into_iter()
                            .map(|l| l.display_name)
                            .collect();
                        ui.label(
                            RichText::new(format!("Active Languages: {}", langs.join(", ")))
                                .small()
                                .color(Color32::from_rgb(100, 116, 139)),
                        );
                    });
                });
            });

        // Left Sidebar: Multi-Project Switcher
        egui::SidePanel::left("projects_sidebar")
            .resizable(true)
            .default_width(245.0)
            .min_width(200.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("PROJECTS")
                            .strong()
                            .small()
                            .color(Color32::from_rgb(139, 155, 184)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(!self.bg_busy, egui::Button::new("+ Add Project"))
                            .clicked()
                        {
                            if let Some(folder) = rfd::FileDialog::new()
                                .set_title("Select Project Folder to Track")
                                .pick_folder()
                            {
                                self.start_bg_index(folder, false, true);
                            }
                        }
                    });
                });
                ui.add_space(6.0);

                let all_selected = self.selection == Selection::AllProjects;
                let all_btn = egui::Button::new(
                    RichText::new(format!("All Projects ({})", self.overview.projects.len()))
                        .strong(),
                )
                .min_size(Vec2::new(ui.available_width(), 32.0))
                .fill(if all_selected {
                    Color32::from_rgb(24, 48, 78)
                } else {
                    Color32::from_rgb(18, 24, 38)
                });
                if ui.add(all_btn).clicked() {
                    self.selection = Selection::AllProjects;
                    self.loaded = None;
                }

                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);

                let mut to_select: Option<String> = None;
                let mut to_remove: Option<String> = None;

                egui::ScrollArea::vertical().show(ui, |ui| {
                    if self.overview.projects.is_empty() {
                        ui.label(
                            RichText::new(
                                "No projects yet.\nClick '+ Add Project' or open any repo in Claude Code once connected.",
                            )
                            .small()
                            .color(Color32::from_rgb(139, 155, 184)),
                        );
                    }

                    for snap in &self.overview.projects {
                        let is_sel = matches!(&self.selection, Selection::Project(p) if p.eq_ignore_ascii_case(&snap.entry.path));
                        let frame = egui::Frame::NONE
                            .fill(if is_sel {
                                Color32::from_rgb(24, 48, 78)
                            } else {
                                Color32::from_rgb(20, 27, 42)
                            })
                            .stroke(Stroke::new(
                                1.0_f32,
                                if is_sel {
                                    Color32::from_rgb(56, 189, 248)
                                } else {
                                    Color32::from_rgb(36, 48, 74)
                                },
                            ))
                            .corner_radius(CornerRadius::same(6))
                            .inner_margin(8.0);

                        frame.show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let title = ui.add(
                                    egui::Label::new(RichText::new(&snap.entry.name).strong())
                                        .sense(Sense::click()),
                                );
                                if title.clicked() {
                                    to_select = Some(snap.entry.path.clone());
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .small_button("x")
                                            .on_hover_text("Remove from TrackerTop list")
                                            .clicked()
                                        {
                                            to_remove = Some(snap.entry.path.clone());
                                        }
                                    },
                                );
                            });

                            let subtitle = if snap.stats.total_queries > 0 {
                                format!(
                                    "{} files · saved {} tok ({:.0}%)",
                                    snap.total_files,
                                    fmt_num(snap.stats.total_tokens_saved),
                                    snap.stats.savings_percent
                                )
                            } else {
                                format!(
                                    "{} files · {} tok · Ready",
                                    snap.total_files,
                                    fmt_num(snap.total_repo_tokens)
                                )
                            };
                            let sub_resp = ui.add(
                                egui::Label::new(
                                    RichText::new(subtitle)
                                        .small()
                                        .color(if snap.stats.total_tokens_saved > 0 {
                                            Color32::from_rgb(52, 211, 153)
                                        } else {
                                            Color32::from_rgb(139, 155, 184)
                                        }),
                                )
                                .sense(Sense::click()),
                            );
                            if sub_resp.clicked() {
                                to_select = Some(snap.entry.path.clone());
                            }
                        });
                        ui.add_space(6.0);
                    }
                });

                if let Some(rem) = to_remove {
                    let _ = remove_project(&rem);
                    if matches!(&self.selection, Selection::Project(p) if p.eq_ignore_ascii_case(&rem))
                    {
                        self.selection = Selection::AllProjects;
                        self.loaded = None;
                    }
                    self.reload_overview();
                }
                if let Some(sel) = to_select {
                    self.selection = Selection::Project(sel);
                    self.reload_selected_project();
                }
            });

        // Main Central Panel
        egui::CentralPanel::default().show(ctx, |ui| match &self.selection {
            Selection::AllProjects => self.render_all_projects(ui),
            Selection::Project(_) => self.render_single_project(ui),
        });
    }
}

impl TrackerTopApp {
    fn render_all_projects(&mut self, ui: &mut egui::Ui) {
        let ov = &self.overview;
        ui.add_space(6.0);
        ui.columns(4, |cols| {
            kpi_card(
                &mut cols[0],
                "TRACKED PROJECTS",
                &ov.projects.len().to_string(),
                &format!(
                    "{} files · {} symbols",
                    fmt_num(ov.total_files),
                    fmt_num(ov.total_symbols)
                ),
                Color32::WHITE,
            );
            kpi_card(
                &mut cols[1],
                "TOTAL AI QUERIES",
                &fmt_num(ov.total_queries),
                &format!("Sent {} tok", fmt_num(ov.total_tokens_sent)),
                Color32::from_rgb(56, 189, 248),
            );
            kpi_card(
                &mut cols[2],
                "NET TOKENS SAVED",
                &fmt_num(ov.total_tokens_saved),
                &format!("{:.1}% reduction vs raw reads", ov.savings_percent),
                Color32::from_rgb(52, 211, 153),
            );
            kpi_card(
                &mut cols[3],
                "EST. USD SAVED",
                &format!("${:.4}", ov.est_usd_saved_sonnet),
                &format!("Sonnet · ${:.4} Opus", ov.est_usd_saved_opus),
                Color32::from_rgb(52, 211, 153),
            );
        });

        ui.add_space(14.0);
        ui.heading("Projects Breakdown");
        ui.add_space(6.0);

        let mut open_path: Option<String> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("global_projects_grid")
                .striped(true)
                .min_col_width(90.0)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    for h in [
                        "Project",
                        "Files",
                        "Repo Tokens",
                        "Queries",
                        "Tokens Saved",
                        "Savings %",
                        "Action",
                    ] {
                        ui.label(
                            RichText::new(h)
                                .strong()
                                .color(Color32::from_rgb(148, 163, 184)),
                        );
                    }
                    ui.end_row();

                    for p in &ov.projects {
                        ui.label(RichText::new(&p.entry.name).strong());
                        ui.label(fmt_num(p.total_files));
                        ui.label(fmt_num(p.total_repo_tokens));
                        ui.label(fmt_num(p.stats.total_queries));
                        ui.label(
                            RichText::new(fmt_num(p.stats.total_tokens_saved))
                                .color(Color32::from_rgb(52, 211, 153)),
                        );
                        ui.label(format!("{:.1}%", p.stats.savings_percent));
                        if ui.button("Open").clicked() {
                            open_path = Some(p.entry.path.clone());
                        }
                        ui.end_row();
                    }
                });
        });

        if let Some(p) = open_path {
            self.selection = Selection::Project(p);
            self.reload_selected_project();
        }
    }

    fn render_single_project(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for (tab, label) in [
                (ProjectTab::Savings, "Token Savings (Live)"),
                (ProjectTab::Graph, "Code Graph"),
                (ProjectTab::Architecture, "Architecture & Hubs"),
            ] {
                let active = self.active_tab == tab;
                if ui
                    .selectable_label(active, RichText::new(label).strong().size(14.0))
                    .clicked()
                {
                    if tab == ProjectTab::Graph && self.active_tab != ProjectTab::Graph {
                        self.needs_fit_view = true;
                    }
                    self.active_tab = tab;
                }
            }
        });
        ui.separator();

        if self.loaded.is_none() {
            ui.add_space(20.0);
            ui.label("Project is not indexed yet. Click 'Re-Index' in the top bar.");
            return;
        }

        match self.active_tab {
            ProjectTab::Savings => self.render_savings_tab(ui),
            ProjectTab::Graph => self.render_graph_tab(ui),
            ProjectTab::Architecture => self.render_architecture_tab(ui),
        }
    }

    fn render_savings_tab(&mut self, ui: &mut egui::Ui) {
        let mut run_test_for: Option<String> = None;
        let Some(ref lp) = self.loaded else {
            return;
        };
        let s = &lp.stats;

        ui.add_space(6.0);
        ui.columns(4, |cols| {
            kpi_card(
                &mut cols[0],
                "NET TOKENS SAVED",
                &fmt_num(s.total_tokens_saved),
                &format!("{:.1}% reduction", s.savings_percent),
                Color32::from_rgb(52, 211, 153),
            );
            kpi_card(
                &mut cols[1],
                "SENT VS BASELINE",
                &fmt_num(s.total_tokens_sent),
                &format!("vs {} raw baseline tok", fmt_num(s.total_baseline_tokens)),
                Color32::from_rgb(56, 189, 248),
            );
            kpi_card(
                &mut cols[2],
                "AI QUERIES LOGGED",
                &fmt_num(s.total_queries),
                &format!(
                    "{} files · {} tok indexed",
                    fmt_num(lp.total_files),
                    fmt_num(lp.total_repo_tokens)
                ),
                Color32::WHITE,
            );
            kpi_card(
                &mut cols[3],
                "EST. USD SAVED",
                &format!("${:.4}", s.est_usd_saved_sonnet),
                &format!("Sonnet · ${:.4} Opus", s.est_usd_saved_opus),
                Color32::from_rgb(52, 211, 153),
            );
        });

        ui.add_space(12.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            if s.total_queries == 0 {
                egui::Frame::NONE
                    .fill(Color32::from_rgba_unmultiplied(52, 211, 153, 18))
                    .stroke(Stroke::new(
                        1.0_f32,
                        Color32::from_rgba_unmultiplied(52, 211, 153, 95),
                    ))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(14.0)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new(format!(
                                "Indexed & Ready — {} files ({} symbols, {} tokens) mapped!",
                                fmt_num(lp.total_files),
                                fmt_num(lp.total_symbols),
                                fmt_num(lp.total_repo_tokens)
                            ))
                            .strong()
                            .size(14.0)
                            .color(Color32::from_rgb(52, 211, 153)),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(
                                "TrackerTop works on any existing or new project immediately. Savings appear here as soon as Claude Code runs a query inside this folder — or click below to run a live test query right now.",
                            )
                            .color(Color32::from_rgb(203, 213, 225)),
                        );
                        ui.add_space(8.0);
                        if ui
                            .button(RichText::new("Run Quick Test Query (Verify Savings)").strong())
                            .clicked()
                        {
                            run_test_for = Some(lp.path.clone());
                        }
                    });
                ui.add_space(10.0);
            }

            // Savings by tool
            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("SAVINGS BY MCP TOOL")
                            .strong()
                            .small()
                            .color(Color32::from_rgb(148, 163, 184)),
                    );
                    if s.total_queries > 0 {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Run Test Query").clicked() {
                                run_test_for = Some(lp.path.clone());
                            }
                        });
                    }
                });
                ui.add_space(6.0);
                if s.by_tool.is_empty() {
                    ui.label(
                        RichText::new("Waiting for the first AI query in this project...")
                            .color(Color32::from_rgb(139, 155, 184)),
                    );
                } else {
                    for t in &s.by_tool {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("{} ({}x)", t.tool, t.queries))
                                    .monospace()
                                    .strong(),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "-{} tok ({:.1}%)",
                                        fmt_num(t.tokens_saved),
                                        t.savings_percent
                                    ))
                                    .color(Color32::from_rgb(52, 211, 153)),
                                );
                            });
                        });
                        let frac = (t.savings_percent as f32 / 100.0).clamp(0.0, 1.0);
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_height(8.0)
                                .fill(Color32::from_rgb(52, 211, 153)),
                        );
                        ui.add_space(6.0);
                    }
                }
            });

            ui.add_space(10.0);

            // Recent queries table
            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("LIVE AI QUERY LOG")
                        .strong()
                        .small()
                        .color(Color32::from_rgb(148, 163, 184)),
                );
                ui.add_space(6.0);
                if s.recent.is_empty() {
                    ui.label(
                        RichText::new("Live queries from Claude Code will stream in here automatically every 2 seconds.")
                            .color(Color32::from_rgb(139, 155, 184)),
                    );
                } else {
                    egui::Grid::new("recent_queries_grid")
                        .striped(true)
                        .spacing([14.0, 6.0])
                        .show(ui, |ui| {
                            for h in ["Tool", "Query", "Sent", "Baseline", "Saved", "Latency"] {
                                ui.label(
                                    RichText::new(h)
                                        .strong()
                                        .small()
                                        .color(Color32::from_rgb(148, 163, 184)),
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
                                        .color(Color32::from_rgb(56, 189, 248)),
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
                                    .color(Color32::from_rgb(52, 211, 153)),
                                );
                                ui.label(format!("{} ms", r.latency_ms));
                                ui.end_row();
                            }
                        });
                }
            });
        });

        if let Some(path) = run_test_for {
            self.run_sample_query(&path);
        }
    }

    fn render_graph_tab(&mut self, ui: &mut egui::Ui) {
        let mut layout_dirty = false;

        ui.horizontal_wrapped(|ui| {
            ui.label("Search:");
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.graph_search)
                        .desired_width(155.0)
                        .hint_text("file or symbol..."),
                )
                .changed()
            {
                layout_dirty = true;
            }

            let prev_lang = self.graph_lang_filter.clone();
            egui::ComboBox::from_id_salt("lang_filter")
                .selected_text(if self.graph_lang_filter == "all" {
                    "All Languages".to_string()
                } else {
                    default_registry().display_name_for(&self.graph_lang_filter)
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.graph_lang_filter,
                        "all".to_string(),
                        "All Languages",
                    );
                    for l in default_registry().languages() {
                        ui.selectable_value(&mut self.graph_lang_filter, l.id, l.display_name);
                    }
                });
            if self.graph_lang_filter != prev_lang {
                layout_dirty = true;
            }

            if ui
                .checkbox(&mut self.graph_code_only, "Code Only (hide .toml/.md)")
                .changed()
            {
                layout_dirty = true;
            }

            let prev_layout = self.graph_layout_mode;
            egui::ComboBox::from_id_salt("layout_mode")
                .selected_text(match self.graph_layout_mode {
                    GraphLayoutMode::Folders => "Layout: Folders",
                    GraphLayoutMode::Clusters => "Layout: Clusters",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.graph_layout_mode,
                        GraphLayoutMode::Folders,
                        "Layout: Folders",
                    );
                    ui.selectable_value(
                        &mut self.graph_layout_mode,
                        GraphLayoutMode::Clusters,
                        "Layout: Clusters",
                    );
                });
            if self.graph_layout_mode != prev_layout {
                layout_dirty = true;
            }

            egui::ComboBox::from_id_salt("color_mode")
                .selected_text(match self.graph_color_mode {
                    GraphColorMode::Language => "Color: Language",
                    GraphColorMode::Cluster => "Color: Cluster",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.graph_color_mode,
                        GraphColorMode::Language,
                        "Color: Language",
                    );
                    ui.selectable_value(
                        &mut self.graph_color_mode,
                        GraphColorMode::Cluster,
                        "Color: Cluster",
                    );
                });

            if ui.button("Fit View").clicked() {
                self.needs_fit_view = true;
            }
        });

        if layout_dirty {
            self.recompute_graph_layout();
        }
        ui.add_space(4.0);

        let avail = ui.available_size();
        let inspector_w = if self.selected_node_idx.is_some() {
            290.0
        } else {
            0.0
        };
        let canvas_w = (avail.x - inspector_w - 8.0).max(240.0);

        ui.horizontal_top(|ui| {
            let (rect, resp) = ui.allocate_exact_size(
                Vec2::new(canvas_w, (avail.y - 6.0).max(200.0)),
                Sense::click_and_drag(),
            );
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, CornerRadius::same(8), Color32::from_rgb(10, 14, 24));
            painter.rect_stroke(
                rect,
                CornerRadius::same(8),
                Stroke::new(1.0_f32, Color32::from_rgb(36, 48, 74)),
                StrokeKind::Inside,
            );

            let center = rect.center();
            let reg = default_registry();

            if let Some(ref mut lp) = self.loaded {
                let visible = compute_visible_mask(
                    &lp.nodes,
                    &self.graph_search,
                    &self.graph_lang_filter,
                    self.graph_code_only,
                );

                // Auto-fit all visible islands neatly into the viewport
                if self.needs_fit_view {
                    let (min_x, max_x, min_y, max_y) = compute_bounds(&lp.nodes, &lp.islands, &visible);
                    let span_x = (max_x - min_x).max(160.0);
                    let span_y = (max_y - min_y).max(160.0);
                    let fit_x = (rect.width() - 60.0) / span_x;
                    let fit_y = (rect.height() - 60.0) / span_y;
                    self.cam_zoom = fit_x.min(fit_y).clamp(0.12, 2.2);
                    self.cam_offset = Vec2::new(-(min_x + max_x) * 0.5, -(min_y + max_y) * 0.5);
                    self.needs_fit_view = false;
                }

                let to_screen = |wx: f32, wy: f32, cam_off: Vec2, zoom: f32| -> Pos2 {
                    Pos2::new(
                        center.x + (wx + cam_off.x) * zoom,
                        center.y + (wy + cam_off.y) * zoom,
                    )
                };

                // Smooth scroll zoom centered on canvas
                if resp.hovered() {
                    let scroll = ui.input(|i| i.raw_scroll_delta.y);
                    if scroll.abs() > 0.1 {
                        let factor = if scroll > 0.0 { 1.12 } else { 0.89 };
                        self.cam_zoom = (self.cam_zoom * factor).clamp(0.10, 4.0);
                    }
                }

                // Hit test pointer
                let pointer_pos = resp.hover_pos().or_else(|| resp.interact_pointer_pos());
                let hovered_node = pointer_pos.and_then(|pos| {
                    lp.nodes.iter().enumerate().rev().find_map(|(idx, n)| {
                        if !visible[idx] {
                            return None;
                        }
                        let sp = to_screen(n.x, n.y, self.cam_offset, self.cam_zoom);
                        let r = (n.radius * self.cam_zoom + 5.0).max(7.0);
                        if sp.distance(pos) <= r {
                            Some(idx)
                        } else {
                            None
                        }
                    })
                });

                if resp.drag_started() {
                    self.dragged_node_idx = hovered_node;
                    if hovered_node.is_some() {
                        self.selected_node_idx = hovered_node;
                    }
                }
                if resp.dragged() {
                    let delta = resp.drag_delta() / self.cam_zoom;
                    if let Some(idx) = self.dragged_node_idx {
                        if let Some(n) = lp.nodes.get_mut(idx) {
                            n.x += delta.x;
                            n.y += delta.y;
                        }
                    } else {
                        self.cam_offset += delta;
                    }
                }
                if resp.drag_stopped() {
                    self.dragged_node_idx = None;
                }
                if resp.clicked() {
                    self.selected_node_idx = hovered_node;
                }

                // Focus neighborhood (Obsidian-style dim when hovering/selecting)
                let focus_idx = hovered_node.or(self.selected_node_idx);
                let mut focused_set: HashSet<usize> = HashSet::new();
                if let Some(fi) = focus_idx {
                    focused_set.insert(fi);
                    for e in &lp.edges {
                        if !visible[e.src] || !visible[e.tgt] {
                            continue;
                        }
                        if e.src == fi {
                            focused_set.insert(e.tgt);
                        } else if e.tgt == fi {
                            focused_set.insert(e.src);
                        }
                    }
                }

                // 1. Draw translucent island backgrounds & clean group headers
                for isl in &lp.islands {
                    let sp = to_screen(isl.center_x, isl.center_y, self.cam_offset, self.cam_zoom);
                    let sr = isl.radius * self.cam_zoom;
                    if !rect.expand(sr + 30.0).contains(sp) {
                        continue;
                    }
                    let base_col = cluster_color(isl.color_idx);
                    let fill = Color32::from_rgba_unmultiplied(
                        base_col.r(),
                        base_col.g(),
                        base_col.b(),
                        14,
                    );
                    let border = Color32::from_rgba_unmultiplied(
                        base_col.r(),
                        base_col.g(),
                        base_col.b(),
                        52,
                    );
                    painter.circle_filled(sp, sr, fill);
                    painter.circle_stroke(sp, sr, Stroke::new(1.0_f32, border));

                    // Draw island title above circle
                    if sr >= 14.0 {
                        let title_text = format!("{} ({})", isl.title, isl.count);
                        painter.text(
                            Pos2::new(sp.x, sp.y - sr - 9.0),
                            egui::Align2::CENTER_CENTER,
                            title_text,
                            egui::FontId::proportional(11.5),
                            Color32::from_rgba_unmultiplied(203, 213, 225, 220),
                        );
                    }
                }

                // 2. Draw edges (subtle when unfocused; bright cyan for focused node's dependencies)
                for e in &lp.edges {
                    if !visible[e.src] || !visible[e.tgt] {
                        continue;
                    }
                    let is_hl = focus_idx.is_some_and(|fi| e.src == fi || e.tgt == fi);
                    if focus_idx.is_some() && !is_hl {
                        continue; // Keep focus mode ultra-clean
                    }
                    let a = &lp.nodes[e.src];
                    let b = &lp.nodes[e.tgt];
                    let p1 = to_screen(a.x, a.y, self.cam_offset, self.cam_zoom);
                    let p2 = to_screen(b.x, b.y, self.cam_offset, self.cam_zoom);
                    let stroke = if is_hl {
                        Stroke::new(1.8_f32, Color32::from_rgba_unmultiplied(56, 189, 248, 220))
                    } else {
                        Stroke::new(0.9_f32, Color32::from_rgba_unmultiplied(148, 163, 184, 34))
                    };
                    painter.line_segment([p1, p2], stroke);
                }

                // 3. Draw nodes
                for (idx, n) in lp.nodes.iter().enumerate() {
                    if !visible[idx] {
                        continue;
                    }
                    let sp = to_screen(n.x, n.y, self.cam_offset, self.cam_zoom);
                    if !rect.expand(30.0).contains(sp) {
                        continue;
                    }
                    let base = match self.graph_color_mode {
                        GraphColorMode::Language => parse_hex_color(reg.color_for(&n.language)),
                        GraphColorMode::Cluster => cluster_color(n.community),
                    };
                    let dimmed = focus_idx.is_some() && !focused_set.contains(&idx);
                    let color = if dimmed {
                        Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), 45)
                    } else {
                        base
                    };
                    let r = (n.radius * self.cam_zoom).clamp(3.0, 26.0);
                    painter.circle_filled(sp, r, color);

                    if Some(idx) == self.selected_node_idx || Some(idx) == hovered_node {
                        painter.circle_stroke(sp, r + 2.5, Stroke::new(2.2_f32, Color32::WHITE));
                    } else if focus_idx.is_some() && focused_set.contains(&idx) {
                        painter.circle_stroke(
                            sp,
                            r + 1.5,
                            Stroke::new(1.4_f32, Color32::from_rgb(56, 189, 248)),
                        );
                    }
                }

                // 4. Smart Collision-Free Labels (highest importance first — ZERO overlapping text!)
                let mut label_order: Vec<usize> = (0..lp.nodes.len())
                    .filter(|&i| visible[i])
                    .collect();
                label_order.sort_by(|&a, &b| {
                    let prio = |i: usize| -> i32 {
                        if Some(i) == hovered_node {
                            4
                        } else if Some(i) == self.selected_node_idx {
                            3
                        } else if focus_idx.is_some() && focused_set.contains(&i) {
                            2
                        } else {
                            0
                        }
                    };
                    prio(b)
                        .cmp(&prio(a))
                        .then_with(|| lp.nodes[b].degree.cmp(&lp.nodes[a].degree))
                        .then_with(|| {
                            lp.nodes[b]
                                .rank
                                .partial_cmp(&lp.nodes[a].rank)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .then_with(|| lp.nodes[b].tokens.cmp(&lp.nodes[a].tokens))
                });

                let mut occupied_labels: Vec<Rect> = Vec::new();
                let font = egui::FontId::proportional(11.0);
                for idx in label_order {
                    let is_special = Some(idx) == hovered_node
                        || Some(idx) == self.selected_node_idx
                        || (focus_idx.is_some() && focused_set.contains(&idx));
                    if focus_idx.is_some() && !is_special {
                        continue;
                    }
                    let n = &lp.nodes[idx];
                    let sp = to_screen(n.x, n.y, self.cam_offset, self.cam_zoom);
                    if !rect.contains(sp) {
                        continue;
                    }
                    let r = (n.radius * self.cam_zoom).clamp(3.0, 26.0);
                    let est_w = (n.label.len() as f32 * 6.4 + 10.0).min(170.0);
                    let est_h = 15.0;
                    let label_center = Pos2::new(sp.x, sp.y + r + 9.0);
                    let label_rect =
                        Rect::from_center_size(label_center, Vec2::new(est_w, est_h));

                    // Strict collision avoidance unless hovered/selected
                    if !is_special && occupied_labels.iter().any(|o| o.intersects(label_rect)) {
                        continue;
                    }
                    occupied_labels.push(label_rect.expand(2.0));

                    // Crisp dark pill behind focused/selected labels for instant legibility
                    if is_special {
                        painter.rect_filled(
                            label_rect.expand2(Vec2::new(3.0, 1.0)),
                            CornerRadius::same(3),
                            Color32::from_rgba_unmultiplied(10, 14, 24, 220),
                        );
                    }
                    painter.text(
                        label_center,
                        egui::Align2::CENTER_CENTER,
                        &n.label,
                        font.clone(),
                        if is_special {
                            Color32::WHITE
                        } else {
                            Color32::from_rgb(203, 213, 225)
                        },
                    );
                }

                // 5. Floating Hover Tooltip in top-left of canvas
                if let Some(hi) = hovered_node {
                    let n = &lp.nodes[hi];
                    let tip_text = format!(
                        "{}  ·  {}  ·  {} tok  ·  {} symbols  ·  {} links",
                        n.id,
                        reg.display_name_for(&n.language),
                        fmt_num(n.tokens),
                        n.symbols.len(),
                        n.degree
                    );
                    let tip_rect = Rect::from_min_size(
                        Pos2::new(rect.left() + 10.0, rect.top() + 10.0),
                        Vec2::new((tip_text.len() as f32 * 6.5 + 18.0).min(rect.width() - 20.0), 26.0),
                    );
                    painter.rect_filled(
                        tip_rect,
                        CornerRadius::same(6),
                        Color32::from_rgba_unmultiplied(18, 24, 38, 235),
                    );
                    painter.rect_stroke(
                        tip_rect,
                        CornerRadius::same(6),
                        Stroke::new(1.0_f32, Color32::from_rgb(56, 189, 248)),
                        StrokeKind::Inside,
                    );
                    painter.text(
                        Pos2::new(tip_rect.left() + 9.0, tip_rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        tip_text,
                        egui::FontId::proportional(11.5),
                        Color32::WHITE,
                    );
                }

                // Bottom-left mini legend
                let vis_count = visible.iter().filter(|&&v| v).count();
                painter.text(
                    Pos2::new(rect.left() + 10.0, rect.bottom() - 12.0),
                    egui::Align2::LEFT_CENTER,
                    format!(
                        "Showing {vis_count}/{} files in {} groups · Scroll to zoom · Drag background to pan · Click node to inspect",
                        lp.nodes.len(),
                        lp.islands.len()
                    ),
                    egui::FontId::proportional(11.0),
                    Color32::from_rgb(139, 155, 184),
                );
            }

            // Right: Symbol Inspector when a node is selected
            if let (Some(idx), Some(lp)) = (self.selected_node_idx, &self.loaded) {
                if let Some(node) = lp.nodes.get(idx) {
                    ui.allocate_ui(Vec2::new(inspector_w, (avail.y - 6.0).max(200.0)), |ui| {
                        card_frame().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&node.label).strong().size(14.0));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.small_button("x").clicked() {
                                            self.selected_node_idx = None;
                                        }
                                    },
                                );
                            });
                            ui.label(
                                RichText::new(&node.id)
                                    .small()
                                    .color(Color32::from_rgb(139, 155, 184)),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "{} · {} lines · {} tok · {} links",
                                    reg.display_name_for(&node.language),
                                    node.lines,
                                    fmt_num(node.tokens),
                                    node.degree
                                ))
                                .small()
                                .color(Color32::from_rgb(56, 189, 248)),
                            );
                            ui.separator();
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                for (name, kind, line, sig) in &node.symbols {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(
                                            RichText::new(format!("[{kind}]"))
                                                .monospace()
                                                .small()
                                                .color(Color32::from_rgb(56, 189, 248)),
                                        );
                                        ui.label(RichText::new(name).strong().small());
                                        ui.label(
                                            RichText::new(format!("L{line}"))
                                                .small()
                                                .color(Color32::from_rgb(139, 155, 184)),
                                        );
                                    });
                                    ui.label(
                                        RichText::new(sig)
                                            .monospace()
                                            .small()
                                            .color(Color32::from_rgb(148, 163, 184)),
                                    );
                                    ui.add_space(4.0);
                                }
                            });
                        });
                    });
                }
            }
        });
    }

    fn render_architecture_tab(&mut self, ui: &mut egui::Ui) {
        let Some(ref lp) = self.loaded else {
            return;
        };

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "{} files · {} symbols · {} graph edges · {} total tokens",
                    fmt_num(lp.total_files),
                    fmt_num(lp.total_symbols),
                    fmt_num(lp.total_edges),
                    fmt_num(lp.total_repo_tokens)
                ))
                .color(Color32::from_rgb(148, 163, 184)),
            );
            ui.add_space(6.0);

            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("LANGUAGE BREAKDOWN")
                        .strong()
                        .small()
                        .color(Color32::from_rgb(148, 163, 184)),
                );
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    for (_, display, hex, files, toks) in &lp.lang_breakdown {
                        let col = parse_hex_color(hex);
                        egui::Frame::NONE
                            .fill(Color32::from_rgb(18, 24, 38))
                            .stroke(Stroke::new(1.0_f32, col))
                            .corner_radius(CornerRadius::same(6))
                            .inner_margin(8.0)
                            .show(ui, |ui| {
                                ui.label(RichText::new(display).strong().color(col));
                                ui.label(
                                    RichText::new(format!("{files} files · {} tok", fmt_num(*toks)))
                                        .small(),
                                );
                            });
                    }
                });
            });

            ui.add_space(10.0);

            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("ARCHITECTURAL HUBS (GOD NODES)")
                        .strong()
                        .small()
                        .color(Color32::from_rgb(148, 163, 184)),
                );
                ui.add_space(6.0);
                egui::Grid::new("god_nodes_grid")
                    .striped(true)
                    .spacing([14.0, 6.0])
                    .show(ui, |ui| {
                        for h in ["Symbol", "Kind", "File", "In", "Out", "Score"] {
                            ui.label(
                                RichText::new(h)
                                    .strong()
                                    .small()
                                    .color(Color32::from_rgb(148, 163, 184)),
                            );
                        }
                        ui.end_row();
                        for g in &lp.god_nodes {
                            ui.label(RichText::new(&g.name).monospace().strong());
                            ui.label(
                                RichText::new(g.kind.as_str())
                                    .color(Color32::from_rgb(56, 189, 248)),
                            );
                            ui.label(&g.file_path);
                            ui.label(g.in_degree.to_string());
                            ui.label(g.out_degree.to_string());
                            ui.label(format!("{:.2}", g.score));
                            ui.end_row();
                        }
                    });
            });

            ui.add_space(10.0);

            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("MODULE CLUSTERS")
                        .strong()
                        .small()
                        .color(Color32::from_rgb(148, 163, 184)),
                );
                ui.add_space(6.0);
                for c in &lp.communities {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(format!("C{} {}", c.id, c.label))
                                .strong()
                                .color(cluster_color(c.id)),
                        );
                        ui.label(
                            RichText::new(format!(
                                "({} files · {} symbols · {} tok)",
                                c.files.len(),
                                c.symbol_count,
                                fmt_num(c.total_tokens)
                            ))
                            .small()
                            .color(Color32::from_rgb(139, 155, 184)),
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
}

pub fn run_desktop_app(initial_repo: Option<PathBuf>) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("TrackerTop — AI Codebase Graph & Token Savings")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([880.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "TrackerTop",
        options,
        Box::new(move |_cc| Ok(Box::new(TrackerTopApp::new(initial_repo)))),
    )
    .map_err(|e| anyhow::anyhow!("GUI error: {e}"))
}

fn load_project_details(repo_root: &Path) -> Result<LoadedProject> {
    let db = Database::open_for_repo(repo_root)?;
    let cg = CodeGraph::load(&db)?;
    let stats = db.get_token_stats(30)?;
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
    lang_breakdown.sort_by(|a, b| b.4.cmp(&a.4));

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
        let radius =
            (6.0 + (f.token_count as f32).sqrt() * 0.11 + (rank as f32) * 24.0).clamp(6.0, 20.0);

        nodes.push(GraphNodeVis {
            id: f.path.clone(),
            label: f.path.rsplit('/').next().unwrap_or(&f.path).to_string(),
            group_folder: folder_bucket(&f.path),
            language: f.language.clone(),
            community: *cg.file_communities.get(&f.path).unwrap_or(&0),
            tokens: f.token_count,
            lines: f.line_count,
            rank,
            degree: 0,
            x: 0.0,
            y: 0.0,
            radius,
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
        if let (Some(&si), Some(&ti)) = (idx_by_path.get(&s.file_path), idx_by_path.get(&t.file_path))
        {
            if edge_seen.insert((si, ti)) {
                nodes[si].degree += 1;
                nodes[ti].degree += 1;
                edges.push(GraphEdgeVis { src: si, tgt: ti });
            }
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
        stats,
        nodes,
        edges,
        islands: Vec::new(),
    })
}

/// Extracts a clean 2-level folder bucket so large repos divide into readable architectural islands.
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

/// Arranges visible nodes into clean, non-overlapping islands arranged on a balanced grid.
/// Inside each island, nodes sit in concentric rings sorted by importance (hubs in center, leaves outside).
fn arrange_structured_islands(
    nodes: &mut [GraphNodeVis],
    visible: &[bool],
    mode: GraphLayoutMode,
) -> Vec<GraphIsland> {
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (idx, n) in nodes.iter().enumerate() {
        if !visible[idx] {
            continue;
        }
        let key = match mode {
            GraphLayoutMode::Folders => n.group_folder.clone(),
            GraphLayoutMode::Clusters => format!("Cluster {}", n.community + 1),
        };
        groups.entry(key).or_default().push(idx);
    }

    // Sort groups largest-first so the layout feels balanced
    let mut group_list: Vec<(String, Vec<usize>)> = groups.into_iter().collect();
    group_list.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));

    // Precompute each island's local coordinates and bounding radius (guaranteed zero node overlap)
    struct LocalIsland {
        title: String,
        indices: Vec<usize>,
        local_pos: Vec<(f32, f32)>,
        radius: f32,
        color_idx: usize,
    }

    let mut local_islands = Vec::with_capacity(group_list.len());
    for (g_idx, (title, mut indices)) in group_list.into_iter().enumerate() {
        // Place highest-degree / highest-rank hubs at the center of each island
        indices.sort_by(|&a, &b| {
            nodes[b]
                .degree
                .cmp(&nodes[a].degree)
                .then_with(|| nodes[b].tokens.cmp(&nodes[a].tokens))
        });

        let mut local_pos = Vec::with_capacity(indices.len());
        let mut max_extent = 38.0_f32;

        if indices.len() == 1 {
            local_pos.push((0.0, 0.0));
            max_extent = nodes[indices[0]].radius + 26.0;
        } else {
            // Concentric non-overlapping rings with generous spacing
            let mut placed = 0usize;
            let mut ring_radius = 0.0_f32;

            while placed < indices.len() {
                if placed == 0 {
                    local_pos.push((0.0, 0.0));
                    ring_radius = nodes[indices[0]].radius + 36.0;
                    placed = 1;
                } else {
                    let spacing = 34.0_f32;
                    let circumference = std::f32::consts::TAU * ring_radius;
                    let cap = (circumference / spacing).floor().max(6.0) as usize;
                    let count = (indices.len() - placed).min(cap);
                    let angle_offset = (g_idx as f32) * 0.37;
                    for k in 0..count {
                        let angle = angle_offset
                            + (k as f32 / count as f32) * std::f32::consts::TAU;
                        let x = angle.cos() * ring_radius;
                        let y = angle.sin() * ring_radius;
                        let nr = nodes[indices[placed + k]].radius;
                        max_extent = max_extent.max(ring_radius + nr + 22.0);
                        local_pos.push((x, y));
                    }
                    placed += count;
                    ring_radius += 38.0;
                }
            }
        }

        local_islands.push(LocalIsland {
            title,
            indices,
            local_pos,
            radius: max_extent,
            color_idx: g_idx,
        });
    }

    // Place islands into clean non-overlapping rows with generous margins
    let total_islands = local_islands.len().max(1);
    let cols = (total_islands as f32).sqrt().ceil().max(1.0) as usize;
    let gap = 58.0_f32;

    let mut islands = Vec::with_capacity(local_islands.len());
    let mut row_y = 0.0_f32;
    let mut i = 0usize;

    while i < local_islands.len() {
        let row_end = (i + cols).min(local_islands.len());
        let row_slice = &local_islands[i..row_end];
        let row_max_r = row_slice
            .iter()
            .map(|isl| isl.radius)
            .fold(0.0_f32, f32::max);
        let row_total_w: f32 = row_slice.iter().map(|isl| isl.radius * 2.0).sum::<f32>()
            + gap * (row_slice.len().saturating_sub(1) as f32);

        let mut cursor_x = -row_total_w * 0.5;
        let cy = row_y + row_max_r;

        for isl in row_slice {
            let cx = cursor_x + isl.radius;
            for (&node_idx, &(lx, ly)) in isl.indices.iter().zip(isl.local_pos.iter()) {
                nodes[node_idx].x = cx + lx;
                nodes[node_idx].y = cy + ly;
            }
            islands.push(GraphIsland {
                title: isl.title.clone(),
                center_x: cx,
                center_y: cy,
                radius: isl.radius,
                count: isl.indices.len(),
                color_idx: isl.color_idx,
            });
            cursor_x += isl.radius * 2.0 + gap;
        }

        row_y += row_max_r * 2.0 + gap + 22.0;
        i = row_end;
    }

    islands
}

fn compute_bounds(
    nodes: &[GraphNodeVis],
    islands: &[GraphIsland],
    visible: &[bool],
) -> (f32, f32, f32, f32) {
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;

    for isl in islands {
        min_x = min_x.min(isl.center_x - isl.radius);
        max_x = max_x.max(isl.center_x + isl.radius);
        min_y = min_y.min(isl.center_y - isl.radius - 20.0);
        max_y = max_y.max(isl.center_y + isl.radius);
    }

    if !min_x.is_finite() {
        for (i, n) in nodes.iter().enumerate() {
            if !visible[i] {
                continue;
            }
            min_x = min_x.min(n.x - n.radius);
            max_x = max_x.max(n.x + n.radius);
            min_y = min_y.min(n.y - n.radius);
            max_y = max_y.max(n.y + n.radius);
        }
    }

    if !min_x.is_finite() {
        (-100.0, 100.0, -100.0, 100.0)
    } else {
        (min_x, max_x, min_y, max_y)
    }
}

fn apply_dark_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(13, 18, 30);
    visuals.window_fill = Color32::from_rgb(18, 24, 38);
    visuals.extreme_bg_color = Color32::from_rgb(10, 14, 24);
    ctx.set_visuals(visuals);
}

fn card_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(Color32::from_rgb(20, 27, 42))
        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(36, 48, 74)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(12.0)
}

fn kpi_card(ui: &mut egui::Ui, label: &str, value: &str, sub: &str, color: Color32) {
    card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(
            RichText::new(label)
                .small()
                .color(Color32::from_rgb(139, 155, 184)),
        );
        ui.add_space(2.0);
        ui.label(RichText::new(value).strong().size(22.0).color(color));
        ui.label(
            RichText::new(sub)
                .small()
                .color(Color32::from_rgb(139, 155, 184)),
        );
    });
}

fn parse_hex_color(hex: &str) -> Color32 {
    let h = hex.trim_start_matches('#');
    if h.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&h[0..2], 16),
            u8::from_str_radix(&h[2..4], 16),
            u8::from_str_radix(&h[4..6], 16),
        ) {
            return Color32::from_rgb(r, g, b);
        }
    }
    Color32::from_rgb(148, 163, 184)
}

fn cluster_color(id: usize) -> Color32 {
    const PALETTE: [(u8, u8, u8); 8] = [
        (56, 189, 248),
        (52, 211, 153),
        (251, 146, 60),
        (167, 139, 250),
        (244, 114, 182),
        (251, 191, 36),
        (34, 211, 238),
        (248, 113, 113),
    ];
    let (r, g, b) = PALETTE[id % PALETTE.len()];
    Color32::from_rgb(r, g, b)
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

fn truncate_label(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(3)).collect();
        t.push_str("...");
        t
    }
}
