//! Bottom status bar: the selected project's index state on the left, the AI agent's
//! latest TrackerTop call on the right. Every value comes from the index database or the
//! running index job; an empty slot stays empty.

use super::theme as T;
use super::{ProjectTab, Selection, TrackerTopApp, fmt_num};
use crate::db::Database;
use crate::indexer::IndexStage;
use crate::journal::{call_headline, fmt_age, secs_since};
use crate::model::JournalEntry;
use eframe::egui::{self, Color32, Margin, RichText, Sense, Stroke, text::LayoutJob};
use std::path::Path;
use std::time::{Duration, Instant};

/// How long an action's message (note saved, install result, …) replaces the index state.
const NOTICE_SECS: u64 = 6;
const SIZE: f32 = 11.5;

/// What the bar shows for one project, re-read with the app's regular refresh.
#[derive(Default)]
pub(super) struct StatusInfo {
    path: String,
    /// Unix seconds of the last finished index run (`None`: an index written before this was recorded).
    indexed_at: Option<i64>,
    last_call: Option<JournalEntry>,
}

impl StatusInfo {
    pub(super) fn load(repo: &str) -> Self {
        let db_path = Path::new(repo).join(".trackertop").join("index.db");
        let (indexed_at, last_call) = if db_path.is_file() {
            Database::open_path(&db_path)
                .map(|db| {
                    (
                        db.indexed_at().ok().flatten(),
                        db.latest_agent_call().ok().flatten(),
                    )
                })
                .unwrap_or_default()
        } else {
            (None, None)
        };
        Self {
            path: repo.to_string(),
            indexed_at,
            last_call,
        }
    }
}

/// A message from a user action, shown for [`NOTICE_SECS`].
pub(super) struct Notice {
    text: String,
    error: bool,
    at: Instant,
}

impl TrackerTopApp {
    pub(super) fn notify(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            error: false,
            at: Instant::now(),
        });
    }

    pub(super) fn notify_error(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            error: true,
            at: Instant::now(),
        });
    }

    /// Re-reads the bar's data for the selected project.
    pub(super) fn refresh_status(&mut self) {
        self.status = match &self.selection {
            Selection::Project(p) => StatusInfo::load(p),
            Selection::AllProjects => StatusInfo::default(),
        };
    }

    pub(super) fn render_status_bar(&mut self, ctx: &egui::Context) {
        let mut open_journal = false;
        egui::TopBottomPanel::bottom("status_bar")
            .frame(
                egui::Frame::NONE
                    .fill(T::PANEL)
                    .inner_margin(Margin::symmetric(16, 5))
                    .stroke(Stroke::new(1.0_f32, T::BORDER)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    self.render_index_state(ui);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        open_journal = self.render_latest_call(ui);
                    });
                });
            });
        if open_journal {
            self.active_tab = ProjectTab::Memory;
        }
    }

    fn selected_path(&self) -> Option<&str> {
        match &self.selection {
            Selection::Project(p) => Some(p),
            Selection::AllProjects => None,
        }
    }

    fn render_index_state(&mut self, ui: &mut egui::Ui) {
        if let Some(n) = &self.notice {
            if n.at.elapsed() < Duration::from_secs(NOTICE_SECS) {
                let color = if n.error { T::DANGER } else { T::TEXT_2 };
                ui.label(RichText::new(&n.text).size(SIZE).color(color));
                return;
            }
            self.notice = None;
        }

        if let (true, Some(progress)) = (self.bg_busy, self.index_progress.as_ref()) {
            ui.add(egui::Spinner::new().size(12.0).color(T::ACCENT));
            let other = self
                .indexing_path
                .as_deref()
                .filter(|p| {
                    !self
                        .selected_path()
                        .is_some_and(|s| s.eq_ignore_ascii_case(p))
                })
                .map(project_name);
            let what = match other {
                Some(name) => format!("Indexing {name}…"),
                None => "Indexing…".to_string(),
            };
            let step = match progress.stage() {
                IndexStage::Scanning => "scanning folders".to_string(),
                IndexStage::Parsing { done, total } => {
                    format!("{}/{} files", fmt_num(done), fmt_num(total))
                }
                IndexStage::Linking => "linking symbols".to_string(),
            };
            ui.label(RichText::new(what).size(SIZE).color(T::TEXT_2));
            ui.label(RichText::new(step).size(SIZE).color(T::TEXT_3));
            return;
        }

        let Some(sel) = self.selected_path().map(str::to_string) else {
            return;
        };

        if let Some((path, err)) = &self.index_error
            && path.eq_ignore_ascii_case(&sel)
        {
            let first = err.lines().next().unwrap_or("").trim();
            ui.label(
                RichText::new(format!("Index failed: {}", clip(first, 90)))
                    .size(SIZE)
                    .color(T::DANGER),
            )
            .on_hover_text(err.as_str());
            return;
        }

        let Some(snap) = self
            .overview
            .projects
            .iter()
            .find(|p| p.entry.path.eq_ignore_ascii_case(&sel))
        else {
            return;
        };
        if !snap.exists_on_disk {
            ui.label(RichText::new("Folder not found").size(SIZE).color(T::WARN));
            return;
        }
        if !snap.indexed {
            ui.label(RichText::new("Not indexed").size(SIZE).color(T::TEXT_3));
            return;
        }

        let counts = format!(
            "{} files · {} symbols",
            fmt_num(snap.total_files),
            fmt_num(snap.total_symbols)
        );
        let at = (self.status.path.eq_ignore_ascii_case(&sel))
            .then_some(self.status.indexed_at)
            .flatten();
        match at {
            Some(at) => {
                let now = chrono::Utc::now().timestamp();
                let mut job = LayoutJob::default();
                push(&mut job, "Indexed ", T::TEXT_3, false);
                push(&mut job, &fmt_age((now - at).max(0)), T::TEXT_2, false);
                push(&mut job, &format!("  ·  {counts}"), T::TEXT_3, false);
                ui.label(job).on_hover_text(format!(
                    "Last index run finished {}",
                    chrono::DateTime::from_timestamp(at, 0)
                        .map(|t| {
                            t.with_timezone(&chrono::Local)
                                .format("%Y-%m-%d %H:%M:%S")
                                .to_string()
                        })
                        .unwrap_or_default()
                ));
            }
            None => {
                ui.label(RichText::new(counts).size(SIZE).color(T::TEXT_3));
            }
        };
    }

    /// Draws the latest agent call; returns `true` when it was clicked.
    fn render_latest_call(&self, ui: &mut egui::Ui) -> bool {
        let Some(sel) = self.selected_path() else {
            return false;
        };
        if !self.status.path.eq_ignore_ascii_case(sel) {
            return false;
        }
        let Some(call) = &self.status.last_call else {
            return false;
        };
        let (agent, tool, target) = call_headline(call);
        let age = secs_since(&call.timestamp, chrono::Utc::now().timestamp()).map(fmt_age);

        let mut job = LayoutJob::default();
        push(&mut job, &agent, T::TEXT_3, false);
        push(&mut job, "  ·  ", T::TEXT_3, false);
        let tool_color = if call.is_error() {
            T::DANGER
        } else {
            T::TEXT_2
        };
        push(&mut job, &tool, tool_color, false);
        if let Some(t) = target {
            push(&mut job, " ", T::TEXT_3, false);
            push(&mut job, &clip(&t, 48), T::ACCENT, true);
        }
        if let Some(age) = age {
            push(&mut job, &format!("  ·  {age}"), T::TEXT_3, false);
        }
        let resp = ui
            .add(egui::Label::new(job).sense(Sense::click()))
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let mut hover = format!("Latest AI agent call: {tool}");
        if !call.query.is_empty() {
            hover.push_str(&format!("\n{}", call.query));
        }
        hover.push_str(&format!("\nResult: {}", call.outcome));
        hover.push_str("\n\nClick to open the work journal (AI Memory).");
        resp.on_hover_text(hover).clicked()
    }
}

fn push(job: &mut LayoutJob, text: &str, color: Color32, mono: bool) {
    let font = if mono {
        T::mono_font(SIZE)
    } else {
        T::ui_font(SIZE)
    };
    job.append(text, 0.0, egui::TextFormat::simple(font, color));
}

fn project_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
