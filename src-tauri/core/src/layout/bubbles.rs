//! Bubbles (spec §7 mode 5): "Nested bubbles, one per folder".
//!
//! Hierarchical circle packing: folders are translucent circles containing
//! their children; area is proportional to bytes. The algorithm is
//! two-phase, which keeps **sibling area ratios exact at every level**:
//!
//! 1. **Hierarchy phase** ([`build_bubble`]) collects the subtree
//!    (id, size, children) — no geometry.
//! 2. **Emission phase** ([`emit`]) works top-down: children of a node
//!    with drawn radius `R` get `r_i = sqrt(share_i) × (R − PAD)`
//!    (square root because AREA ∝ bytes), are ring-packed largest-first
//!    ([`ring_pack`]), and the whole level is uniformly shrunk when the
//!    pack needs more than the usable radius (Cauchy–Schwarz guarantees
//!    sibling sqrt-shares rarely fit unshrunk). Uniform scaling preserves
//!    every sibling ratio exactly, so "area ∝ bytes" holds per level;
//!    nesting is exact by construction (a level never exceeds its
//!    parent's usable radius after the shrink).

use crate::error::CoreError;
use crate::layout::{
    check_geometry, depth_below, effective_branch_root, node_color, Cell, ColorMode, LayoutBuffer,
    LayoutMeta,
};
use crate::scan::node::Tree;

/// Padding between a parent circle's content and its rim.
pub(crate) const PAD: f32 = 3.0;
/// Gap between sibling circles on a ring.
pub(crate) const GAP: f32 = 2.0;
/// Minimum radius to emit (sub-pixel bubbles skipped).
pub(crate) const MIN_R: f32 = 1.0;
/// Alpha for the primary tier — the root container, the single-child
/// chain and the effective top-level branches: the reference's soft
/// translucent fill so nested circles read through.
const ALPHA_PRIMARY: u32 = 0xB4;
/// Alpha for nested child bubbles (slightly more opaque so they separate
/// from the soft parent fill).
const ALPHA_NESTED: u32 = 0xD9;

/// One node in the bubble hierarchy (geometry is computed at emission).
#[derive(Debug, Clone)]
struct Bubble {
    id: u32,
    /// On-disk size in bytes (drives the area share).
    size: u64,
    children: Vec<Bubble>,
}

/// Layout the subtree under `node` as nested bubbles.
///
/// # Errors
/// - [`CoreError::InvalidGeometry`] when `width`/`height` are zero.
/// - [`CoreError::NodeNotFound`] when `node` is not in the arena.
#[allow(clippy::too_many_arguments)]
pub fn bubbles(
    tree: &Tree,
    node: u32,
    width: f32,
    height: f32,
    depth: u32,
    color: ColorMode,
    now: i64,
) -> Result<LayoutBuffer, CoreError> {
    check_geometry(width, height)?;
    let n = tree.node(node).ok_or(CoreError::NodeNotFound(node))?;
    let total = n.on_disk;
    let root_r = width.min(height) / 2.0 - 2.0;
    let root = build_bubble(tree, node, depth);
    // By-folder families attach at the effective branch root (descend
    // single-sizeable-child chains like "This PC" → "C:"); bubbles at
    // or above that level form the soft "primary" translucency tier.
    let branch_root = effective_branch_root(tree, node);
    let branch_level = depth_below(tree, branch_root, node) + 1;
    // Emit cells (root bubble centered).
    let cx = width / 2.0;
    let cy = height / 2.0;
    let mut cells: Vec<Cell> = Vec::with_capacity(512);
    let mut truncated = false;
    emit(
        tree,
        &root,
        cx,
        cy,
        root_r,
        0,
        color,
        now,
        &mut cells,
        &mut truncated,
        0,
        branch_root,
        branch_level,
    );
    Ok(LayoutBuffer {
        cells,
        meta: LayoutMeta {
            mode: "bubbles".into(),
            generation: tree.generation,
            node,
            width,
            height,
            depth,
            color_mode: color,
            cell_count: 0,
            truncated,
            center: Some((cx, cy)),
            groups: Vec::new(),
            total_bytes: total,
        },
    })
}

/// Collect the bubble hierarchy down to `depth_left` levels (no geometry).
fn build_bubble(tree: &Tree, id: u32, depth_left: u32) -> Bubble {
    let n = tree.node(id).expect("node id");
    let mut children = Vec::new();
    if depth_left > 0 && n.is_dir() && n.child_count > 0 {
        for &cid in tree.children_sorted(id) {
            let c = tree.node(cid).expect("child id");
            if c.on_disk == 0 || c.is_removed() {
                continue;
            }
            children.push(build_bubble(tree, cid, depth_left - 1));
        }
    }
    Bubble {
        id,
        size: n.on_disk,
        children,
    }
}

/// One allocated child during emission: `(child index, x, y, r)` with
/// positions relative to the parent's center.
type Placed = (usize, f32, f32, f32);

/// Emit the circle for `b` at `(cx, cy)` with radius `drawn_r`, then
/// allocate + pack + recursively emit its children inside. `top_index` is
/// the inherited by-folder family; `branch_root`'s children re-assign it.
#[allow(clippy::too_many_arguments)]
fn emit(
    tree: &Tree,
    b: &Bubble,
    cx: f32,
    cy: f32,
    drawn_r: f32,
    depth: u32,
    color: ColorMode,
    now: i64,
    cells: &mut Vec<Cell>,
    truncated: &mut bool,
    top_index: usize,
    branch_root: u32,
    branch_level: u32,
) {
    if crate::layout::over_budget(cells, truncated) {
        return;
    }
    if drawn_r < MIN_R {
        return;
    }
    let rgb = match color {
        ColorMode::ByFolder => {
            node_color(tree, b.id, color, now, top_index, depth as u16, top_index)
        }
        ColorMode::ByType => tree.dominant_category(b.id).color(),
        ColorMode::ByAge => node_color(tree, b.id, color, now, 0, 0, top_index),
    };
    // Primary tier: the root/chain containers and the effective top-level
    // branches (soft fill); deeper descendants are the nested tier.
    let alpha = if depth <= branch_level {
        ALPHA_PRIMARY
    } else {
        ALPHA_NESTED
    };
    let rgba = (rgb << 8) | alpha;
    cells.push(Cell::circle(b.id, depth as u16, rgba, cx, cy, drawn_r));

    // Allocate children by sqrt area share of the usable radius.
    let usable = (drawn_r - PAD).max(0.0);
    let total: u64 = b.children.iter().map(|c| c.size).sum();
    if total == 0 || usable < MIN_R {
        return;
    }
    let mut kids: Vec<Placed> = Vec::with_capacity(b.children.len());
    for (i, c) in b.children.iter().enumerate() {
        if c.size == 0 {
            continue;
        }
        let r = (c.size as f32 / total as f32).sqrt() * usable;
        if r < MIN_R {
            continue;
        }
        kids.push((i, 0.0, 0.0, r));
    }
    if kids.is_empty() {
        return;
    }
    ring_pack(&mut kids);
    let extent = |ks: &[Placed]| {
        ks.iter()
            .map(|&(_, x, y, r)| (x * x + y * y).sqrt() + r)
            .fold(0.0f32, f32::max)
    };
    let needed = extent(&kids);
    if needed > usable && needed > 0.0 {
        // Fill-fit: bisect the largest uniform scale whose ring pack
        // still fits `usable`. The old one-shot shrink (k = usable /
        // needed) under-filled the parent whenever the pack geometry
        // changed discontinuously with scale — visible as a gap between
        // the children and the parent rim (VLM: "massive unutilized
        // gaps"). The pack now ends tangent to the inner rim, and the
        // uniform scale keeps every sibling ratio exact (the
        // algorithm's core guarantee) with nesting still exact.
        let base: Vec<f32> = kids.iter().map(|k| k.3).collect();
        let pack_at = |ks: &mut Vec<Placed>, s: f32| {
            for (k, br) in ks.iter_mut().zip(base.iter()) {
                k.3 = s * br;
                k.1 = 0.0;
                k.2 = 0.0;
            }
            ring_pack(ks);
            extent(ks)
        };
        let mut lo = 0.0f32; // feasible (degenerate point)
        let mut hi = 1.0f32; // infeasible (extent(1.0) = needed > usable)
        for _ in 0..24 {
            let mid = 0.5 * (lo + hi);
            if pack_at(&mut kids, mid) <= usable {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let _ = pack_at(&mut kids, lo);
    }
    for (i, (child_idx, x, y, r)) in kids.iter().enumerate() {
        emit(
            tree,
            &b.children[*child_idx],
            cx + x,
            cy + y,
            *r,
            depth + 1,
            color,
            now,
            cells,
            truncated,
            crate::layout::family_of(b.id, branch_root, i, top_index),
            branch_root,
            branch_level,
        );
    }
}

/// Pack circles on concentric rings (largest first): the largest starts at
/// the center, subsequent circles go on rings outside the already-placed
/// extent; each ring holds children until the next would not fit
/// tangentially. Positions are relative to the parent's center; radii are
/// the allocated drawn radii.
fn ring_pack(kids: &mut [Placed]) {
    if kids.is_empty() {
        return;
    }
    // Largest first (sort by allocated radius).
    kids.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    let mut placed = 0usize;
    let mut ring_r = 0.0f32; // enclosing radius of everything placed so far
    while placed < kids.len() {
        let r1 = kids[placed].3;
        let ring_center_r = if ring_r == 0.0 { 0.0 } else { ring_r + r1 };
        if ring_center_r <= 0.0 {
            // Largest bubble sits at the center.
            kids[placed].1 = 0.0;
            kids[placed].2 = 0.0;
            ring_r = r1;
            placed += 1;
            continue;
        }
        // How many of the remaining fit on this ring, tangentially?
        let mut count = 1usize;
        let mut angle_used = 0.0f32;
        let mut i = placed + 1;
        while i < kids.len() {
            let r2 = kids[i].3;
            // Chord needed for two adjacent bubbles r1..r2 on the ring.
            let half = (r1 + r2 + GAP) / 2.0;
            let ratio = (half / ring_center_r).clamp(0.0, 1.0);
            // ratio >= 1 means they cannot share this ring even opposite.
            let theta = if ratio >= 1.0 {
                std::f32::consts::PI
            } else {
                2.0 * ratio.asin()
            };
            if angle_used + theta > std::f32::consts::TAU {
                break;
            }
            angle_used += theta;
            count += 1;
            i += 1;
        }
        // Place `count` bubbles evenly around the ring.
        let step = std::f32::consts::TAU / count as f32;
        let mut angle = 0.0f32;
        for kid in &mut kids[placed..placed + count] {
            kid.1 = ring_center_r * angle.cos();
            kid.2 = ring_center_r * angle.sin();
            angle += step;
        }
        ring_r = ring_center_r + kids[placed].3;
        placed += count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::node::{BatchEntry, Node, Tree};
    use crate::scan::rollup;

    fn build() -> Tree {
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\B");
        t.append_batch(0, vec![dir("a"), dir("b"), dir("c")]);
        t.append_batch(1, vec![file("a1", 60, 60, 1), file("a2", 30, 30, 1)]);
        t.append_batch(2, vec![file("b1", 40, 40, 1)]);
        t.append_batch(3, vec![file("c1", 10, 10, 1)]);
        rollup::finalize(&mut t);
        t
    }

    fn dir(name: &str) -> BatchEntry {
        let mut node = Node::new_dir();
        node.modified = 1;
        BatchEntry {
            name: name.encode_utf16().collect(),
            node,
        }
    }

    fn file(name: &str, logical: u64, on_disk: u64, modified: i64) -> BatchEntry {
        let mut node = Node::new_file();
        node.logical = logical;
        node.on_disk = on_disk;
        node.modified = modified;
        BatchEntry {
            name: name.encode_utf16().collect(),
            node,
        }
    }

    #[test]
    fn circles_nested_and_non_overlapping() {
        let t = build();
        let buf = bubbles(&t, 0, 800.0, 800.0, 3, ColorMode::ByType, 1).unwrap();
        assert!(!buf.cells.is_empty());
        // Every child circle sits inside its parent circle (root center
        // 400,400; level-1 circles inside root radius).
        let root = buf.cells.iter().find(|c| c.depth == 0).unwrap();
        for c in buf.cells.iter().filter(|c| c.depth == 1) {
            let d = ((c.g[0] - root.g[0]).powi(2) + (c.g[1] - root.g[1]).powi(2)).sqrt();
            assert!(d + c.g[2] <= root.g[2] + 0.5, "child outside root");
        }
        // Level-1 circles pairwise non-overlapping.
        let l1: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 1).collect();
        for i in 0..l1.len() {
            for j in (i + 1)..l1.len() {
                let (a, b) = (l1[i], l1[j]);
                let d = ((a.g[0] - b.g[0]).powi(2) + (a.g[1] - b.g[1]).powi(2)).sqrt();
                assert!(d >= a.g[2] + b.g[2] - 1.0, "sibling overlap");
            }
        }
    }

    #[test]
    fn areas_proportional_at_level() {
        let t = build();
        let buf = bubbles(&t, 0, 800.0, 800.0, 2, ColorMode::ByType, 1).unwrap();
        let l1: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 1).collect();
        // a=90, b=40, c=10 → areas ∝ 9:4:1.
        let area = |c: &Cell| c.g[2] * c.g[2];
        let find = |id: u32| l1.iter().find(|c| c.id == id).unwrap();
        let ratio_ab = area(find(1)) / area(find(2));
        assert!(
            (ratio_ab - 9.0 / 4.0).abs() < 0.6,
            "a/b area ratio {ratio_ab}"
        );
    }

    /// "This PC" → single "C:" drive → 6 folders with distinct sizes
    /// (each holding one file) — the shape that collapsed the whole
    /// bubble map into one pastel family.
    fn build_single_drive() -> Tree {
        let mut t = Tree::new(1);
        t.add_root_path(0, "This PC");
        t.append_batch(0, vec![dir("C:")]); // id 1
        t.append_batch(
            1,
            vec![
                dir("Users"),    // 2
                dir("Windows"),  // 3
                dir("Programs"), // 4
                dir("Data"),     // 5
                dir("Temp"),     // 6
                dir("Logs"),     // 7
            ],
        );
        for (id, size) in [
            (2u32, 600u64),
            (3, 500),
            (4, 400),
            (5, 300),
            (6, 200),
            (7, 100),
        ] {
            t.append_batch(id, vec![file("f.bin", size, size, 1)]);
        }
        rollup::finalize(&mut t);
        t
    }

    #[test]
    fn single_child_root_assigns_branch_families_and_translucency() {
        let t = build_single_drive();
        let buf = bubbles(&t, 0, 800.0, 800.0, 3, ColorMode::ByFolder, 1).unwrap();
        // Depth-2 bubbles = C:'s children (the effective top-level
        // branches): distinct pastel families in the soft primary tier.
        let branch: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 2).collect();
        assert!(branch.len() >= 3, "all branch bubbles must emit");
        let mut rgb: Vec<u32> = branch.iter().map(|c| c.rgba >> 8).collect();
        rgb.sort_unstable();
        rgb.dedup();
        assert!(
            rgb.len() >= 3,
            "branch bubbles must span >= 3 pastel families"
        );
        assert!(
            branch.iter().all(|c| c.rgba & 0xFF == ALPHA_PRIMARY),
            "branch bubbles are the soft primary tier"
        );
        // The root/chain containers stay in the primary tier too.
        for c in buf.cells.iter().filter(|c| c.depth <= 1) {
            assert_eq!(c.rgba & 0xFF, ALPHA_PRIMARY, "container stays primary");
        }
        // Nested child bubbles (files inside the branches) inherit their
        // branch family and use the more opaque nested tier.
        let nested: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 3).collect();
        assert!(nested.len() >= 3, "nested bubbles must emit");
        let mut rgb: Vec<u32> = nested.iter().map(|c| c.rgba >> 8).collect();
        rgb.sort_unstable();
        rgb.dedup();
        assert!(
            rgb.len() >= 3,
            "nested bubbles must inherit distinct branch families"
        );
        assert!(nested.iter().all(|c| c.rgba & 0xFF == ALPHA_NESTED));
    }

    #[test]
    fn fill_fit_packs_children_to_the_rim_no_loose_gap() {
        // Fill-fit regression: the old one-shot shrink left the level
        // floating loose inside the parent (a visible rim gap) whenever
        // the ring-pack extent changed discontinuously with scale. The
        // bisection must land the pack tangent to the usable radius.
        let t = build();
        let buf = bubbles(&t, 0, 800.0, 800.0, 2, ColorMode::ByType, 1).unwrap();
        // Root circle (depth 0) centered, radius = 400 - 2.
        let root = buf.cells.iter().find(|c| c.depth == 0).unwrap();
        let usable = root.g[2] - PAD;
        let kids: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 1).collect();
        assert!(!kids.is_empty());
        // Every child fully inside the parent's usable radius (nesting
        // exact) …
        for k in &kids {
            let d = ((k.g[0] - root.g[0]).powi(2) + (k.g[1] - root.g[1]).powi(2)).sqrt();
            assert!(
                d + k.g[2] <= usable + 0.75,
                "child exceeds the usable radius: d+r={} usable={}",
                d + k.g[2],
                usable
            );
        }
        // … and the LARGEST extent lands on the rim (fill, not loose):
        // the pack's outermost child reaches within a hair of usable.
        let extent: f32 = kids
            .iter()
            .map(|k| ((k.g[0] - root.g[0]).powi(2) + (k.g[1] - root.g[1]).powi(2)).sqrt() + k.g[2])
            .fold(0.0, f32::max);
        assert!(
            (usable - extent).abs() <= 1.0,
            "pack must fill the usable radius (extent {extent} vs usable {usable})"
        );
    }
}
