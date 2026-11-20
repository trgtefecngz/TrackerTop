#![cfg(feature = "gui")]

use crate::context::TrackerEngine;
use crate::db::Database;
use crate::graph::{CodeGraph, CommunitySummary, HealthReport, ScoredNode};
use crate::indexer::index_repository;
use crate::install::{AiAgentKind, generic_mcp_json_snippet, install_agent, is_agent_installed};
use crate::model::{EdgeKind, ProjectNote, SymbolKind, TokenStatsSummary};
use crate::parser::default_registry;
use crate::registry::{
    GlobalOverview, load_global_overview, register_project, remove_project_with_cache,
};
use anyhow::Result;
use eframe::egui::{
    self, Color32, CornerRadius, Margin, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2,
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
    Memory,
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
    symbols: Vec<(String, String, usize, String)>,
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
    lang_breakdown: Vec<(String, String, String, usize, usize)>,
    god_nodes: Vec<ScoredNode>,
    communities: Vec<CommunitySummary>,
    health: HealthReport,
    notes: Vec<ProjectNote>,
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
    connected_agents: Vec<(AiAgentKind, bool)>,
    show_agents_modal: bool,
    status_banner: String,
    last_refresh: Instant,
    bg_rx: Option<Receiver<BgMessage>>,
    bg_busy: bool,

    // AI Memory form inputs
    note_target_input: String,
    note_content_input: String,

    // Smooth GPU radial aurora texture & dry-run preview state
    backdrop_tex: Option<egui::TextureHandle>,
    preview_result: Option<crate::context::QueryOutput>,

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
            backdrop_tex: None,
            preview_result: None,
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
        self.connected_agents = poll_agent_statuses();
    }

    fn reload_selected_project(&mut self) {
        let Selection::Project(ref path) = self.selection else {
            self.loaded = None;
            return;
        };
        self.selected_node_idx = None;
        self.dragged_node_idx = None;
        self.preview_result = None;
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
        ctx.request_repaint_after(Duration::from_millis(if self.bg_busy { 100 } else { 750 }));

        apply_soft_modern_theme(ctx);
        let bg_tex_id = self
            .backdrop_tex
            .get_or_insert_with(|| build_smooth_aurora_texture(ctx))
            .id();

        // Top Header Bar
        egui::TopBottomPanel::top("top_bar")
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(10, 15, 27))
                    .inner_margin(Margin::symmetric(18, 12))
                    .stroke(Stroke::new(
                        1.0_f32,
                        Color32::from_rgba_unmultiplied(148, 163, 184, 24),
                    )),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    // Glowing brand badge
                    egui::Frame::NONE
                        .fill(Color32::from_rgba_unmultiplied(56, 189, 248, 28))
                        .stroke(Stroke::new(
                            1.0_f32,
                            Color32::from_rgba_unmultiplied(56, 189, 248, 90),
                        ))
                        .corner_radius(CornerRadius::same(12))
                        .inner_margin(Margin::symmetric(10, 5))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new("TrackerTop")
                                    .strong()
                                    .size(16.0)
                                    .color(Color32::from_rgb(125, 211, 252)),
                            );
                        });

                    ui.add_space(10.0);

                    match &self.selection {
                        Selection::AllProjects => {
                            ui.label(
                                RichText::new("Global Workspace")
                                    .strong()
                                    .size(14.5)
                                    .color(Color32::from_rgb(226, 232, 240)),
                            );
                        }
                        Selection::Project(path) => {
                            let name = Path::new(path)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or(path);
                            ui.label(
                                RichText::new(name)
                                    .strong()
                                    .size(15.0)
                                    .color(Color32::WHITE),
                            );
                            ui.label(
                                RichText::new(path)
                                    .small()
                                    .color(Color32::from_rgb(130, 148, 178)),
                            );
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let connected_count =
                            self.connected_agents.iter().filter(|(_, ok)| *ok).count();
                        let agents_label = if connected_count > 0 {
                            format!("AI Agents ({connected_count} connected)")
                        } else {
                            "Connect AI Agents".to_string()
                        };
                        let agents_btn =
                            egui::Button::new(RichText::new(agents_label).strong().color(
                                if connected_count > 0 {
                                    Color32::from_rgb(110, 231, 183)
                                } else {
                                    Color32::WHITE
                                },
                            ))
                            .corner_radius(CornerRadius::same(18))
                            .fill(if connected_count > 0 {
                                Color32::from_rgba_unmultiplied(16, 185, 129, 34)
                            } else {
                                Color32::from_rgb(37, 99, 235)
                            })
                            .stroke(Stroke::new(
                                1.0_f32,
                                if connected_count > 0 {
                                    Color32::from_rgba_unmultiplied(52, 211, 153, 110)
                                } else {
                                    Color32::from_rgb(96, 165, 250)
                                },
                            ));
                        if ui.add(agents_btn).clicked() {
                            self.show_agents_modal = true;
                        }

                        if let Selection::Project(ref p) = self.selection.clone() {
                            ui.add_space(6.0);
                            if ui
                                .add_enabled(
                                    !self.bg_busy,
                                    egui::Button::new("Re-Index")
                                        .corner_radius(CornerRadius::same(16))
                                        .fill(Color32::from_rgba_unmultiplied(30, 41, 59, 180)),
                                )
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
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(8, 12, 22))
                    .inner_margin(Margin::symmetric(16, 6))
                    .stroke(Stroke::new(
                        1.0_f32,
                        Color32::from_rgba_unmultiplied(148, 163, 184, 18),
                    )),
            )
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
                            RichText::new(format!("Open Core: {}", langs.join(" · ")))
                                .small()
                                .color(Color32::from_rgb(100, 116, 139)),
                        );
                    });
                });
            });

        // Left Sidebar: Soft Rounded Multi-Project Switcher
        egui::SidePanel::left("projects_sidebar")
            .resizable(true)
            .default_width(260.0)
            .min_width(215.0)
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(10, 15, 27))
                    .inner_margin(Margin::same(14))
                    .stroke(Stroke::new(
                        1.0_f32,
                        Color32::from_rgba_unmultiplied(148, 163, 184, 20),
                    )),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("PROJECTS")
                            .strong()
                            .small()
                            .color(Color32::from_rgb(130, 148, 178)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let add_btn = egui::Button::new(
                            RichText::new("+ Add")
                                .strong()
                                .color(Color32::from_rgb(125, 211, 252)),
                        )
                        .corner_radius(CornerRadius::same(14))
                        .fill(Color32::from_rgba_unmultiplied(56, 189, 248, 24))
                        .stroke(Stroke::new(
                            1.0_f32,
                            Color32::from_rgba_unmultiplied(56, 189, 248, 80),
                        ));
                        if ui.add_enabled(!self.bg_busy, add_btn).clicked() {
                            self.pick_and_add_project();
                        }
                    });
                });
                ui.add_space(10.0);

                // All Projects pill
                let all_selected = self.selection == Selection::AllProjects;
                let all_btn = egui::Button::new(
                    RichText::new(format!("All Projects  ({})", self.overview.projects.len()))
                        .strong()
                        .color(if all_selected {
                            Color32::WHITE
                        } else {
                            Color32::from_rgb(203, 213, 225)
                        }),
                )
                .min_size(Vec2::new(ui.available_width(), 38.0))
                .corner_radius(CornerRadius::same(14))
                .fill(if all_selected {
                    Color32::from_rgba_unmultiplied(56, 189, 248, 42)
                } else {
                    Color32::from_rgba_unmultiplied(30, 41, 59, 110)
                })
                .stroke(Stroke::new(
                    1.0_f32,
                    if all_selected {
                        Color32::from_rgba_unmultiplied(56, 189, 248, 140)
                    } else {
                        Color32::from_rgba_unmultiplied(148, 163, 184, 24)
                    },
                ));
                if ui.add(all_btn).clicked() {
                    self.selection = Selection::AllProjects;
                    self.loaded = None;
                }

                ui.add_space(10.0);

                let mut to_select: Option<String> = None;
                let mut to_remove: Option<(String, bool)> = None;

                egui::ScrollArea::vertical().show(ui, |ui| {
                    if self.overview.projects.is_empty() {
                        soft_card_frame(12).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new("No projects pinned yet.")
                                    .strong()
                                    .small()
                                    .color(Color32::from_rgb(203, 213, 225)),
                            );
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(
                                    "Add a folder above, or just open any repo in Claude Code — it appears here automatically.",
                                )
                                .small()
                                .color(Color32::from_rgb(130, 148, 178)),
                            );
                        });
                    }

                    for snap in &self.overview.projects {
                        let is_sel = matches!(&self.selection, Selection::Project(p) if p.eq_ignore_ascii_case(&snap.entry.path));
                        let frame = egui::Frame::NONE
                            .fill(if is_sel {
                                Color32::from_rgba_unmultiplied(56, 189, 248, 32)
                            } else {
                                Color32::from_rgba_unmultiplied(22, 31, 49, 185)
                            })
                            .stroke(Stroke::new(
                                1.0_f32,
                                if is_sel {
                                    Color32::from_rgba_unmultiplied(56, 189, 248, 150)
                                } else {
                                    Color32::from_rgba_unmultiplied(148, 163, 184, 24)
                                },
                            ))
                            .corner_radius(CornerRadius::same(14))
                            .inner_margin(Margin::same(11));

                        frame.show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let dot_col = if snap.stats.total_tokens_saved > 0 {
                                    Color32::from_rgb(52, 211, 153)
                                } else {
                                    Color32::from_rgb(56, 189, 248)
                                };
                                let (dot_rect, _) =
                                    ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                                ui.painter().circle_filled(dot_rect.center(), 4.0, dot_col);

                                let title = ui.add(
                                    egui::Label::new(
                                        RichText::new(&snap.entry.name)
                                            .strong()
                                            .color(Color32::WHITE),
                                    )
                                    .sense(Sense::click()),
                                );
                                if title.clicked() {
                                    to_select = Some(snap.entry.path.clone());
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add(
                                                egui::Button::new(RichText::new("×").small())
                                                    .corner_radius(CornerRadius::same(10))
                                                    .fill(Color32::TRANSPARENT),
                                            )
                                            .on_hover_text(
                                                "Remove from list (Shift+Click to also delete local .trackertop cache)",
                                            )
                                            .clicked()
                                        {
                                            let wipe = ui.input(|i| i.modifiers.shift);
                                            to_remove = Some((snap.entry.path.clone(), wipe));
                                        }
                                    },
                                );
                            });

                            ui.add_space(3.0);
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
                                            Color32::from_rgb(110, 231, 183)
                                        } else {
                                            Color32::from_rgb(148, 163, 184)
                                        }),
                                )
                                .sense(Sense::click()),
                            );
                            if sub_resp.clicked() {
                                to_select = Some(snap.entry.path.clone());
                            }
                        });
                        ui.add_space(8.0);
                    }
                });

                if let Some((rem, wipe)) = to_remove {
                    let _ = remove_project_with_cache(&rem, wipe);
                    if matches!(&self.selection, Selection::Project(p) if p.eq_ignore_ascii_case(&rem))
                    {
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
            });

        // Main Central Panel with Ambient Radial Glow Backdrop
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(7, 11, 20))
                    .inner_margin(Margin::same(18)),
            )
            .show(ctx, |ui| {
                paint_smooth_aurora(ui.painter(), ui.max_rect(), bg_tex_id);
                match &self.selection {
                    Selection::AllProjects => self.render_all_projects(ui),
                    Selection::Project(_) => self.render_single_project(ui),
                }
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
                    .fill(Color32::from_rgb(15, 22, 38))
                    .stroke(Stroke::new(
                        1.0_f32,
                        Color32::from_rgba_unmultiplied(56, 189, 248, 90),
                    ))
                    .corner_radius(CornerRadius::same(18))
                    .inner_margin(Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Connect TrackerTop once globally — every repository you open in any connected AI agent is indexed and tracked automatically.",
                    )
                    .small()
                    .color(Color32::from_rgb(148, 163, 184)),
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
                                        .strong()
                                        .color(Color32::WHITE),
                                );
                                ui.label(
                                    RichText::new(agent.description())
                                        .small()
                                        .color(Color32::from_rgb(130, 148, 178)),
                                );
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let label = if connected { "Connected ✓" } else { "Connect" };
                                let btn = egui::Button::new(RichText::new(label).strong().color(
                                    if connected {
                                        Color32::from_rgb(110, 231, 183)
                                    } else {
                                        Color32::WHITE
                                    },
                                ))
                                .corner_radius(CornerRadius::same(14))
                                .fill(if connected {
                                    Color32::from_rgba_unmultiplied(16, 185, 129, 32)
                                } else {
                                    Color32::from_rgb(37, 99, 235)
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
                                .strong()
                                .small(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(
                                    egui::Button::new("Copy MCP JSON")
                                        .corner_radius(CornerRadius::same(14)),
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
                Color32::from_rgb(129, 140, 248),
            );
            glow_kpi_card(
                &mut cols[1],
                "TOTAL AI QUERIES",
                &fmt_num(ov.total_queries),
                &format!("Sent {} tok", fmt_num(ov.total_tokens_sent)),
                Color32::from_rgb(125, 211, 252),
                Color32::from_rgb(56, 189, 248),
            );
            glow_kpi_card(
                &mut cols[2],
                "NET TOKENS SAVED",
                &fmt_num(ov.total_tokens_saved),
                &format!("{:.1}% reduction vs raw reads", ov.savings_percent),
                Color32::from_rgb(110, 231, 183),
                Color32::from_rgb(52, 211, 153),
            );
            glow_kpi_card(
                &mut cols[3],
                "EST. USD SAVED",
                &format!("${:.4}", ov.est_usd_saved_sonnet),
                &format!("Sonnet · ${:.4} Opus", ov.est_usd_saved_opus),
                Color32::from_rgb(110, 231, 183),
                Color32::from_rgb(16, 185, 129),
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
                            .strong()
                            .size(20.0)
                            .color(Color32::WHITE),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "Add any project folder to inspect its architecture and token savings, or simply open a repository in Claude Code — TrackerTop tracks it automatically in the background.",
                        )
                        .color(Color32::from_rgb(148, 163, 184)),
                    );
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        ui.add_space(100.0);
                        let add = egui::Button::new(
                            RichText::new("+ Add Your First Project")
                                .strong()
                                .color(Color32::WHITE),
                        )
                        .corner_radius(CornerRadius::same(18))
                        .fill(Color32::from_rgb(37, 99, 235));
                        if ui.add(add).clicked() {
                            self.pick_and_add_project();
                        }
                        ui.add_space(8.0);
                        if ui
                            .add(
                                egui::Button::new("Connect AI Agents")
                                    .corner_radius(CornerRadius::same(18)),
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
                    .strong()
                    .small()
                    .color(Color32::from_rgb(148, 163, 184)),
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
                                    .strong()
                                    .small()
                                    .color(Color32::from_rgb(130, 148, 178)),
                            );
                        }
                        ui.end_row();

                        for p in &ov.projects {
                            ui.label(RichText::new(&p.entry.name).strong().color(Color32::WHITE));
                            ui.label(fmt_num(p.total_files));
                            ui.label(fmt_num(p.total_repo_tokens));
                            ui.label(fmt_num(p.stats.total_queries));
                            ui.label(
                                RichText::new(fmt_num(p.stats.total_tokens_saved))
                                    .strong()
                                    .color(Color32::from_rgb(110, 231, 183)),
                            );
                            ui.label(format!("{:.1}%", p.stats.savings_percent));
                            if ui
                                .add(
                                    egui::Button::new("Open").corner_radius(CornerRadius::same(14)),
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
        // Floating Rounded Pill Tab Bar
        egui::Frame::NONE
            .fill(Color32::from_rgba_unmultiplied(15, 23, 42, 200))
            .stroke(Stroke::new(
                1.0_f32,
                Color32::from_rgba_unmultiplied(148, 163, 184, 28),
            ))
            .corner_radius(CornerRadius::same(20))
            .inner_margin(Margin::symmetric(6, 5))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (tab, label) in [
                        (ProjectTab::Savings, "Overview & Savings"),
                        (ProjectTab::Graph, "Code Graph"),
                        (ProjectTab::Architecture, "Architecture & Health"),
                        (ProjectTab::Memory, "AI Memory"),
                    ] {
                        let active = self.active_tab == tab;
                        let btn = egui::Button::new(
                            RichText::new(label).strong().size(13.5).color(if active {
                                Color32::WHITE
                            } else {
                                Color32::from_rgb(148, 163, 184)
                            }),
                        )
                        .corner_radius(CornerRadius::same(16))
                        .fill(if active {
                            Color32::from_rgba_unmultiplied(56, 189, 248, 52)
                        } else {
                            Color32::TRANSPARENT
                        })
                        .stroke(if active {
                            Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(56, 189, 248, 140))
                        } else {
                            Stroke::NONE
                        });
                        if ui.add(btn).clicked() {
                            if tab == ProjectTab::Graph && self.active_tab != ProjectTab::Graph {
                                self.needs_fit_view = true;
                            }
                            self.active_tab = tab;
                        }
                    }
                });
            });

        ui.add_space(12.0);

        if self.loaded.is_none() {
            soft_card_frame(16).show(ui, |ui| {
                ui.label("Project is not indexed yet. Click 'Re-Index' in the top bar.");
            });
            return;
        }

        match self.active_tab {
            ProjectTab::Savings => self.render_savings_tab(ui),
            ProjectTab::Graph => self.render_graph_tab(ui),
            ProjectTab::Architecture => self.render_architecture_tab(ui),
            ProjectTab::Memory => self.render_memory_tab(ui),
        }
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
                "NET TOKENS SAVED",
                &fmt_num(s.total_tokens_saved),
                &format!("{:.1}% reduction", s.savings_percent),
                Color32::from_rgb(110, 231, 183),
                Color32::from_rgb(52, 211, 153),
            );
            glow_kpi_card(
                &mut cols[1],
                "SENT VS BASELINE",
                &fmt_num(s.total_tokens_sent),
                &format!("vs {} raw baseline tok", fmt_num(s.total_baseline_tokens)),
                Color32::from_rgb(125, 211, 252),
                Color32::from_rgb(56, 189, 248),
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
                Color32::from_rgb(129, 140, 248),
            );
            glow_kpi_card(
                &mut cols[3],
                "EST. USD SAVED",
                &format!("${:.4}", s.est_usd_saved_sonnet),
                &format!("Sonnet · ${:.4} Opus", s.est_usd_saved_opus),
                Color32::from_rgb(110, 231, 183),
                Color32::from_rgb(16, 185, 129),
            );
        });

        ui.add_space(14.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            if s.total_queries == 0 {
                egui::Frame::NONE
                    .fill(Color32::from_rgba_unmultiplied(16, 185, 129, 22))
                    .stroke(Stroke::new(
                        1.0_f32,
                        Color32::from_rgba_unmultiplied(52, 211, 153, 95),
                    ))
                    .corner_radius(CornerRadius::same(16))
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
                            .strong()
                            .size(14.5)
                            .color(Color32::from_rgb(110, 231, 183)),
                        );
                        ui.add_space(5.0);
                        ui.label(
                            RichText::new(
                                "TrackerTop works immediately on any existing or new project. Open this folder in Claude Code (or Cursor / Codex) and ask a question — real token savings will stream in automatically.",
                            )
                            .color(Color32::from_rgb(203, 213, 225)),
                        );
                        ui.add_space(10.0);
                        let test_btn = egui::Button::new(
                            RichText::new("Preview Savings (Dry-Run)")
                                .strong()
                                .color(Color32::WHITE),
                        )
                        .corner_radius(CornerRadius::same(16))
                        .fill(Color32::from_rgb(16, 185, 129));
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
                    .fill(Color32::from_rgba_unmultiplied(56, 189, 248, 22))
                    .stroke(Stroke::new(
                        1.0_f32,
                        Color32::from_rgba_unmultiplied(56, 189, 248, 110),
                    ))
                    .corner_radius(CornerRadius::same(16))
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
                                .strong()
                                .small()
                                .color(Color32::from_rgb(125, 211, 252)),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui
                                    .add(egui::Button::new("Close").corner_radius(CornerRadius::same(12)))
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
                                        .color(Color32::from_rgb(203, 213, 225)),
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
                            .strong()
                            .small()
                            .color(Color32::from_rgb(148, 163, 184)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if s.total_queries > 0 {
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("Reset Stats")
                                            .small()
                                            .color(Color32::from_rgb(252, 165, 165)),
                                    )
                                    .corner_radius(CornerRadius::same(14)),
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
                                        .corner_radius(CornerRadius::same(14)),
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
                            .color(Color32::from_rgb(130, 148, 178)),
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
                                    .strong()
                                    .color(Color32::from_rgb(110, 231, 183)),
                                );
                            });
                        });
                        let frac = (t.savings_percent as f32 / 100.0).clamp(0.0, 1.0);
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_height(9.0)
                                .corner_radius(CornerRadius::same(5))
                                .fill(Color32::from_rgb(52, 211, 153)),
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
                        .strong()
                        .small()
                        .color(Color32::from_rgb(148, 163, 184)),
                );
                ui.add_space(8.0);
                if s.recent.is_empty() {
                    ui.label(
                        RichText::new(
                            "Live queries from Claude Code, Cursor, or Codex stream in here every 2 seconds.",
                        )
                        .color(Color32::from_rgb(130, 148, 178)),
                    );
                } else {
                    egui::Grid::new("recent_queries_grid")
                        .striped(true)
                        .spacing([16.0, 8.0])
                        .show(ui, |ui| {
                            for h in ["Tool", "Query", "Sent", "Baseline", "Saved", "Latency"] {
                                ui.label(
                                    RichText::new(h)
                                        .strong()
                                        .small()
                                        .color(Color32::from_rgb(130, 148, 178)),
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
                                        .color(Color32::from_rgb(125, 211, 252)),
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
                                    .color(Color32::from_rgb(110, 231, 183)),
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
        let mut layout_dirty = false;

        // Floating rounded toolbar
        soft_card_frame(16).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.graph_search)
                        .desired_width(165.0)
                        .hint_text("Search file or symbol..."),
                );
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
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

                if ui
                    .add(egui::Button::new("Fit View").corner_radius(CornerRadius::same(14)))
                    .clicked()
                {
                    self.needs_fit_view = true;
                }
            });
        });

        if layout_dirty {
            self.recompute_graph_layout();
        }
        ui.add_space(10.0);

        let avail = ui.available_size();
        let inspector_w = if self.selected_node_idx.is_some() {
            300.0
        } else {
            0.0
        };
        let canvas_w = (avail.x - inspector_w - 10.0).max(240.0);

        ui.horizontal_top(|ui| {
            let (rect, resp) = ui.allocate_exact_size(
                Vec2::new(canvas_w, (avail.y - 6.0).max(200.0)),
                Sense::click_and_drag(),
            );
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, CornerRadius::same(18), Color32::from_rgb(9, 13, 24));
            if let Some(ref tex) = self.backdrop_tex {
                paint_smooth_aurora(&painter, rect, tex.id());
            }
            painter.rect_stroke(
                rect,
                CornerRadius::same(18),
                Stroke::new(
                    1.0_f32,
                    Color32::from_rgba_unmultiplied(148, 163, 184, 34),
                ),
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

                if self.needs_fit_view {
                    let (min_x, max_x, min_y, max_y) =
                        compute_bounds(&lp.nodes, &lp.islands, &visible);
                    let span_x = (max_x - min_x).max(160.0);
                    let span_y = (max_y - min_y).max(160.0);
                    let fit_x = (rect.width() - 68.0) / span_x;
                    let fit_y = (rect.height() - 68.0) / span_y;
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

                if resp.hovered() {
                    let scroll = ui.input(|i| i.raw_scroll_delta.y);
                    if scroll.abs() > 0.1 {
                        let factor = if scroll > 0.0 { 1.12 } else { 0.89 };
                        self.cam_zoom = (self.cam_zoom * factor).clamp(0.10, 4.0);
                    }
                }

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

                // 1. Soft radial island halos & headers
                for isl in &lp.islands {
                    let sp = to_screen(isl.center_x, isl.center_y, self.cam_offset, self.cam_zoom);
                    let sr = isl.radius * self.cam_zoom;
                    if !rect.expand(sr + 30.0).contains(sp) {
                        continue;
                    }
                    let base_col = cluster_color(isl.color_idx);
                    painter.circle_filled(
                        sp,
                        sr * 1.06,
                        Color32::from_rgba_unmultiplied(
                            base_col.r(),
                            base_col.g(),
                            base_col.b(),
                            7,
                        ),
                    );
                    painter.circle_filled(
                        sp,
                        sr,
                        Color32::from_rgba_unmultiplied(
                            base_col.r(),
                            base_col.g(),
                            base_col.b(),
                            15,
                        ),
                    );
                    painter.circle_stroke(
                        sp,
                        sr,
                        Stroke::new(
                            1.0_f32,
                            Color32::from_rgba_unmultiplied(
                                base_col.r(),
                                base_col.g(),
                                base_col.b(),
                                56,
                            ),
                        ),
                    );

                    if sr >= 14.0 {
                        let title_text = format!("{} ({})", isl.title, isl.count);
                        painter.text(
                            Pos2::new(sp.x, sp.y - sr - 10.0),
                            egui::Align2::CENTER_CENTER,
                            title_text,
                            egui::FontId::proportional(11.5),
                            Color32::from_rgba_unmultiplied(215, 224, 238, 228),
                        );
                    }
                }

                // 2. Edges
                for e in &lp.edges {
                    if !visible[e.src] || !visible[e.tgt] {
                        continue;
                    }
                    let is_hl = focus_idx.is_some_and(|fi| e.src == fi || e.tgt == fi);
                    if focus_idx.is_some() && !is_hl {
                        continue;
                    }
                    let a = &lp.nodes[e.src];
                    let b = &lp.nodes[e.tgt];
                    let p1 = to_screen(a.x, a.y, self.cam_offset, self.cam_zoom);
                    let p2 = to_screen(b.x, b.y, self.cam_offset, self.cam_zoom);
                    let stroke = if is_hl {
                        Stroke::new(
                            1.8_f32,
                            Color32::from_rgba_unmultiplied(56, 189, 248, 225),
                        )
                    } else {
                        Stroke::new(
                            0.9_f32,
                            Color32::from_rgba_unmultiplied(148, 163, 184, 34),
                        )
                    };
                    painter.line_segment([p1, p2], stroke);
                }

                // 3. Nodes with soft outer glow on hover/focus
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
                        Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), 42)
                    } else {
                        base
                    };
                    let r = (n.radius * self.cam_zoom).clamp(3.0, 26.0);
                    if !dimmed {
                        painter.circle_filled(
                            sp,
                            r + 4.0,
                            Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), 26),
                        );
                    }
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

                // 4. Collision-free labels
                let mut label_order: Vec<usize> =
                    (0..lp.nodes.len()).filter(|&i| visible[i]).collect();
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
                    let label_rect = Rect::from_center_size(label_center, Vec2::new(est_w, est_h));

                    if !is_special && occupied_labels.iter().any(|o| o.intersects(label_rect)) {
                        continue;
                    }
                    occupied_labels.push(label_rect.expand(2.0));

                    if is_special {
                        painter.rect_filled(
                            label_rect.expand2(Vec2::new(4.0, 2.0)),
                            CornerRadius::same(6),
                            Color32::from_rgba_unmultiplied(10, 14, 24, 225),
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

                // 5. Floating rounded tooltip
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
                        Pos2::new(rect.left() + 12.0, rect.top() + 12.0),
                        Vec2::new(
                            (tip_text.len() as f32 * 6.5 + 22.0).min(rect.width() - 24.0),
                            28.0,
                        ),
                    );
                    painter.rect_filled(
                        tip_rect,
                        CornerRadius::same(12),
                        Color32::from_rgba_unmultiplied(15, 23, 42, 235),
                    );
                    painter.rect_stroke(
                        tip_rect,
                        CornerRadius::same(12),
                        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(56, 189, 248, 150)),
                        StrokeKind::Inside,
                    );
                    painter.text(
                        Pos2::new(tip_rect.left() + 11.0, tip_rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        tip_text,
                        egui::FontId::proportional(11.5),
                        Color32::WHITE,
                    );
                }

                let vis_count = visible.iter().filter(|&&v| v).count();
                painter.text(
                    Pos2::new(rect.left() + 14.0, rect.bottom() - 14.0),
                    egui::Align2::LEFT_CENTER,
                    format!(
                        "{vis_count}/{} files in {} groups · Scroll to zoom · Drag to pan · Click node to inspect",
                        lp.nodes.len(),
                        lp.islands.len()
                    ),
                    egui::FontId::proportional(11.0),
                    Color32::from_rgb(130, 148, 178),
                );
            }

            // Right: Floating Rounded Inspector Drawer
            if let (Some(idx), Some(lp)) = (self.selected_node_idx, &self.loaded)
                && let Some(node) = lp.nodes.get(idx) {
                    ui.add_space(8.0);
                    ui.allocate_ui(Vec2::new(inspector_w, (avail.y - 6.0).max(200.0)), |ui| {
                        soft_card_frame(16).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(&node.label)
                                        .strong()
                                        .size(14.5)
                                        .color(Color32::WHITE),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add(
                                                egui::Button::new("×")
                                                    .corner_radius(CornerRadius::same(10)),
                                            )
                                            .clicked()
                                        {
                                            self.selected_node_idx = None;
                                        }
                                    },
                                );
                            });
                            ui.label(
                                RichText::new(&node.id)
                                    .small()
                                    .color(Color32::from_rgb(130, 148, 178)),
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
                                .color(Color32::from_rgb(125, 211, 252)),
                            );
                            ui.separator();
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                for (name, kind, line, sig) in &node.symbols {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(
                                            RichText::new(format!("[{kind}]"))
                                                .monospace()
                                                .small()
                                                .color(Color32::from_rgb(125, 211, 252)),
                                        );
                                        ui.label(RichText::new(name).strong().small());
                                        ui.label(
                                            RichText::new(format!("L{line}"))
                                                .small()
                                                .color(Color32::from_rgb(130, 148, 178)),
                                        );
                                    });
                                    ui.label(
                                        RichText::new(sig)
                                            .monospace()
                                            .small()
                                            .color(Color32::from_rgb(148, 163, 184)),
                                    );
                                    ui.add_space(5.0);
                                }
                            });
                        });
                    });
                }
        });
    }

    fn render_architecture_tab(&mut self, ui: &mut egui::Ui) {
        let Some(ref lp) = self.loaded else {
            return;
        };

        egui::ScrollArea::vertical().show(ui, |ui| {
            // Code Health Summary Row
            ui.columns(3, |cols| {
                let bloat_col = if lp.health.bloated_files.is_empty() {
                    Color32::from_rgb(110, 231, 183)
                } else {
                    Color32::from_rgb(251, 191, 36)
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
                    Color32::from_rgb(110, 231, 183)
                } else {
                    Color32::from_rgb(248, 113, 113)
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
                    Color32::from_rgb(125, 211, 252),
                    Color32::from_rgb(56, 189, 248),
                );
            });

            ui.add_space(12.0);

            if !lp.health.bloated_files.is_empty() || !lp.health.circular_groups.is_empty() {
                soft_card_frame(16).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new("CODE HEALTH DIAGNOSTICS")
                            .strong()
                            .small()
                            .color(Color32::from_rgb(251, 191, 36)),
                    );
                    ui.add_space(6.0);
                    for (p, toks, lines) in lp.health.bloated_files.iter().take(6) {
                        ui.label(
                            RichText::new(format!(
                                "Bloat: {p} — {} tokens ({lines} lines) — consider splitting",
                                fmt_num(*toks)
                            ))
                            .small()
                            .color(Color32::from_rgb(253, 224, 71)),
                        );
                    }
                    for cyc in lp.health.circular_groups.iter().take(4) {
                        ui.label(
                            RichText::new(format!("Cycle: {}", cyc.join(" <-> ")))
                                .small()
                                .color(Color32::from_rgb(252, 165, 165)),
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
                    .strong()
                    .small()
                    .color(Color32::from_rgb(148, 163, 184)),
                );
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    for (_, display, hex, files, toks) in &lp.lang_breakdown {
                        let col = parse_hex_color(hex);
                        egui::Frame::NONE
                            .fill(Color32::from_rgba_unmultiplied(
                                col.r(),
                                col.g(),
                                col.b(),
                                20,
                            ))
                            .stroke(Stroke::new(
                                1.0_f32,
                                Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 100),
                            ))
                            .corner_radius(CornerRadius::same(14))
                            .inner_margin(Margin::symmetric(12, 8))
                            .show(ui, |ui| {
                                ui.label(RichText::new(display).strong().color(col));
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
                        .strong()
                        .small()
                        .color(Color32::from_rgb(148, 163, 184)),
                );
                ui.add_space(8.0);
                egui::Grid::new("god_nodes_grid")
                    .striped(true)
                    .spacing([14.0, 7.0])
                    .show(ui, |ui| {
                        for h in ["Symbol", "Kind", "File", "In", "Out", "Score"] {
                            ui.label(
                                RichText::new(h)
                                    .strong()
                                    .small()
                                    .color(Color32::from_rgb(130, 148, 178)),
                            );
                        }
                        ui.end_row();
                        for g in &lp.god_nodes {
                            ui.label(RichText::new(&g.name).monospace().strong());
                            ui.label(
                                RichText::new(g.kind.as_str())
                                    .color(Color32::from_rgb(125, 211, 252)),
                            );
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
                        .strong()
                        .small()
                        .color(Color32::from_rgb(148, 163, 184)),
                );
                ui.add_space(8.0);
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
                            .color(Color32::from_rgb(130, 148, 178)),
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
                    .strong()
                    .small()
                    .color(Color32::from_rgb(125, 211, 252)),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Notes saved here (or by AI agents via `remember`) are automatically attached whenever any AI reads that symbol or opens `repo_map`.",
                )
                .small()
                .color(Color32::from_rgb(148, 163, 184)),
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
                let save_btn = egui::Button::new(RichText::new("Save").strong().color(Color32::WHITE))
                    .corner_radius(CornerRadius::same(14))
                    .fill(Color32::from_rgb(37, 99, 235));
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
                            .color(Color32::from_rgb(130, 148, 178)),
                    );
                });
            } else {
                for note in &lp.notes {
                    soft_card_frame(14).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            egui::Frame::NONE
                                .fill(Color32::from_rgba_unmultiplied(56, 189, 248, 28))
                                .corner_radius(CornerRadius::same(10))
                                .inner_margin(Margin::symmetric(8, 3))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(&note.target)
                                            .monospace()
                                            .small()
                                            .strong()
                                            .color(Color32::from_rgb(125, 211, 252)),
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
                                                .corner_radius(CornerRadius::same(12)),
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
        Box::new(move |_cc| Ok(Box::new(TrackerTopApp::new(initial_repo)))),
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
        islands: Vec::new(),
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

    let mut group_list: Vec<(String, Vec<usize>)> = groups.into_iter().collect();
    group_list.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));

    struct LocalIsland {
        title: String,
        indices: Vec<usize>,
        local_pos: Vec<(f32, f32)>,
        radius: f32,
        color_idx: usize,
    }

    let mut local_islands = Vec::with_capacity(group_list.len());
    for (g_idx, (title, mut indices)) in group_list.into_iter().enumerate() {
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
                        let angle =
                            angle_offset + (k as f32 / count as f32) * std::f32::consts::TAU;
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

/// Generates a silky-smooth, zero-banding 256x256 GPU-filtered radial aurora gradient texture
/// using true exponential Gaussian falloff + subtle blue-noise-like dithering.
fn build_smooth_aurora_texture(ctx: &egui::Context) -> egui::TextureHandle {
    const W: usize = 256;
    const H: usize = 256;
    let mut pixels = vec![Color32::TRANSPARENT; W * H];

    let base = (7.0_f32, 11.0_f32, 20.0_f32);
    // (center, radius, rgb, strength)
    type SoftLight = ((f32, f32), f32, (f32, f32, f32), f32);
    let lights: [SoftLight; 3] = [
        ((0.16, 0.14), 0.42, (56.0, 189.0, 248.0), 0.16), // soft cyan top-left
        ((0.86, 0.84), 0.48, (129.0, 140.0, 248.0), 0.15), // soft indigo bottom-right
        ((0.50, 0.92), 0.38, (16.0, 185.0, 129.0), 0.10), // whisper emerald bottom
    ];

    for y in 0..H {
        let ny = y as f32 / (H - 1) as f32;
        for x in 0..W {
            let nx = x as f32 / (W - 1) as f32;
            let mut r = base.0;
            let mut g = base.1;
            let mut b = base.2;

            for &((lx, ly), sigma, (cr, cg, cb), strength) in &lights {
                let dx = nx - lx;
                let dy = ny - ly;
                let dist_sq = dx * dx + dy * dy;
                let falloff = (-dist_sq / (2.0 * sigma * sigma)).exp() * strength;
                r += (cr - base.0) * falloff;
                g += (cg - base.1) * falloff;
                b += (cb - base.2) * falloff;
            }

            // Tiny deterministic sub-pixel dither (+/- 0.4) to completely eliminate 8-bit banding
            let hash = ((x.wrapping_mul(374761393) ^ y.wrapping_mul(668265263)) & 255) as f32
                / 255.0
                - 0.5;
            let rr = (r + hash * 0.8).clamp(0.0, 255.0) as u8;
            let gg = (g + hash * 0.8).clamp(0.0, 255.0) as u8;
            let bb = (b + hash * 0.8).clamp(0.0, 255.0) as u8;
            pixels[y * W + x] = Color32::from_rgb(rr, gg, bb);
        }
    }

    let image = egui::ColorImage {
        size: [W, H],
        pixels,
    };
    ctx.load_texture("trackertop_aurora_bg", image, egui::TextureOptions::LINEAR)
}

fn paint_smooth_aurora(painter: &egui::Painter, rect: Rect, tex_id: egui::TextureId) {
    painter.image(
        tex_id,
        rect,
        Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
        Color32::WHITE,
    );
}

fn apply_soft_modern_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(10.0, 8.0);
    style.spacing.button_padding = Vec2::new(13.0, 6.0);

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(9, 13, 24);
    visuals.window_fill = Color32::from_rgb(15, 22, 38);
    visuals.extreme_bg_color = Color32::from_rgb(11, 17, 30);
    visuals.window_corner_radius = CornerRadius::same(16);
    visuals.widgets.noninteractive.corner_radius = CornerRadius::same(12);
    visuals.widgets.inactive.corner_radius = CornerRadius::same(12);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(12);
    visuals.widgets.active.corner_radius = CornerRadius::same(12);
    visuals.widgets.inactive.bg_fill = Color32::from_rgba_unmultiplied(30, 41, 59, 185);
    visuals.widgets.hovered.bg_fill = Color32::from_rgba_unmultiplied(51, 65, 85, 215);
    visuals.widgets.active.bg_fill = Color32::from_rgba_unmultiplied(56, 189, 248, 80);
    visuals.widgets.inactive.bg_stroke =
        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(148, 163, 184, 34));
    visuals.widgets.hovered.bg_stroke =
        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(56, 189, 248, 120));

    style.visuals = visuals;
    ctx.set_style(style);
}

fn soft_card_frame(radius: u8) -> egui::Frame {
    egui::Frame::NONE
        .fill(Color32::from_rgba_unmultiplied(17, 24, 41, 220))
        .stroke(Stroke::new(
            1.0_f32,
            Color32::from_rgba_unmultiplied(148, 163, 184, 30),
        ))
        .corner_radius(CornerRadius::same(radius))
        .inner_margin(Margin::same(15))
}

fn glow_kpi_card(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    sub: &str,
    text_col: Color32,
    accent: Color32,
) {
    egui::Frame::NONE
        .fill(Color32::from_rgba_unmultiplied(17, 24, 41, 228))
        .stroke(Stroke::new(
            1.0_f32,
            Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 68),
        ))
        .corner_radius(CornerRadius::same(16))
        .inner_margin(Margin::same(15))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(label)
                    .small()
                    .strong()
                    .color(Color32::from_rgb(130, 148, 178)),
            );
            ui.add_space(4.0);
            ui.label(RichText::new(value).strong().size(24.0).color(text_col));
            ui.add_space(2.0);
            ui.label(
                RichText::new(sub)
                    .small()
                    .color(Color32::from_rgb(148, 163, 184)),
            );
        });
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
