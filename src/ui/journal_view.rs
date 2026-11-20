//! AI Memory tab: the work journal as a readable timeline ("what the agent did and why").
//! Kept in its own file so the tab itself changes by a single call.

use super::theme as T;
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
        "error" => ("ERROR", T::DANGER),
        "check" => ("CHECK", T::SUCCESS),
        "progress" => ("PROGRESS", T::SUCCESS),
        "tried" => ("TRIED", T::WARN),
        "blocked" => ("BLOCKED", T::DANGER),
        "decision" => ("DECISION", T::ACCENT),
        "dismissed" => ("DISMISSED", T::TEXT_3),
        "note" => ("NOTE", T::ACCENT),
        _ => ("CALLS", T::TEXT_3),
    }
}

/// The collapsible timeline card shown above the notes list.
pub(super) fn render_timeline(ui: &mut egui::Ui, repo: &Path) {
    let (items, hidden) = load(repo);
    T::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::CollapsingHeader::new(
            RichText::new("WORK JOURNAL: WHAT THE AGENT DID AND WHY")
                .small()
                .family(T::semibold_family())
                .color(T::ACCENT),
        )
        .default_open(true)
        .show(ui, |ui| {
            if items.is_empty() {
                ui.label(
                    RichText::new(
                        "Nothing yet. Agent tool calls over MCP and remember(kind=progress|tried|blocked) entries appear here.",
                    )
                    .small()
                    .color(T::TEXT_3),
                );
                return;
            }
            for item in &items {
                let (label, color) = badge(&item.kind);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        T::mono(fmt_ts(&item.timestamp), 11.0).color(T::TEXT_3),
                    );
                    egui::Frame::NONE
                        .fill(color.gamma_multiply(0.18))
                        .corner_radius(CornerRadius::same(T::R_CARD))
                        .inner_margin(Margin::symmetric(6, 1))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(label)
                                    .small()
                                    .family(T::semibold_family())
                                    .color(color),
                            );
                        });
                    ui.label(RichText::new(&item.title).color(T::TEXT));
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
                            .color(T::TEXT_2),
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
                    .color(T::TEXT_3),
                );
            }
        });
    });
    ui.add_space(12.0);
}
