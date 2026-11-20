//! The Savings tab: token-savings KPIs, charts and the query log.

use super::*;

impl TrackerTopApp {
    pub(super) fn render_savings_tab(&mut self, ui: &mut egui::Ui) {
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
}
