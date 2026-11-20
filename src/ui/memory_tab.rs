//! The Memory tab: notes, decisions and dismissed review leads.

use super::*;

impl TrackerTopApp {
    pub(super) fn render_memory_tab(&mut self, ui: &mut egui::Ui) {
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
            super::journal_view::render_timeline(ui, Path::new(&repo_path));
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
