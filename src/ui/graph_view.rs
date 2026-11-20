//! Code Graph canvas: force-directed layout clustered by group, curved edges (bundled
//! between groups at low zoom), level-of-detail labels, hover focus, animated drill
//! transitions, a minimap and the agent activity layer (heat, trail, badges).
//!
//! The canvas only draws a [`Scene`]; what a scene contains, and the counts in its coverage
//! line, come from the data layer (`crate::nested`, `crate::activity`).

use super::theme as T;
use crate::activity::ActivityStep;
use crate::nested::NestedLevel;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Id, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2,
    epaint::QuadraticBezierShape,
};
use std::collections::{HashMap, HashSet};

/// Drill transition length in seconds.
pub const TRANSITION_SECS: f32 = 0.45;
const HOVER_FADE_SECS: f32 = 0.12;
/// Below this zoom, edges between groups are bundled (all-files layouts).
pub const BUNDLE_BELOW_ZOOM: f32 = 1.1;
/// Scenes up to this many nodes label every node (overlaps still skipped).
const LABEL_ALL_UP_TO: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Community,
    File,
    Symbol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemKind {
    Note,
    Decision,
    Finding,
}

#[derive(Debug, Clone)]
pub struct MemItem {
    pub kind: MemKind,
    pub text: String,
    pub target: String,
    pub when: String,
}

#[derive(Debug, Clone)]
pub struct SceneNode {
    pub id: String,
    pub label: String,
    pub kind: NodeKind,
    pub color: Color32,
    /// Cluster key for the layout (community or folder index).
    pub group: usize,
    /// Summed PageRank; drives the radius.
    pub rank: f64,
    pub tokens: usize,
    pub child_count: usize,
    pub location: Option<String>,
    /// Language for files, symbol kind for symbols.
    pub detail: String,
    pub signature: String,
    pub drill: Option<NestedLevel>,
    /// Agent calls (last days) that named this node.
    pub heat: usize,
    /// Has uncommitted changes (files), or holds a changed file (communities).
    pub changed: bool,
    pub mem: Vec<MemItem>,
}

#[derive(Debug, Clone)]
pub struct SceneEdge {
    pub src: usize,
    pub tgt: usize,
    pub weight: usize,
}

/// Edges leaving the scene toward one outside group, drawn as a pill at the border.
#[derive(Debug, Clone)]
pub struct Portal {
    pub label: String,
    pub color: Color32,
    pub count: usize,
    pub level: Option<NestedLevel>,
    /// `(node index, edges)` inside the scene that carry these edges.
    pub from: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Node(usize),
    Portal(usize),
}

pub struct Scene {
    pub title: String,
    pub hint: String,
    pub coverage: String,
    pub nodes: Vec<SceneNode>,
    pub edges: Vec<SceneEdge>,
    pub portals: Vec<Portal>,
    /// Group labels and colours (all-files layouts), indexed by `SceneNode::group`.
    pub groups: Vec<(String, Color32)>,
    /// Bundle edges between different groups at low zoom.
    pub bundle_groups: bool,
    /// Where a trail step lands: file path or symbol id → node or portal.
    pub locate: HashMap<String, Target>,
    pub pos: Vec<Pos2>,
    pub radius: Vec<f32>,
    pub portal_pos: Vec<Pos2>,
    pub bounds: Rect,
    pub max_heat: usize,
}

impl Scene {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        title: String,
        hint: String,
        coverage: String,
        nodes: Vec<SceneNode>,
        edges: Vec<SceneEdge>,
        portals: Vec<Portal>,
        groups: Vec<(String, Color32)>,
        bundle_groups: bool,
        locate: HashMap<String, Target>,
    ) -> Self {
        let max_rank = nodes.iter().map(|n| n.rank).fold(0.0_f64, f64::max);
        let radius: Vec<f32> = nodes
            .iter()
            .map(|n| node_radius(n.kind, n.rank, max_rank, nodes.len()))
            .collect();
        let links: Vec<(usize, usize, f32)> = edges
            .iter()
            .map(|e| (e.src, e.tgt, e.weight as f32))
            .collect();
        let group_of: Vec<usize> = nodes.iter().map(|n| n.group).collect();
        let seed = nodes
            .iter()
            .fold(0x9E37_79B9_7F4A_7C15_u64, |h, n| hash_str(h, &n.id));
        let pos = force_layout(&radius, &group_of, &links, seed);
        let mut bounds = bounds_of(&pos, &radius);
        let portal_pos = place_portals(portals.len(), bounds);
        for p in &portal_pos {
            bounds = bounds.union(Rect::from_center_size(*p, Vec2::new(160.0, 30.0)));
        }
        let max_heat = nodes.iter().map(|n| n.heat).max().unwrap_or(0);
        Self {
            title,
            hint,
            coverage,
            nodes,
            edges,
            portals,
            groups,
            bundle_groups,
            locate,
            pos,
            radius,
            portal_pos,
            bounds,
            max_heat,
        }
    }

    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.id == id)
    }

    /// Where a trail step lands in this scene: its symbol, else its first file that maps.
    pub fn locate_step(&self, step: &ActivityStep) -> Option<Target> {
        step.symbol
            .as_ref()
            .and_then(|s| self.locate.get(s))
            .or_else(|| step.files.iter().find_map(|f| self.locate.get(f)))
            .copied()
    }
}

fn node_radius(kind: NodeKind, rank: f64, max_rank: f64, n: usize) -> f32 {
    let t = if max_rank > 0.0 {
        (rank / max_rank).sqrt() as f32
    } else {
        0.3
    };
    let (lo, hi) = match kind {
        NodeKind::Community => (16.0, 42.0),
        NodeKind::File if n > 300 => (3.5, 14.0),
        NodeKind::File => (5.0, 20.0),
        NodeKind::Symbol => (4.5, 16.0),
    };
    lo + (hi - lo) * t
}

fn hash_str(mut h: u64, s: &str) -> u64 {
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01B3);
    }
    h
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// Deterministic force-directed layout (Fruchterman–Reingold with grid-limited repulsion).
/// Nodes are pulled toward their group's centre, so groups form visible clusters.
pub fn force_layout(
    radius: &[f32],
    group: &[usize],
    links: &[(usize, usize, f32)],
    seed: u64,
) -> Vec<Pos2> {
    let n = radius.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![Pos2::ZERO];
    }
    let mut rng = Rng(seed | 1);
    let avg_r = radius.iter().sum::<f32>() / n as f32;
    let k = (avg_r * 2.5 + 44.0).max(36.0);

    // Group centres on a golden-angle spiral, largest group in the middle.
    let mut sizes: HashMap<usize, usize> = HashMap::new();
    for g in group {
        *sizes.entry(*g).or_insert(0) += 1;
    }
    let mut order: Vec<(usize, usize)> = sizes.into_iter().collect();
    order.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let multi = order.len() > 1;
    let spacing = k * (n as f32 / order.len() as f32).sqrt() * 1.6;
    let centre: HashMap<usize, Vec2> = order
        .iter()
        .enumerate()
        .map(|(i, (g, _))| {
            let a = i as f32 * 2.399_963;
            let r = if multi {
                spacing * (i as f32).sqrt()
            } else {
                0.0
            };
            (*g, Vec2::new(a.cos() * r, a.sin() * r))
        })
        .collect();

    let spread = k * (n as f32).sqrt() * 0.5;
    let mut p: Vec<Vec2> = (0..n)
        .map(|i| {
            centre[&group[i]]
                + Vec2::new(rng.next() - 0.5, rng.next() - 0.5)
                    * if multi { spread * 0.4 } else { spread }
        })
        .collect();

    // Dense graphs would collapse under their own springs: weaken them by mean degree.
    let avg_degree = 2.0 * links.len() as f32 / n as f32;
    let att_scale = 1.0 / (1.0 + avg_degree / 3.0).sqrt();
    let iters = ((300.0 * (500.0 / n as f32).min(1.0)) as usize).clamp(80, 300);
    let cell = k * 2.5;
    let mut t = k * (n as f32).sqrt() * 0.25;
    let cool = (0.02 / t.max(0.02)).powf(1.0 / iters as f32);
    let mut disp = vec![Vec2::ZERO; n];
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for _ in 0..iters {
        disp.iter_mut().for_each(|d| *d = Vec2::ZERO);
        grid.clear();
        for (i, q) in p.iter().enumerate() {
            grid.entry(((q.x / cell).floor() as i32, (q.y / cell).floor() as i32))
                .or_default()
                .push(i);
        }
        for i in 0..n {
            let (cx, cy) = (
                (p[i].x / cell).floor() as i32,
                (p[i].y / cell).floor() as i32,
            );
            for dx in -1..=1 {
                for dy in -1..=1 {
                    let Some(list) = grid.get(&(cx + dx, cy + dy)) else {
                        continue;
                    };
                    for &j in list {
                        if j <= i {
                            continue;
                        }
                        let mut d = p[i] - p[j];
                        let mut len = d.length();
                        if len < 0.01 {
                            d = Vec2::new(rng.next() - 0.5, rng.next() - 0.5);
                            len = d.length().max(0.01);
                        }
                        let mut f = k * k / len;
                        let min_d = radius[i] + radius[j] + 10.0;
                        if len < min_d {
                            f += (min_d - len) * 4.0;
                        }
                        let v = d / len * f;
                        disp[i] += v;
                        disp[j] -= v;
                    }
                }
            }
        }
        for &(a, b, w) in links {
            if a == b || a >= n || b >= n {
                continue;
            }
            let d = p[a] - p[b];
            let len = d.length().max(0.01);
            let strength = 1.0 + w.max(1.0).log2().min(3.0) * 0.25;
            let same = group[a] == group[b];
            let f = len * len / k * strength * att_scale * if same || !multi { 1.0 } else { 0.08 };
            let v = d / len * f;
            disp[a] -= v;
            disp[b] += v;
        }
        for i in 0..n {
            let c = centre[&group[i]];
            disp[i] += (c - p[i]) * if multi { 0.1 } else { 0.02 } * k;
            let len = disp[i].length();
            if len > 0.0 {
                p[i] += disp[i] / len * len.min(t);
            }
        }
        t *= cool;
    }
    if multi {
        pack_groups(&mut p, group, radius);
    }
    // Final pass: no two nodes overlap, with room for a label between them.
    for _ in 0..if n <= 800 { 60 } else { 0 } {
        let mut moved = false;
        for i in 0..n {
            for j in i + 1..n {
                let d = p[i] - p[j];
                let len = d.length().max(0.01);
                let min_d = radius[i] + radius[j] + 30.0;
                if len < min_d {
                    let dir = if d.length() < 0.01 { Vec2::X } else { d / len };
                    let push = dir * (min_d - len) * 0.5;
                    p[i] += push;
                    p[j] -= push;
                    moved = true;
                }
            }
        }
        if !moved {
            break;
        }
    }
    p.into_iter().map(|v| v.to_pos2()).collect()
}

/// Moves whole groups (as discs) toward the centre until they touch, so a far-off small
/// group does not shrink everything else when the view is fitted.
fn pack_groups(p: &mut [Vec2], group: &[usize], radius: &[f32]) {
    let mut ids: Vec<usize> = group.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let disc = |p: &[Vec2], g: usize| {
        let members: Vec<usize> = (0..p.len()).filter(|&i| group[i] == g).collect();
        let c = members.iter().map(|&i| p[i]).fold(Vec2::ZERO, |a, b| a + b) / members.len() as f32;
        let r = members
            .iter()
            .map(|&i| (p[i] - c).length() + radius[i])
            .fold(0.0_f32, f32::max)
            + 40.0;
        (c, r)
    };
    for _ in 0..80 {
        let discs: Vec<(Vec2, f32)> = ids.iter().map(|&g| disc(p, g)).collect();
        let total: f32 = discs.iter().map(|d| d.1 * d.1).sum();
        let centre = discs.iter().fold(Vec2::ZERO, |a, d| a + d.0 * d.1 * d.1) / total.max(1.0);
        let mut shift = vec![Vec2::ZERO; ids.len()];
        for a in 0..ids.len() {
            shift[a] += (centre - discs[a].0) * 0.08;
            for b in 0..ids.len() {
                if a == b {
                    continue;
                }
                let d = discs[a].0 - discs[b].0;
                let len = d.length().max(0.01);
                let min_d = discs[a].1 + discs[b].1;
                if len < min_d {
                    shift[a] += d / len * (min_d - len) * 0.5;
                }
            }
        }
        for (k, &g) in ids.iter().enumerate() {
            for i in 0..p.len() {
                if group[i] == g {
                    p[i] += shift[k];
                }
            }
        }
    }
}

fn bounds_of(pos: &[Pos2], radius: &[f32]) -> Rect {
    let mut r = Rect::NOTHING;
    for (p, rad) in pos.iter().zip(radius) {
        r = r.union(Rect::from_center_size(*p, Vec2::splat(rad * 2.0 + 40.0)));
    }
    if r.is_negative() {
        Rect::from_center_size(Pos2::ZERO, Vec2::splat(200.0))
    } else {
        r
    }
}

/// Portals sit on an ellipse just outside the nodes, largest first from the top.
fn place_portals(count: usize, bounds: Rect) -> Vec<Pos2> {
    let c = bounds.center();
    let (rx, ry) = (bounds.width() * 0.5 + 120.0, bounds.height() * 0.5 + 70.0);
    (0..count)
        .map(|i| {
            let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * i as f32 / count as f32;
            Pos2::new(c.x + rx * a.cos(), c.y + ry * a.sin())
        })
        .collect()
}

#[derive(Debug, Clone)]
struct Transition {
    id: u64,
    /// In: children grow out of this screen point. Out: the parent settles around `focus`.
    origin: Option<Pos2>,
    focus: Option<String>,
    out: bool,
    primed: bool,
}

/// Camera, interaction and agent-layer state of the canvas.
pub struct GraphViewState {
    pub zoom: f32,
    pub pan: Vec2,
    pub selected: Option<String>,
    pub hovered: Option<String>,
    /// Hover used when the pointer is not over the canvas (debug UI tour).
    pub forced_hover: Option<String>,
    last_hovered: Option<String>,
    pub show_heat: bool,
    pub show_trail: bool,
    pub show_badges: bool,
    pub trail_step: usize,
    pub playing: bool,
    last_step_at: f64,
    transition: Option<Transition>,
    transition_seq: u64,
    heat_tex: Option<egui::TextureHandle>,
}

impl Default for GraphViewState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: Vec2::ZERO,
            selected: None,
            hovered: None,
            forced_hover: None,
            last_hovered: None,
            show_heat: true,
            show_trail: true,
            show_badges: true,
            trail_step: usize::MAX,
            playing: false,
            last_step_at: 0.0,
            transition: None,
            transition_seq: 0,
            heat_tex: None,
        }
    }
}

impl GraphViewState {
    /// Starts a drill-in (children grow from `origin`) or drill-out (the parent level
    /// settles around the node `focus` we came from) and resets the camera.
    pub fn begin_transition(&mut self, origin: Option<Pos2>, focus: Option<String>, out: bool) {
        self.transition_seq += 1;
        self.transition = Some(Transition {
            id: self.transition_seq,
            origin,
            focus,
            out,
            primed: false,
        });
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
        self.selected = None;
        self.hovered = None;
        self.last_hovered = None;
    }

    pub fn fit(&mut self) {
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
    }

    fn progress(&mut self, ctx: &egui::Context) -> f32 {
        let Some(tr) = self.transition.as_mut() else {
            return 1.0;
        };
        let id = Id::new(("tt-graph-transition", tr.id));
        let t = if tr.primed {
            ctx.animate_value_with_time(id, 1.0, TRANSITION_SECS)
        } else {
            tr.primed = true;
            ctx.animate_value_with_time(id, 0.0, 0.0);
            ctx.request_repaint();
            0.0
        };
        if t >= 1.0 {
            self.transition = None;
        }
        t
    }
}

/// What the user asked for on the canvas this frame.
#[derive(Default)]
pub struct GraphOutput {
    /// Open a level; the point is where the clicked node was on screen.
    pub drill: Option<(NestedLevel, Pos2)>,
}

struct Cam {
    center: Pos2,
    world_center: Pos2,
    scale: f32,
}

impl Cam {
    fn to_screen(&self, w: Pos2) -> Pos2 {
        self.center + (w - self.world_center) * self.scale
    }
    fn to_world(&self, s: Pos2) -> Pos2 {
        self.world_center + (s - self.center) / self.scale
    }
}

/// Centre of the free area: below the title, above the trail player.
fn view_center(rect: Rect) -> Pos2 {
    rect.center() - Vec2::new(0.0, 30.0)
}

fn camera(scene: &Scene, rect: Rect, st: &GraphViewState) -> Cam {
    let b = scene.bounds;
    let fit = ((rect.width() - 40.0) / b.width().max(1.0))
        .min((rect.height() - 210.0).max(80.0) / b.height().max(1.0))
        .clamp(0.05, 2.0);
    Cam {
        center: view_center(rect) + st.pan,
        world_center: b.center(),
        scale: fit * st.zoom,
    }
}

fn with_alpha(c: Color32, a: f32) -> Color32 {
    c.gamma_multiply(a.clamp(0.0, 1.0))
}

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn curve_ctrl(a: Pos2, b: Pos2, bend: f32) -> Pos2 {
    let m = a + (b - a) * 0.5;
    let d = b - a;
    m + Vec2::new(-d.y, d.x) * bend
}

fn heat_texture(ctx: &egui::Context) -> egui::TextureHandle {
    const N: usize = 64;
    let mut px = Vec::with_capacity(N * N * 4);
    for y in 0..N {
        for x in 0..N {
            let dx = (x as f32 + 0.5) / N as f32 * 2.0 - 1.0;
            let dy = (y as f32 + 0.5) / N as f32 * 2.0 - 1.0;
            let d = (dx * dx + dy * dy).sqrt().min(1.0);
            let a = (1.0 - d).powf(1.8);
            let c = T::HEAT;
            px.extend_from_slice(&[
                (c.r() as f32 * a) as u8,
                (c.g() as f32 * a) as u8,
                (c.b() as f32 * a) as u8,
                (255.0 * a) as u8,
            ]);
        }
    }
    let img = egui::ColorImage::from_rgba_premultiplied([N, N], &px);
    ctx.load_texture("tt-heat", img, egui::TextureOptions::LINEAR)
}

fn dashed_circle(center: Pos2, r: f32, stroke: Stroke) -> Vec<Shape> {
    let pts: Vec<Pos2> = (0..=48)
        .map(|i| {
            let a = std::f32::consts::TAU * i as f32 / 48.0;
            center + Vec2::new(a.cos(), a.sin()) * r
        })
        .collect();
    Shape::dashed_line(&pts, stroke, 3.0, 3.0)
}

fn bezier_points(a: Pos2, c: Pos2, b: Pos2, steps: usize) -> Vec<Pos2> {
    (0..=steps)
        .map(|i| {
            let t = i as f32 / steps as f32;
            let u = 1.0 - t;
            Pos2::new(
                u * u * a.x + 2.0 * u * t * c.x + t * t * b.x,
                u * u * a.y + 2.0 * u * t * c.y + t * t * b.y,
            )
        })
        .collect()
}

/// Draws the canvas in `rect`. `trail` is the latest agent session.
pub fn show(
    ui: &mut egui::Ui,
    rect: Rect,
    scene: &Scene,
    st: &mut GraphViewState,
    trail: &[ActivityStep],
) -> GraphOutput {
    let mut out = GraphOutput::default();
    let ctx = ui.ctx().clone();
    let resp = ui.interact(
        rect,
        ui.id().with("tt-graph-canvas"),
        Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::ZERO, T::BG);

    // Camera input: drag to pan, wheel / pinch to zoom about the pointer.
    if resp.dragged() {
        st.pan += resp.drag_delta();
    }
    if resp.hovered() {
        let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
        let factor = (scroll * 0.0018).exp() * pinch;
        if (factor - 1.0).abs() > 1e-4
            && let Some(ptr) = resp.hover_pos()
        {
            let new_zoom = (st.zoom * factor).clamp(0.4, 6.0);
            let f = new_zoom / st.zoom;
            let c = view_center(rect) + st.pan;
            st.pan += (ptr - c) * (1.0 - f);
            st.zoom = new_zoom;
        }
    }
    let cam = camera(scene, rect, st);

    // Dot grid, fixed to the world so it moves with the graph.
    let step = 22.0 * cam.scale.clamp(0.5, 3.0) / cam.scale.clamp(0.5, 3.0) * 1.0;
    let origin = cam.to_screen(Pos2::ZERO);
    let ox = (origin.x - rect.left()).rem_euclid(step);
    let oy = (origin.y - rect.top()).rem_euclid(step);
    let mut y = rect.top() + oy;
    while y < rect.bottom() {
        let mut x = rect.left() + ox;
        while x < rect.right() {
            painter.circle_filled(Pos2::new(x, y), 0.9, T::GRID_DOT);
            x += step;
        }
        y += step;
    }

    if scene.nodes.is_empty() {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "This level has no children.",
            T::ui_font(14.0),
            T::TEXT_3,
        );
    }

    // Transition: interpolate screen positions.
    let t_raw = st.progress(&ctx);
    let e = ease_out_cubic(t_raw);
    let fade = (t_raw * 1.6).min(1.0);
    let efade = t_raw * t_raw;
    let final_pos: Vec<Pos2> = scene.pos.iter().map(|p| cam.to_screen(*p)).collect();
    let final_portal: Vec<Pos2> = scene.portal_pos.iter().map(|p| cam.to_screen(*p)).collect();
    let tr = st.transition.clone();
    let focus = tr.as_ref().and_then(|tr| {
        tr.focus
            .as_ref()
            .and_then(|f| scene.index_of(f))
            .map(|i| final_pos[i])
            .or(tr.origin)
    });
    let anim = |p: Pos2| -> Pos2 {
        match &tr {
            None => p,
            Some(tr) if !tr.out => {
                let o = tr.origin.unwrap_or(rect.center());
                o + (p - o) * e
            }
            Some(_) => {
                let f = focus.unwrap_or(rect.center());
                let start = f + (p - f) * 2.6;
                start + (p - start) * e
            }
        }
    };
    let pos: Vec<Pos2> = final_pos.iter().map(|p| anim(*p)).collect();
    let ppos: Vec<Pos2> = final_portal.iter().map(|p| anim(*p)).collect();
    let rad: Vec<f32> = scene
        .radius
        .iter()
        .map(|r| (r * cam.scale).clamp(2.5, 90.0))
        .collect();

    // Hover: nearest node within its radius, or a portal pill.
    let pointer = resp.hover_pos();
    let hovered_node = pointer.and_then(|h| {
        (0..scene.nodes.len())
            .filter(|&i| h.distance(pos[i]) <= rad[i].max(6.0) + 2.0)
            .min_by(|&a, &b| h.distance(pos[a]).total_cmp(&h.distance(pos[b])))
    });
    let hovered_node = hovered_node.or_else(|| {
        if pointer.is_none() {
            st.forced_hover.as_ref().and_then(|id| scene.index_of(id))
        } else {
            None
        }
    });
    let portal_rects: Vec<Rect> = scene
        .portals
        .iter()
        .zip(&ppos)
        .map(|(p, c)| {
            let w = painter
                .layout_no_wrap(portal_text(p), T::ui_font(11.0), T::TEXT)
                .size()
                .x
                + 30.0;
            Rect::from_center_size(*c, Vec2::new(w, 24.0))
        })
        .collect();
    let hovered_portal = pointer.and_then(|h| portal_rects.iter().position(|r| r.contains(h)));
    let hover_target = hovered_node
        .map(Target::Node)
        .or(hovered_portal.map(Target::Portal));
    st.hovered = hovered_node.map(|i| scene.nodes[i].id.clone());
    if st.hovered.is_some() {
        st.last_hovered = st.hovered.clone();
    }
    let dim = ctx.animate_bool_with_time(
        ui.id().with("tt-graph-hover"),
        hover_target.is_some(),
        HOVER_FADE_SECS,
    );
    // Neighbourhood of the hovered (or fading-out) target.
    let focus_target = hover_target.or_else(|| {
        st.last_hovered
            .as_ref()
            .and_then(|id| scene.index_of(id))
            .map(Target::Node)
    });
    let mut near: HashSet<usize> = HashSet::new();
    let mut near_portals: HashSet<usize> = HashSet::new();
    match focus_target {
        Some(Target::Node(h)) => {
            near.insert(h);
            for ed in &scene.edges {
                if ed.src == h {
                    near.insert(ed.tgt);
                } else if ed.tgt == h {
                    near.insert(ed.src);
                }
            }
            for (j, p) in scene.portals.iter().enumerate() {
                if p.from.iter().any(|(i, _)| *i == h) {
                    near_portals.insert(j);
                }
            }
        }
        Some(Target::Portal(j)) => {
            near_portals.insert(j);
            near.extend(scene.portals[j].from.iter().map(|(i, _)| *i));
        }
        None => {}
    }
    let node_alpha = |i: usize| -> f32 {
        let a = if focus_target.is_some() && !near.contains(&i) {
            1.0 - 0.88 * dim
        } else {
            1.0
        };
        a * fade
    };

    // Edges.
    let bundled = scene.bundle_groups && st.zoom < BUNDLE_BELOW_ZOOM;
    let max_w = scene
        .edges
        .iter()
        .map(|e| e.weight)
        .max()
        .unwrap_or(1)
        .max(1) as f32;
    let curved = scene.edges.len() <= 2500;
    let mut count_labels: Vec<(Pos2, String, Color32)> = Vec::new();
    let mut bundles: HashMap<(usize, usize), usize> = HashMap::new();
    let hovered_idx = match hover_target {
        Some(Target::Node(i)) => Some(i),
        _ => None,
    };
    for ed in &scene.edges {
        let (a, b) = (pos[ed.src], pos[ed.tgt]);
        if ed.src == ed.tgt || (b - a).length() < 1.0 {
            continue;
        }
        let incident = hovered_idx.is_some_and(|h| h == ed.src || h == ed.tgt);
        let (ga, gb) = (scene.nodes[ed.src].group, scene.nodes[ed.tgt].group);
        if bundled && ga != gb && !incident {
            *bundles.entry((ga.min(gb), ga.max(gb))).or_insert(0) += 1;
            continue;
        }
        let tw = (ed.weight as f32 / max_w).sqrt();
        let base = match scene.nodes[ed.src].kind {
            NodeKind::Community => with_alpha(scene.nodes[ed.src].color, 0.45),
            _ if scene.edges.len() > 150 => with_alpha(T::CONTROL_STRONG, 0.45),
            _ => with_alpha(T::CONTROL_STRONG, 0.75),
        };
        let (color, width) = if incident {
            if hovered_idx == Some(ed.src) {
                (T::ACCENT, 1.4 + 2.0 * tw)
            } else {
                (T::TEXT_2, 1.4 + 2.0 * tw)
            }
        } else {
            let a = if focus_target.is_some() {
                1.0 - 0.94 * dim
            } else {
                1.0
            };
            (with_alpha(base, a), 0.8 + 2.4 * tw)
        };
        let color = with_alpha(color, efade);
        let dir = (b - a).normalized();
        let start = a + dir * rad[ed.src];
        let end = b - dir * (rad[ed.tgt] + 1.0);
        let ctrl = curve_ctrl(start, end, 0.16);
        if curved {
            painter.add(QuadraticBezierShape::from_points_stroke(
                [start, ctrl, end],
                false,
                Color32::TRANSPARENT,
                Stroke::new(width, color),
            ));
        } else {
            painter.line_segment([start, end], Stroke::new(width, color));
        }
        if incident {
            // Arrowhead along the curve's end tangent.
            let tan = (end - ctrl).normalized();
            let normal = Vec2::new(-tan.y, tan.x);
            let head = 7.0_f32.min((end - start).length() * 0.3);
            let back = end - tan * head;
            painter.add(Shape::convex_polygon(
                vec![
                    end,
                    back + normal * head * 0.45,
                    back - normal * head * 0.45,
                ],
                color,
                Stroke::NONE,
            ));
            let at = if hovered_idx == Some(ed.src) {
                0.7
            } else {
                0.3
            };
            let pts = bezier_points(start, ctrl, end, 10);
            let lp = pts[((pts.len() - 1) as f32 * at) as usize];
            if !scene.bundle_groups {
                count_labels.push((lp, fmt_count(ed.weight), color));
            }
        }
    }
    let mut bundle_count = 0usize;
    if bundled {
        let mut centroid: HashMap<usize, (Vec2, f32)> = HashMap::new();
        for (i, n) in scene.nodes.iter().enumerate() {
            let c = centroid.entry(n.group).or_insert((Vec2::ZERO, 0.0));
            c.0 += pos[i].to_vec2();
            c.1 += 1.0;
        }
        let mut list: Vec<_> = bundles.into_iter().collect();
        list.sort();
        bundle_count = list.len();
        let a_dim = if focus_target.is_some() {
            1.0 - 0.94 * dim
        } else {
            1.0
        };
        for ((ga, gb), count) in list {
            let (Some(ca), Some(cb)) = (centroid.get(&ga), centroid.get(&gb)) else {
                continue;
            };
            let (a, b) = ((ca.0 / ca.1).to_pos2(), (cb.0 / cb.1).to_pos2());
            let color = scene
                .groups
                .get(ga)
                .map(|g| g.1)
                .unwrap_or(T::CONTROL_STRONG);
            let width = (1.0 + (1.0 + count as f32).log2() * 1.4).min(9.0);
            painter.add(QuadraticBezierShape::from_points_stroke(
                [a, curve_ctrl(a, b, 0.2), b],
                false,
                Color32::TRANSPARENT,
                Stroke::new(width, with_alpha(color, 0.32 * a_dim * efade)),
            ));
        }
    }

    // Portal stubs.
    for (j, p) in scene.portals.iter().enumerate() {
        let hot = near_portals.contains(&j);
        for &(i, _) in &p.from {
            let a_dim = if focus_target.is_some() && !hot {
                1.0 - 0.94 * dim
            } else {
                1.0
            };
            let color = if hot && focus_target.is_some() {
                with_alpha(p.color, 0.8 * efade)
            } else {
                with_alpha(T::CONTROL_STRONG, 0.55 * a_dim * efade)
            };
            let pts = bezier_points(pos[i], curve_ctrl(pos[i], ppos[j], 0.12), ppos[j], 16);
            for s in Shape::dashed_line(&pts, Stroke::new(1.0_f32, color), 3.0, 4.0) {
                painter.add(s);
            }
        }
    }

    // Heat glow (behind nodes).
    if st.show_heat && scene.max_heat > 0 {
        let tex = st.heat_tex.get_or_insert_with(|| heat_texture(&ctx)).id();
        for (i, n) in scene.nodes.iter().enumerate() {
            if n.heat == 0 {
                continue;
            }
            let h = (n.heat as f32 / scene.max_heat as f32).sqrt();
            let a = (0.7 * h * node_alpha(i)).clamp(0.0, 1.0);
            let size = rad[i] * 4.6 + 22.0;
            let tint = Color32::from_rgba_premultiplied(
                (255.0 * a) as u8,
                (255.0 * a) as u8,
                (255.0 * a) as u8,
                (255.0 * a) as u8,
            );
            painter.add(Shape::image(
                tex,
                Rect::from_center_size(pos[i], Vec2::splat(size)),
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                tint,
            ));
        }
    }

    // Trail path (under nodes).
    let mut marks: Vec<(Pos2, usize)> = Vec::new();
    if st.show_trail && !trail.is_empty() {
        let upto = st.trail_step.min(trail.len());
        let mut last: Option<Target> = None;
        for (k, step) in trail.iter().enumerate().take(upto) {
            let Some(target) = scene.locate_step(step) else {
                continue;
            };
            let p = match target {
                Target::Node(i) => pos[i],
                Target::Portal(j) => ppos[j],
            };
            if last == Some(target) {
                if let Some(m) = marks.last_mut() {
                    m.1 = k + 1;
                }
                continue;
            }
            last = Some(target);
            marks.push((p, k + 1));
        }
        for w in marks.windows(2) {
            let (a, b) = (w[0].0, w[1].0);
            let pts = bezier_points(a, curve_ctrl(a, b, 0.25), b, 20);
            for s in Shape::dashed_line(
                &pts,
                Stroke::new(2.0_f32, with_alpha(T::HEAT, 0.85 * fade)),
                6.0,
                5.0,
            ) {
                painter.add(s);
            }
        }
    }

    // Nodes.
    for (i, n) in scene.nodes.iter().enumerate() {
        let a = node_alpha(i);
        let is_sel = st.selected.as_deref() == Some(n.id.as_str());
        let is_hov = hovered_idx == Some(i);
        painter.circle(
            pos[i],
            rad[i],
            with_alpha(n.color, a),
            Stroke::new(
                1.0_f32,
                if is_hov {
                    with_alpha(Color32::WHITE, a)
                } else {
                    T::BG
                },
            ),
        );
        if is_sel {
            painter.circle_stroke(
                pos[i],
                rad[i] + 5.0,
                Stroke::new(2.0_f32, with_alpha(T::ACCENT, a)),
            );
        }
        if st.show_heat && n.changed {
            for s in dashed_circle(
                pos[i],
                rad[i] + 4.0,
                Stroke::new(1.5_f32, with_alpha(T::HEAT, a)),
            ) {
                painter.add(s);
            }
        }
    }

    // Portals.
    for (j, p) in scene.portals.iter().enumerate() {
        let r = portal_rects[j];
        let hot = hovered_portal == Some(j);
        let a = if focus_target.is_some() && !near_portals.contains(&j) {
            1.0 - 0.88 * dim
        } else {
            1.0
        } * fade;
        painter.rect(
            r,
            CornerRadius::same(T::R_PILL),
            with_alpha(if hot { T::HOVER } else { T::CARD }, a),
            Stroke::new(1.0_f32, with_alpha(p.color, a)),
            StrokeKind::Inside,
        );
        painter.circle_filled(
            Pos2::new(r.left() + 12.0, r.center().y),
            3.5,
            with_alpha(p.color, a),
        );
        painter.text(
            Pos2::new(r.left() + 21.0, r.center().y),
            Align2::LEFT_CENTER,
            portal_text(p),
            T::ui_font(11.0),
            with_alpha(T::TEXT, a),
        );
    }

    // Labels: level of detail by zoom, largest first, overlaps skipped.
    let n_nodes = scene.nodes.len();
    let limit = if n_nodes <= LABEL_ALL_UP_TO {
        usize::MAX
    } else if st.zoom < 1.1 {
        12
    } else if st.zoom < 1.8 {
        40
    } else {
        usize::MAX
    };
    let mut order: Vec<usize> = (0..n_nodes).collect();
    order.sort_by(|&a, &b| {
        scene.nodes[b]
            .rank
            .total_cmp(&scene.nodes[a].rank)
            .then(a.cmp(&b))
    });
    let mut priority: Vec<usize> = Vec::new();
    if let Some(h) = hovered_idx {
        priority.push(h);
    }
    if let Some(s) = st.selected.as_ref().and_then(|s| scene.index_of(s)) {
        priority.push(s);
    }
    priority.extend(near.iter().copied());
    let mut placed: Vec<Rect> = Vec::new();
    // Badges follow labels, so they thin out with the same level of detail.
    let mut labelled: HashSet<usize> = HashSet::new();
    let mut shown = 0usize;
    let mut drawn: HashSet<usize> = HashSet::new();
    for (rank_pos, i) in priority
        .iter()
        .copied()
        .chain(order.iter().copied())
        .enumerate()
    {
        if !drawn.insert(i) {
            continue;
        }
        let forced = rank_pos < priority.len();
        if !forced && shown >= limit {
            continue;
        }
        let n = &scene.nodes[i];
        let a = node_alpha(i);
        if a < 0.3 && !forced {
            continue;
        }
        let font = match n.kind {
            NodeKind::Community => T::strong_font(13.0),
            NodeKind::File => T::mono_font(11.0),
            NodeKind::Symbol => T::mono_font(11.0),
        };
        let strong = forced || st.selected.as_deref() == Some(n.id.as_str());
        let color = with_alpha(if strong { T::TEXT } else { T::TEXT_2 }, a);
        let g = painter.layout_no_wrap(truncate_tail(&n.label, 30), font, color);
        let mut lines = vec![g];
        if n.kind == NodeKind::Community {
            lines.push(painter.layout_no_wrap(
                format!("{} files", n.child_count),
                T::ui_font(11.0),
                with_alpha(T::TEXT_3, a),
            ));
        }
        let w = lines.iter().map(|g| g.size().x).fold(0.0, f32::max);
        let h: f32 = lines.iter().map(|g| g.size().y).sum();
        let top = pos[i].y + rad[i] + 4.0;
        let r = Rect::from_min_size(Pos2::new(pos[i].x - w * 0.5, top), Vec2::new(w, h));
        if !forced && (!rect.contains_rect(r) || placed.iter().any(|p| p.expand(2.0).intersects(r)))
        {
            continue;
        }
        placed.push(r);
        labelled.insert(i);
        shown += 1;
        let mut y = top;
        for g in lines {
            let x = pos[i].x - g.size().x * 0.5;
            let bg = Rect::from_min_size(Pos2::new(x, y), g.size()).expand2(Vec2::new(3.0, 0.0));
            painter.rect_filled(bg, CornerRadius::same(3), with_alpha(T::BG, 0.72 * a));
            let gh = g.size().y;
            painter.galley(Pos2::new(x, y), g, T::TEXT);
            y += gh;
        }
    }

    // Group titles (all-files layouts) while zoomed out.
    if !scene.groups.is_empty() && st.zoom < 1.8 {
        let mut top: HashMap<usize, (f32, f32, f32)> = HashMap::new();
        for (i, n) in scene.nodes.iter().enumerate() {
            let e = top.entry(n.group).or_insert((f32::MAX, 0.0, 0.0));
            e.0 = e.0.min(pos[i].y - rad[i]);
            e.1 += pos[i].x;
            e.2 += 1.0;
        }
        for (g, (ymin, xs, cnt)) in top {
            let Some((label, color)) = scene.groups.get(g) else {
                continue;
            };
            if cnt < 2.0 {
                continue;
            }
            let p = Pos2::new(xs / cnt, ymin - 10.0);
            let a = if focus_target.is_some() { 0.3 } else { 0.9 } * fade;
            let g = painter.layout_no_wrap(
                label.to_uppercase(),
                T::strong_font(11.0),
                with_alpha(*color, a),
            );
            let r = Rect::from_min_size(p - Vec2::new(g.size().x * 0.5, g.size().y), g.size());
            if placed.iter().any(|q| q.intersects(r)) {
                continue;
            }
            placed.push(r);
            painter.galley(r.min, g, T::TEXT);
        }
    }

    // Badges.
    if st.show_badges {
        for (i, n) in scene.nodes.iter().enumerate() {
            if !labelled.contains(&i) {
                continue;
            }
            let kinds: Vec<(Color32, &str)> = [
                (MemKind::Note, T::NOTE, "N"),
                (MemKind::Decision, T::ACCENT, "D"),
                (MemKind::Finding, T::DANGER, "!"),
            ]
            .into_iter()
            .filter(|(k, _, _)| n.mem.iter().any(|m| m.kind == *k))
            .map(|(_, c, g)| (c, g))
            .collect();
            let a = node_alpha(i);
            for (j, (c, g)) in kinds.iter().enumerate() {
                let br = 6.0;
                let p = pos[i] + Vec2::new(rad[i] * 0.72 + j as f32 * br * 2.1, -rad[i] * 0.72);
                painter.circle(p, br, with_alpha(*c, a), Stroke::new(1.5_f32, T::BG));
                painter.text(
                    p,
                    Align2::CENTER_CENTER,
                    *g,
                    T::strong_font(8.5),
                    with_alpha(T::BG, a),
                );
            }
        }
    }

    // Trail markers (above nodes).
    let nmarks = marks.len();
    for (k, (p, num)) in marks.iter().enumerate() {
        if k + 1 == nmarks {
            painter.circle_stroke(
                *p,
                14.0,
                Stroke::new(1.5_f32, with_alpha(T::HEAT, 0.6 * fade)),
            );
        }
        painter.circle_filled(*p, 8.5, with_alpha(T::HEAT, fade));
        painter.text(
            *p,
            Align2::CENTER_CENTER,
            num.to_string(),
            T::mono_font(9.0),
            with_alpha(T::HEAT_INK, fade),
        );
    }

    // Edge counts for the hovered node.
    for (p, text, color) in count_labels {
        let g = painter.layout_no_wrap(text, T::mono_font(10.0), T::TEXT);
        let r = Rect::from_center_size(p, g.size() + Vec2::new(8.0, 2.0));
        painter.rect(
            r,
            CornerRadius::same(4),
            T::CARD,
            Stroke::new(1.0_f32, color),
            StrokeKind::Inside,
        );
        painter.galley(r.center() - g.size() * 0.5, g, T::TEXT);
    }

    // Clicks: select, double-click to drill, portals open their level.
    if resp.double_clicked() {
        if let Some(i) = hovered_idx
            && let Some(level) = scene.nodes[i].drill.clone()
        {
            out.drill = Some((level, pos[i]));
        }
    } else if resp.clicked() {
        match hover_target {
            Some(Target::Node(i)) => st.selected = Some(scene.nodes[i].id.clone()),
            Some(Target::Portal(j)) => {
                if let Some(level) = scene.portals[j].level.clone() {
                    out.drill = Some((level, ppos[j]));
                }
            }
            None => st.selected = None,
        }
    }
    if hover_target.is_some() {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    } else if resp.dragged() {
        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
    }

    // Overlays.
    let title_pos = rect.left_top() + Vec2::new(16.0, 14.0);
    painter.text(
        title_pos,
        Align2::LEFT_TOP,
        &scene.title,
        T::strong_font(15.0),
        T::TEXT,
    );
    painter.text(
        title_pos + Vec2::new(0.0, 21.0),
        Align2::LEFT_TOP,
        &scene.hint,
        T::ui_font(12.0),
        T::TEXT_3,
    );

    let mut coverage = scene.coverage.clone();
    if scene.bundle_groups {
        coverage.push_str(&if bundled {
            format!(
                " · cross-group edges bundled into {bundle_count} bundles at this zoom (zoom in to see each)"
            )
        } else {
            " · every edge drawn".to_string()
        });
    }
    let cov_rect = Rect::from_min_max(
        Pos2::new(rect.left() + 16.0, rect.bottom() - 26.0),
        Pos2::new(rect.right() - 210.0, rect.bottom() - 8.0),
    );
    let g = painter.layout(
        coverage,
        T::mono_font(11.0),
        T::TEXT_3,
        cov_rect.width().max(50.0),
    );
    painter.galley(
        Pos2::new(cov_rect.left(), cov_rect.bottom() - g.size().y),
        g,
        T::TEXT_3,
    );

    // Zoom controls.
    let zr = Rect::from_min_size(
        Pos2::new(rect.right() - 200.0, rect.top() + 12.0),
        Vec2::new(188.0, 28.0),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(zr), |ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if ui
                .add(T::button("Fit"))
                .on_hover_text("Fit the graph to the view")
                .clicked()
            {
                st.fit();
            }
            if ui
                .add(T::button("+").min_size(Vec2::splat(28.0)))
                .on_hover_text("Zoom in")
                .clicked()
            {
                st.zoom = (st.zoom * 1.25).min(6.0);
            }
            if ui
                .add(T::button("−").min_size(Vec2::splat(28.0)))
                .on_hover_text("Zoom out")
                .clicked()
            {
                st.zoom = (st.zoom / 1.25).max(0.4);
            }
            let tier = if n_nodes <= LABEL_ALL_UP_TO || limit == usize::MAX {
                "all labels"
            } else if limit == 12 {
                "hub labels"
            } else {
                "top 40 labels"
            };
            ui.label(T::mono(format!("{:.0}% · {tier}", st.zoom * 100.0), 11.0).color(T::TEXT_3));
        });
    });

    minimap(ui, rect, scene, st, &cam);

    if st.show_trail && !trail.is_empty() {
        trail_player(ui, rect, st, trail, scene);
    }

    if st.transition.is_some() || dim > 0.0 && dim < 1.0 || st.playing {
        ctx.request_repaint();
    }
    out
}

fn portal_text(p: &Portal) -> String {
    format!("{} · {}", truncate_tail(&p.label, 28), fmt_count(p.count))
}

fn minimap(ui: &mut egui::Ui, rect: Rect, scene: &Scene, st: &mut GraphViewState, cam: &Cam) {
    let mm = Rect::from_min_size(
        Pos2::new(rect.right() - 192.0, rect.bottom() - 134.0),
        Vec2::new(180.0, 122.0),
    );
    let painter = ui.painter_at(mm);
    painter.rect(
        mm,
        CornerRadius::same(T::R_CARD),
        T::PANEL,
        Stroke::new(1.0_f32, T::CONTROL),
        StrokeKind::Inside,
    );
    let b = scene.bounds;
    let inner = mm.shrink(8.0);
    let s = (inner.width() / b.width().max(1.0)).min(inner.height() / b.height().max(1.0));
    let to_mm = |w: Pos2| inner.center() + (w - b.center()) * s;
    for (i, p) in scene.pos.iter().enumerate() {
        painter.circle_filled(
            to_mm(*p),
            (scene.radius[i] * s).clamp(1.2, 4.0),
            scene.nodes[i].color,
        );
    }
    for (j, p) in scene.portal_pos.iter().enumerate() {
        painter.circle_filled(to_mm(*p), 2.0, scene.portals[j].color);
    }
    let view = Rect::from_min_max(
        cam.to_world(rect.left_top()),
        cam.to_world(rect.right_bottom()),
    );
    let vr = Rect::from_min_max(to_mm(view.min), to_mm(view.max)).intersect(mm);
    painter.rect(
        vr,
        CornerRadius::same(2),
        T::ACCENT_SOFT,
        Stroke::new(1.0_f32, T::ACCENT),
        StrokeKind::Inside,
    );
    let resp = ui.interact(mm, ui.id().with("tt-minimap"), Sense::click_and_drag());
    if (resp.clicked() || resp.dragged())
        && let Some(p) = resp.interact_pointer_pos()
    {
        // Centre the main view on the clicked world point.
        let world = b.center() + (p - inner.center()) / s;
        st.pan -= cam.to_screen(world) - view_center(rect);
    }
}

fn trail_player(
    ui: &mut egui::Ui,
    rect: Rect,
    st: &mut GraphViewState,
    trail: &[ActivityStep],
    scene: &Scene,
) {
    let n = trail.len();
    st.trail_step = st.trail_step.min(n);
    if st.playing {
        let now = ui.input(|i| i.time);
        if now - st.last_step_at >= 0.7 {
            st.last_step_at = now;
            if st.trail_step >= n {
                st.playing = false;
            } else {
                st.trail_step += 1;
            }
        }
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
    }
    let w = (rect.width() - 240.0).clamp(420.0, 640.0);
    let pr = Rect::from_min_size(
        Pos2::new(rect.center().x - w * 0.5, rect.bottom() - 100.0),
        Vec2::new(w, 54.0),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(pr), |ui| {
        egui::Frame::NONE
            .fill(T::CARD)
            .stroke(Stroke::new(1.0_f32, T::CONTROL))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(w - 24.0);
                ui.horizontal(|ui| {
                    let glyph = if st.playing { "❚❚" } else { "▶" };
                    let b = egui::Button::new(egui::RichText::new(glyph).color(T::HEAT).size(12.0))
                        .fill(Color32::TRANSPARENT)
                        .stroke(Stroke::new(1.0_f32, with_alpha(T::HEAT, 0.5)))
                        .corner_radius(CornerRadius::same(T::R_CONTROL))
                        .min_size(Vec2::splat(30.0));
                    let label = if st.playing {
                        "Pause the replay"
                    } else {
                        "Replay the agent's path"
                    };
                    if ui.add(b).on_hover_text(label).clicked() {
                        if st.playing {
                            st.playing = false;
                        } else {
                            if st.trail_step >= n {
                                st.trail_step = 0;
                            }
                            st.playing = true;
                            st.last_step_at = ui.input(|i| i.time);
                        }
                    }
                    let mut step = st.trail_step;
                    ui.spacing_mut().slider_width = 120.0;
                    ui.visuals_mut().widgets.inactive.bg_fill = T::CONTROL_STRONG;
                    ui.visuals_mut().selection.bg_fill = T::HEAT;
                    if ui
                        .add(
                            egui::Slider::new(&mut step, 0..=n)
                                .show_value(false)
                                .trailing_fill(true),
                        )
                        .on_hover_text("Trail step")
                        .changed()
                    {
                        st.trail_step = step;
                        st.playing = false;
                    }
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        let line = if st.trail_step == 0 {
                            format!("step 0/{n}")
                        } else {
                            let s = &trail[st.trail_step - 1];
                            let placed = scene.locate_step(s).is_some();
                            format!(
                                "{}/{n}  {}  {}  {}{}",
                                st.trail_step,
                                short_time(&s.timestamp),
                                s.tool,
                                truncate_tail(&s.query, 34),
                                if placed { "" } else { "  (not on this level)" }
                            )
                        };
                        ui.label(T::mono(line, 11.0).color(T::TEXT));
                        let first = trail
                            .first()
                            .map(|s| short_time(&s.timestamp))
                            .unwrap_or_default();
                        let last = trail
                            .last()
                            .map(|s| short_time(&s.timestamp))
                            .unwrap_or_default();
                        ui.label(
                            egui::RichText::new(format!(
                                "Latest agent session {first}–{last} · {n} calls"
                            ))
                            .size(11.0)
                            .color(T::TEXT_3),
                        );
                    });
                });
            });
    });
}

/// `HH:MM` in local time from an RFC 3339 timestamp; the raw text when it does not parse.
pub fn short_time(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_else(|_| ts.chars().take(16).collect())
}

pub fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Keeps the distinctive end of a label (`...parser/mod.rs`).
pub fn truncate_tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let tail: String = s.chars().skip(count - max.saturating_sub(1)).collect();
        format!("…{tail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_deterministic_and_separates_nodes() {
        let radius = vec![10.0; 30];
        let group: Vec<usize> = (0..30).map(|i| i % 3).collect();
        let links: Vec<(usize, usize, f32)> = (1..30).map(|i| (i, i - 1, 1.0)).collect();
        let a = force_layout(&radius, &group, &links, 42);
        let b = force_layout(&radius, &group, &links, 42);
        assert_eq!(a, b);
        assert!(a.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
        let mut min_d = f32::MAX;
        for i in 0..a.len() {
            for j in i + 1..a.len() {
                min_d = min_d.min(a[i].distance(a[j]));
            }
        }
        assert!(min_d > 10.0, "nodes overlap: closest pair {min_d}");
    }

    #[test]
    fn counts_are_grouped_and_tails_kept() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(1234567), "1,234,567");
        assert_eq!(truncate_tail("src/parser/mod.rs", 10), "…er/mod.rs");
        assert_eq!(truncate_tail("a.rs", 10), "a.rs");
    }
}
