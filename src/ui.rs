#![cfg(feature = "gui")]

use crate::db::Database;
use crate::graph::{CodeGraph, CommunitySummary, ScoredNode};
use crate::indexer::index_repository;
use crate::install::{install_claude_code, is_claude_global_installed};
use crate::model::{EdgeKind, SymbolKind, TokenStatsSummary};
use crate::parser::default_registry;
use crate::registry::{GlobalOverview, load_global_overview, register_project, remove_project};
use crate::report::generate_reports;
use anyhow::Result;
use eframe::egui::{self, Color32, CornerRadius, Pos2, RichText, Sense, Stroke, StrokeKind, Vec2};
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphColorMode {
    Language,
    Cluster,
}

struct GraphNodeVis {
    id: String,
    label: String,
    language: String,
    community: usize,
    tokens: usize,
    lines: usize,
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    radius: f32,
    symbols: Vec<(String, String, usize, String)>, // (name, kind, line, signature)
}

struct GraphEdgeVis {
    src: usize,
    tgt: usize,
}

struct LoadedProject {
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

    // Graph canvas interaction state
    cam_offset: Vec2,
    cam_zoom: f32,
    graph_search: String,
    graph_lang_filter: String,
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
            graph_search: String::new(),
            graph_lang_filter: "all".to_string(),
            graph_color_mode: GraphColorMode::Language,
            selected_node_idx: None,
            dragged_node_idx: None,
        };
        app.reload_selected_project(true);
        app
    }

    fn reload_overview(&mut self) {
        self.overview = load_global_overview();
        self.claude_connected = is_claude_global_installed();
    }

    fn reload_selected_project(&mut self, reset_graph_positions: bool) {
        let Selection::Project(ref path) = self.selection else {
            self.loaded = None;
            return;
        };

        let old_positions: HashMap<String, (f32, f32)> = if !reset_graph_positions {
            self.loaded
                .as_ref()
                .map(|lp| lp.nodes.iter().map(|n| (n.id.clone(), (n.x, n.y))).collect())
                .unwrap_or_default()
        } else {
            self.selected_node_idx = None;
            self.cam_offset = Vec2::ZERO;
            self.cam_zoom = 1.0;
            HashMap::new()
        };

        self.loaded = load_project_details(Path::new(path), &old_positions).ok();
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
                self.reload_selected_project(true);
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
                // Lightweight refresh of telemetry without disturbing graph layout
                if let Some(ref mut lp) = self.loaded {
                    if let Ok(db) = Database::open_path(&Path::new(p).join(".trackertop").join("index.db")) {
                        if let Ok(stats) = db.get_token_stats(30) {
                            lp.stats = stats;
                        }
                    }
                } else {
                    self.reload_selected_project(true);
                }
            }
        }
        ctx.request_repaint_after(Duration::from_millis(if self.active_tab == ProjectTab::Graph {
            33
        } else {
            500
        }));

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
                        // Claude Code global connection button/status
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
                                Selection::AllProjects => std::env::current_dir()
                                    .unwrap_or_else(|_| PathBuf::from(".")),
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
                            if ui.button("Export HTML").clicked() {
                                match generate_reports(Path::new(p)) {
                                    Ok(paths) => {
                                        self.status_banner = format!(
                                            "Exported {}",
                                            paths.html_path.display()
                                        );
                                        open_in_os(&paths.html_path);
                                    }
                                    Err(e) => {
                                        self.status_banner = format!("Report error: {e}");
                                    }
                                }
                            }
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

                // All Projects global button
                let all_selected = self.selection == Selection::AllProjects;
                let all_btn = egui::Button::new(
                    RichText::new(format!(
                        "All Projects ({})",
                        self.overview.projects.len()
                    ))
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

                            let subtitle = format!(
                                "{} files · saved {} tok ({:.0}%)",
                                snap.total_files,
                                fmt_num(snap.stats.total_tokens_saved),
                                snap.stats.savings_percent
                            );
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
                    self.reload_selected_project(true);
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
                &format!("{} files · {} symbols", fmt_num(ov.total_files), fmt_num(ov.total_symbols)),
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
                        ui.label(RichText::new(h).strong().color(Color32::from_rgb(148, 163, 184)));
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
            self.reload_selected_project(true);
        }
    }

    fn render_single_project(&mut self, ui: &mut egui::Ui) {
        // Tab switcher
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
                &format!("Repo size: {} tok", fmt_num(lp.total_repo_tokens)),
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
            // Savings by tool
            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("SAVINGS BY MCP TOOL").strong().small().color(Color32::from_rgb(148, 163, 184)));
                ui.add_space(6.0);
                if s.by_tool.is_empty() {
                    ui.label(
                        RichText::new("No AI queries recorded for this project yet. Connect Claude Code or run `trackertop query map`.")
                            .color(Color32::from_rgb(139, 155, 184)),
                    );
                } else {
                    for t in &s.by_tool {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{} ({}x)", t.tool, t.queries)).monospace().strong());
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
                ui.label(RichText::new("LIVE AI QUERY LOG").strong().small().color(Color32::from_rgb(148, 163, 184)));
                ui.add_space(6.0);
                if s.recent.is_empty() {
                    ui.label(RichText::new("Waiting for MCP / CLI queries...").color(Color32::from_rgb(139, 155, 184)));
                } else {
                    egui::Grid::new("recent_queries_grid")
                        .striped(true)
                        .spacing([14.0, 6.0])
                        .show(ui, |ui| {
                            for h in ["Tool", "Query", "Sent", "Baseline", "Saved", "Latency"] {
                                ui.label(RichText::new(h).strong().small().color(Color32::from_rgb(148, 163, 184)));
                            }
                            ui.end_row();
                            for r in &s.recent {
                                let pct = if r.baseline_tokens > 0 {
                                    (r.tokens_saved as f64 / r.baseline_tokens as f64) * 100.0
                                } else {
                                    0.0
                                };
                                ui.label(RichText::new(&r.tool).monospace().color(Color32::from_rgb(56, 189, 248)));
                                ui.label(truncate_label(&r.query, 40));
                                ui.label(fmt_num(r.tokens_sent));
                                ui.label(fmt_num(r.baseline_tokens));
                                ui.label(
                                    RichText::new(format!("{} ({:.0}%)", fmt_num(r.tokens_saved), pct))
                                        .color(Color32::from_rgb(52, 211, 153)),
                                );
                                ui.label(format!("{} ms", r.latency_ms));
                                ui.end_row();
                            }
                        });
                }
            });
        });
    }

    fn render_graph_tab(&mut self, ui: &mut egui::Ui) {
        // Toolbar
        ui.horizontal(|ui| {
            ui.label("Search:");
            ui.add(
                egui::TextEdit::singleline(&mut self.graph_search)
                    .desired_width(170.0)
                    .hint_text("file or symbol..."),
            );

            egui::ComboBox::from_id_salt("lang_filter")
                .selected_text(if self.graph_lang_filter == "all" {
                    "All Languages".to_string()
                } else {
                    default_registry().display_name_for(&self.graph_lang_filter)
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.graph_lang_filter, "all".to_string(), "All Languages");
                    for l in default_registry().languages() {
                        ui.selectable_value(&mut self.graph_lang_filter, l.id, l.display_name);
                    }
                });

            egui::ComboBox::from_id_salt("color_mode")
                .selected_text(match self.graph_color_mode {
                    GraphColorMode::Language => "Color: Language",
                    GraphColorMode::Cluster => "Color: Cluster",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.graph_color_mode, GraphColorMode::Language, "Color: Language");
                    ui.selectable_value(&mut self.graph_color_mode, GraphColorMode::Cluster, "Color: Cluster");
                });

            if ui.button("Reset View").clicked() {
                self.cam_offset = Vec2::ZERO;
                self.cam_zoom = 1.0;
            }
        });
        ui.add_space(4.0);

        let avail = ui.available_size();
        let inspector_w = if self.selected_node_idx.is_some() { 290.0 } else { 0.0 };
        let canvas_w = (avail.x - inspector_w - 8.0).max(240.0);

        ui.horizontal_top(|ui| {
            // Left: GPU Force-Directed Canvas
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(canvas_w, avail.y - 6.0), Sense::click_and_drag());
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
                // Step force-directed physics gently
                step_graph_physics(&mut lp.nodes, &lp.edges, self.dragged_node_idx);

                let to_screen = |wx: f32, wy: f32, cam_off: Vec2, zoom: f32| -> Pos2 {
                    Pos2::new(
                        center.x + (wx + cam_off.x) * zoom,
                        center.y + (wy + cam_off.y) * zoom,
                    )
                };

                // Zoom with scroll wheel
                if resp.hovered() {
                    let scroll = ui.input(|i| i.raw_scroll_delta.y);
                    if scroll.abs() > 0.1 {
                        let factor = if scroll > 0.0 { 1.10 } else { 0.91 };
                        self.cam_zoom = (self.cam_zoom * factor).clamp(0.25, 3.5);
                    }
                }

                // Determine visible set
                let q = self.graph_search.trim().to_ascii_lowercase();
                let visible: Vec<bool> = lp
                    .nodes
                    .iter()
                    .map(|n| {
                        if self.graph_lang_filter != "all" && n.language != self.graph_lang_filter {
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
                    .collect();

                // Hit-test pointer
                let pointer_pos = resp.interact_pointer_pos();
                let hit_node = pointer_pos.and_then(|pos| {
                    lp.nodes.iter().enumerate().rev().find_map(|(idx, n)| {
                        if !visible[idx] {
                            return None;
                        }
                        let sp = to_screen(n.x, n.y, self.cam_offset, self.cam_zoom);
                        let r = (n.radius * self.cam_zoom + 5.0).max(8.0);
                        if sp.distance(pos) <= r {
                            Some(idx)
                        } else {
                            None
                        }
                    })
                });

                if resp.drag_started() {
                    self.dragged_node_idx = hit_node;
                    if hit_node.is_some() {
                        self.selected_node_idx = hit_node;
                    }
                }
                if resp.dragged() {
                    let delta = resp.drag_delta() / self.cam_zoom;
                    if let Some(idx) = self.dragged_node_idx {
                        if let Some(n) = lp.nodes.get_mut(idx) {
                            n.x += delta.x;
                            n.y += delta.y;
                            n.vx = 0.0;
                            n.vy = 0.0;
                        }
                    } else {
                        self.cam_offset += delta;
                    }
                }
                if resp.drag_stopped() {
                    self.dragged_node_idx = None;
                }
                if resp.clicked() {
                    self.selected_node_idx = hit_node;
                }

                // Draw edges
                for e in &lp.edges {
                    if !visible[e.src] || !visible[e.tgt] {
                        continue;
                    }
                    let a = &lp.nodes[e.src];
                    let b = &lp.nodes[e.tgt];
                    let hl = self.selected_node_idx == Some(e.src)
                        || self.selected_node_idx == Some(e.tgt);
                    let stroke = if hl {
                        Stroke::new(1.8_f32, Color32::from_rgba_unmultiplied(56, 189, 248, 210))
                    } else {
                        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(148, 163, 184, 55))
                    };
                    painter.line_segment(
                        [
                            to_screen(a.x, a.y, self.cam_offset, self.cam_zoom),
                            to_screen(b.x, b.y, self.cam_offset, self.cam_zoom),
                        ],
                        stroke,
                    );
                }

                // Draw nodes & labels
                for (idx, n) in lp.nodes.iter().enumerate() {
                    if !visible[idx] {
                        continue;
                    }
                    let sp = to_screen(n.x, n.y, self.cam_offset, self.cam_zoom);
                    if !rect.expand(40.0).contains(sp) {
                        continue;
                    }
                    let color = match self.graph_color_mode {
                        GraphColorMode::Language => parse_hex_color(reg.color_for(&n.language)),
                        GraphColorMode::Cluster => cluster_color(n.community),
                    };
                    let r = (n.radius * self.cam_zoom).clamp(5.0, 32.0);
                    painter.circle_filled(sp, r, color);
                    if self.selected_node_idx == Some(idx) {
                        painter.circle_stroke(sp, r + 2.0, Stroke::new(2.2_f32, Color32::WHITE));
                    }
                    painter.text(
                        Pos2::new(sp.x, sp.y + r + 10.0),
                        egui::Align2::CENTER_CENTER,
                        &n.label,
                        egui::FontId::proportional(11.0),
                        Color32::from_rgb(230, 237, 247),
                    );
                }
            }

            // Right: Symbol Inspector when a node is selected
            if let (Some(idx), Some(lp)) = (self.selected_node_idx, &self.loaded) {
                if let Some(node) = lp.nodes.get(idx) {
                    ui.allocate_ui(Vec2::new(inspector_w, avail.y - 6.0), |ui| {
                        card_frame().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&node.label).strong().size(14.0));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.small_button("x").clicked() {
                                        self.selected_node_idx = None;
                                    }
                                });
                            });
                            ui.label(
                                RichText::new(&node.id)
                                    .small()
                                    .color(Color32::from_rgb(139, 155, 184)),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "{} · {} lines · {} tok",
                                    reg.display_name_for(&node.language),
                                    node.lines,
                                    fmt_num(node.tokens)
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

            // Language breakdown
            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("LANGUAGE BREAKDOWN").strong().small().color(Color32::from_rgb(148, 163, 184)));
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

            // God nodes
            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("ARCHITECTURAL HUBS (GOD NODES)").strong().small().color(Color32::from_rgb(148, 163, 184)));
                ui.add_space(6.0);
                egui::Grid::new("god_nodes_grid")
                    .striped(true)
                    .spacing([14.0, 6.0])
                    .show(ui, |ui| {
                        for h in ["Symbol", "Kind", "File", "In", "Out", "Score"] {
                            ui.label(RichText::new(h).strong().small().color(Color32::from_rgb(148, 163, 184)));
                        }
                        ui.end_row();
                        for g in &lp.god_nodes {
                            ui.label(RichText::new(&g.name).monospace().strong());
                            ui.label(RichText::new(g.kind.as_str()).color(Color32::from_rgb(56, 189, 248)));
                            ui.label(&g.file_path);
                            ui.label(g.in_degree.to_string());
                            ui.label(g.out_degree.to_string());
                            ui.label(format!("{:.2}", g.score));
                            ui.end_row();
                        }
                    });
            });

            ui.add_space(10.0);

            // Communities
            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("MODULE CLUSTERS").strong().small().color(Color32::from_rgb(148, 163, 184)));
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
            .with_inner_size([1240.0, 780.0])
            .with_min_inner_size([860.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "TrackerTop",
        options,
        Box::new(move |_cc| Ok(Box::new(TrackerTopApp::new(initial_repo)))),
    )
    .map_err(|e| anyhow::anyhow!("GUI error: {e}"))
}

fn load_project_details(
    repo_root: &Path,
    old_positions: &HashMap<String, (f32, f32)>,
) -> Result<LoadedProject> {
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
    let n_files = sorted_files.len().max(1);

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
        let (x, y) = if let Some(&(ox, oy)) = old_positions.get(&f.path) {
            (ox, oy)
        } else {
            let angle = (i as f32 / n_files as f32) * std::f32::consts::TAU;
            let rad = 140.0 + ((i % 4) as f32) * 32.0;
            (angle.cos() * rad, angle.sin() * rad)
        };

        let radius = (8.0 + (f.token_count as f32).sqrt() * 0.13 + (rank as f32) * 32.0).clamp(8.0, 24.0);
        nodes.push(GraphNodeVis {
            id: f.path.clone(),
            label: f.path.rsplit('/').next().unwrap_or(&f.path).to_string(),
            language: f.language.clone(),
            community: *cg.file_communities.get(&f.path).unwrap_or(&0),
            tokens: f.token_count,
            lines: f.line_count,
            x,
            y,
            vx: 0.0,
            vy: 0.0,
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
        let (Some(s), Some(t)) = (cg.symbols.get(&e.source_id), cg.symbols.get(&e.target_id)) else {
            continue;
        };
        if s.file_path == t.file_path {
            continue;
        }
        if let (Some(&si), Some(&ti)) = (idx_by_path.get(&s.file_path), idx_by_path.get(&t.file_path))
        {
            if edge_seen.insert((si, ti)) {
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
    })
}

fn step_graph_physics(nodes: &mut [GraphNodeVis], edges: &[GraphEdgeVis], dragged: Option<usize>) {
    let n = nodes.len();
    for i in 0..n {
        nodes[i].vx += -nodes[i].x * 0.002;
        nodes[i].vy += -nodes[i].y * 0.002;
        for j in (i + 1)..n {
            let dx = nodes[j].x - nodes[i].x;
            let dy = nodes[j].y - nodes[i].y;
            let dist_sq = dx * dx + dy * dy + 30.0;
            let dist = dist_sq.sqrt();
            let f = 2200.0 / dist_sq;
            let fx = (dx / dist) * f;
            let fy = (dy / dist) * f;
            nodes[i].vx -= fx;
            nodes[i].vy -= fy;
            nodes[j].vx += fx;
            nodes[j].vy += fy;
        }
    }
    for e in edges {
        let dx = nodes[e.tgt].x - nodes[e.src].x;
        let dy = nodes[e.tgt].y - nodes[e.src].y;
        let dist = (dx * dx + dy * dy).sqrt().max(1.0);
        let f = (dist - 115.0) * 0.014;
        let fx = (dx / dist) * f;
        let fy = (dy / dist) * f;
        nodes[e.src].vx += fx;
        nodes[e.src].vy += fy;
        nodes[e.tgt].vx -= fx;
        nodes[e.tgt].vy -= fy;
    }
    for (i, node) in nodes.iter_mut().enumerate() {
        if dragged == Some(i) {
            continue;
        }
        node.vx *= 0.85;
        node.vy *= 0.85;
        node.x += node.vx.clamp(-10.0, 10.0);
        node.y += node.vy.clamp(-10.0, 10.0);
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
        ui.label(RichText::new(label).small().color(Color32::from_rgb(139, 155, 184)));
        ui.add_space(2.0);
        ui.label(RichText::new(value).strong().size(22.0).color(color));
        ui.label(RichText::new(sub).small().color(Color32::from_rgb(139, 155, 184)));
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

fn open_in_os(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", &path.to_string_lossy()])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}
