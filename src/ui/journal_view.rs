//! AI Memory tab: the work journal as a readable timeline ("what the agent did and why").
//! Kept in its own file so the tab itself changes by a single call.

use crate::db::Database;
use crate::journal::{TimelineItem, fmt_ts, timeline};
use eframe::egui::{self, Color32, CornerRadius, Margin, RichText};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Rows shown; older ones stay in the DB and in `resume`.
const SHOWN: usize = 60;

type Cached = (PathBuf, Instant, Vec<TimelineItem>, usize);

/// The timeline is re-read at most every two seconds (the app repaints far more often).
static CACHE: Mutex<Option<Cached>> = Mutex::new(None);

fn load(repo: &Path) -> (Vec<TimelineItem>, usize) {
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((p, at, items, total)) = cache.as_ref()
        && p == repo
        && at.elapsed() < Duration::from_secs(2)
    {
        return (items.clone(), *total);
    }
    let db_path = repo.join(".trackertop").join("index.db");
    let (items, total) = if db_path.is_file() {
        Database::open_path(&db_path)
            .and_then(|db| {
                let (rows, total) = db.recent_journal(1_000)?;
                let notes = db.list_notes(None)?;
                let all = timeline(&rows, &notes, usize::MAX);
                // Hidden: timeline rows past the cap, plus calls older than the window read
                let hidden = all.len().saturating_sub(SHOWN) + total.saturating_sub(rows.len());
                Ok((all.into_iter().take(SHOWN).collect(), hidden))
            })
            .unwrap_or_default()
    } else {
        (Vec::new(), 0)
    };
    *cache = Some((repo.to_path_buf(), Instant::now(), items.clone(), total));
    (items, total)
}

fn badge(kind: &str) -> (&'static str, Color32) {
    match kind {
        "error" => ("ERROR", Color32::from_rgb(248, 113, 113)),
        "check" => ("CHECK", Color32::from_rgb(52, 211, 153)),
        "progress" => ("PROGRESS", Color32::from_rgb(74, 222, 128)),
        "tried" => ("TRIED", Color32::from_rgb(251, 191, 36)),
        "blocked" => ("BLOCKED", Color32::from_rgb(248, 113, 113)),
        "decision" => ("DECISION", Color32::from_rgb(192, 132, 252)),
        "dismissed" => ("DISMISSED", Color32::from_rgb(148, 163, 184)),
        "note" => ("NOTE", Color32::from_rgb(125, 211, 252)),
        _ => ("CALLS", Color32::from_rgb(100, 116, 139)),
    }
}

/// The collapsible timeline card shown above the notes list.
pub(super) fn render_timeline(ui: &mut egui::Ui, repo: &Path) {
    let (items, hidden) = load(repo);
    super::soft_card_frame(16).show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::CollapsingHeader::new(
            RichText::new("WORK JOURNAL: WHAT THE AGENT DID AND WHY")
                .strong()
                .small()
                .color(Color32::from_rgb(125, 211, 252)),
        )
        .default_open(true)
        .show(ui, |ui| {
            if items.is_empty() {
                ui.label(
                    RichText::new(
                        "Nothing yet. Agent tool calls over MCP and remember(kind=progress|tried|blocked) entries appear here.",
                    )
                    .small()
                    .color(Color32::from_rgb(130, 148, 178)),
                );
                return;
            }
            for item in &items {
                let (label, color) = badge(&item.kind);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(fmt_ts(&item.timestamp))
                            .monospace()
                            .small()
                            .color(Color32::from_rgb(130, 148, 178)),
                    );
                    egui::Frame::NONE
                        .fill(color.gamma_multiply(0.18))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::symmetric(6, 1))
                        .show(ui, |ui| {
                            ui.label(RichText::new(label).small().strong().color(color));
                        });
                    ui.label(RichText::new(&item.title).color(Color32::WHITE));
                });
                if !item.detail.is_empty() {
                    let prefix = if matches!(item.kind.as_str(), "calls" | "check" | "error") {
                        ""
                    } else {
                        "why: "
                    };
                    ui.label(
                        RichText::new(format!("    {prefix}{}", item.detail))
                            .small()
                            .color(Color32::from_rgb(148, 163, 184)),
                    );
                }
                ui.add_space(3.0);
            }
            if hidden > 0 {
                ui.label(
                    RichText::new(format!(
                        "+{hidden} older entries not shown (the `resume` tool summarizes recent sessions)"
                    ))
                    .small()
                    .color(Color32::from_rgb(130, 148, 178)),
                );
            }
        });
    });
    ui.add_space(12.0);
}
