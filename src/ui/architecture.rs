//! The Architecture tab: code health, language breakdown and communities.

use super::graph_tab::comm_color;
use super::*;

impl TrackerTopApp {
    pub(super) fn render_architecture_tab(&mut self, ui: &mut egui::Ui) {
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
                    &format!(
                        "OVERSIZED FILES (>{} TOK)",
                        fmt_num(lp.health.bloat_threshold)
                    ),
                    &lp.health.bloated_files.len().to_string(),
                    "Size outliers among this repo's files",
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
                    "Through code; Rust module cycles allowed",
                    cyc_col,
                    cyc_col,
                );
                glow_kpi_card(
                    &mut cols[2],
                    "ORPHAN CODE FILES",
                    &lp.health.orphan_files.len().to_string(),
                    &format!(
                        "No references; {} entry points/scripts excluded",
                        lp.health.orphan_excluded.len()
                    ),
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
                                "Oversized: {p} — {} tokens ({lines} lines) — consider splitting",
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
