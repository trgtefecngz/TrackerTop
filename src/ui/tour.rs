//! Debug builds only: `TRACKERTOP_UI_TOUR=<dir>` walks the app through every view and graph
//! level and saves a screenshot of each (`<dir>/NN-name.bmp`), then exits. Used to check the
//! UI on screen without driving the real mouse.

use super::{GraphLayoutMode, ProjectTab, Selection, TrackerTopApp};
use crate::nested::NestedLevel;
use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;

pub struct Tour {
    dir: PathBuf,
    step: usize,
    frames: u32,
    requested: bool,
    name: &'static str,
}

const SETTLE_FRAMES: u32 = 45;

impl Tour {
    pub fn from_env() -> Option<Self> {
        let dir = PathBuf::from(std::env::var_os("TRACKERTOP_UI_TOUR")?);
        std::fs::create_dir_all(&dir).ok()?;
        Some(Self {
            dir,
            step: 0,
            frames: 0,
            requested: false,
            name: "",
        })
    }
}

impl TrackerTopApp {
    /// Applies tour step `n`; returns its name, or `None` when the tour is over.
    fn tour_apply(&mut self, n: usize) -> Option<&'static str> {
        let gv = &mut self.graph_view;
        gv.forced_hover = None;
        let name = match n {
            0 => {
                self.active_tab = ProjectTab::Graph;
                self.graph_layout_mode = GraphLayoutMode::Nested;
                self.nested_level = NestedLevel::Project;
                gv.begin_transition(None, None, false);
                "01-project"
            }
            1 => {
                gv.forced_hover = self.scene.as_ref().and_then(|(_, s)| {
                    (0..s.nodes.len())
                        .max_by(|&a, &b| s.nodes[a].rank.total_cmp(&s.nodes[b].rank))
                        .map(|i| s.nodes[i].id.clone())
                });
                "02-project-hover"
            }
            2 => {
                let lp = self.loaded.as_ref()?;
                let big = lp
                    .community_files
                    .iter()
                    .max_by_key(|(id, f)| (f.len(), std::cmp::Reverse((*id).clone())))?;
                self.nested_level = NestedLevel::Community(big.0.clone());
                gv.begin_transition(None, None, false);
                "03a-drill-transition"
            }
            3 => {
                let lp = self.loaded.as_ref()?;
                let big = lp
                    .community_files
                    .iter()
                    .max_by_key(|(id, f)| (f.len(), std::cmp::Reverse((*id).clone())))?;
                self.nested_level = NestedLevel::Community(big.0.clone());
                "03-community"
            }
            4 => {
                let lp = self.loaded.as_ref()?;
                let hot = lp
                    .activity
                    .heat
                    .iter()
                    .filter(|(p, _)| lp.graph.symbols_by_file.contains_key(*p))
                    .max_by_key(|(p, n)| (**n, std::cmp::Reverse((*p).clone())))
                    .map(|(p, _)| p.clone())
                    .or_else(|| {
                        lp.nodes
                            .iter()
                            .max_by(|a, b| a.rank.total_cmp(&b.rank))
                            .map(|n| n.id.clone())
                    })?;
                self.nested_level = NestedLevel::File(hot);
                gv.begin_transition(None, None, false);
                "04-file"
            }
            5 => {
                gv.selected = self.scene.as_ref().and_then(|(_, s)| {
                    s.nodes
                        .iter()
                        .max_by(|a, b| a.rank.total_cmp(&b.rank))
                        .map(|n| n.id.clone())
                });
                "05-file-selected"
            }
            6 => {
                // Back up to the community: the parent settles around the file.
                let lp = self.loaded.as_ref()?;
                let NestedLevel::File(f) = self.nested_level.clone() else {
                    return None;
                };
                let view = crate::nested::NestedGraph::new(&lp.graph)
                    .view(&self.nested_level)
                    .ok()?;
                let parent = view
                    .breadcrumbs
                    .iter()
                    .rev()
                    .nth(1)
                    .map(|b| b.1.clone())
                    .unwrap_or(NestedLevel::Project);
                self.nested_level = parent;
                gv.begin_transition(None, Some(format!("f:{f}")), true);
                "06-back-to-community"
            }
            7 => {
                self.graph_layout_mode = GraphLayoutMode::Clusters;
                gv.begin_transition(None, None, false);
                "07-clusters-overview"
            }
            8 => {
                gv.zoom = 2.2;
                "08-clusters-zoomed"
            }
            9 => {
                self.graph_layout_mode = GraphLayoutMode::Folders;
                gv.begin_transition(None, None, false);
                "09-folders"
            }
            10 => {
                self.active_tab = ProjectTab::Savings;
                "10-savings"
            }
            11 => {
                self.active_tab = ProjectTab::Architecture;
                "11-architecture"
            }
            12 => {
                self.active_tab = ProjectTab::Memory;
                "12-memory"
            }
            13 => {
                self.selection = Selection::AllProjects;
                self.loaded = None;
                "13-global"
            }
            _ => return None,
        };
        Some(name)
    }

    pub(super) fn tour_tick(&mut self, ctx: &egui::Context) {
        let Some(mut tour) = self.tour.take() else {
            return;
        };
        ctx.request_repaint();
        // Save any screenshot that arrived.
        let shots: Vec<(String, Arc<egui::ColorImage>)> = ctx.input(|i| {
            i.raw
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Screenshot {
                        user_data, image, ..
                    } => user_data
                        .data
                        .as_ref()
                        .and_then(|d| d.downcast_ref::<String>())
                        .map(|n| (n.clone(), image.clone())),
                    _ => None,
                })
                .collect()
        });
        for (name, img) in shots {
            let _ = write_bmp(&tour.dir.join(format!("{name}.bmp")), &img);
            tour.step += 1;
            tour.frames = 0;
            tour.requested = false;
        }
        if !tour.requested {
            if tour.frames == 0 {
                match self.tour_apply(tour.step) {
                    Some(name) => tour.name = name,
                    None => std::process::exit(0),
                }
            }
            tour.frames += 1;
            let settle = if tour.name.contains("transition") {
                4
            } else {
                SETTLE_FRAMES
            };
            if tour.frames >= settle {
                let name = tour.name;
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
                    name.to_string(),
                )));
                tour.requested = true;
            }
        }
        self.tour = Some(tour);
    }
}

fn write_bmp(path: &std::path::Path, img: &egui::ColorImage) -> std::io::Result<()> {
    let [w, h] = img.size;
    let row = w * 4;
    let size = 54 + row * h;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(-(h as i32)).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for p in &img.pixels {
        out.extend_from_slice(&[p.b(), p.g(), p.r(), 255]);
    }
    std::fs::write(path, out)
}
