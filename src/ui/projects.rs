//! Project list views: the agents modal, the all-projects overview and the single-project shell with its tab bar.

use super::*;

impl TrackerTopApp {
    pub(super) fn render_agents_modal(&mut self, ctx: &egui::Context) {
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

    pub(super) fn render_all_projects(&mut self, ui: &mut egui::Ui) {
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

    pub(super) fn render_single_project(&mut self, ui: &mut egui::Ui) {
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
}
