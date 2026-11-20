//! The Graph tab: the nested code graph scene, its builders and the node inspector.

use super::*;

impl TrackerTopApp {
    pub(super) fn render_graph_tab(&mut self, ui: &mut egui::Ui) {
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

pub(super) fn comm_color(lp: &LoadedProject, cid: Option<&str>) -> Color32 {
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
