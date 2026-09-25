//! Mind Map (spec §7 mode 6): "Branches from the root, sized by weight".
//!
//! A radial tree with curved links (JS draws a quadratic curve from each
//! dot to its parent dot position, stored in the cell); dot area ∝ share
//! of the folder. Labels are placed JS-side, biggest-first, skipping
//! collisions.

use crate::error::CoreError;
use crate::layout::{
    check_geometry, depth_below, effective_branch_root, node_color, pack_rgba, Cell, ColorMode,
    LayoutBuffer, LayoutMeta,
};
use crate::scan::node::Tree;

/// Minimum dot radius.
pub(crate) const MIN_R: f32 = 1.5;
/// Base dot radius at the root's children (scales with viewport).
pub(crate) const DOT_BASE: f32 = 26.0;
/// Root hub dot radius (label-gate eligible: the JS names the root).
pub(crate) const ROOT_DOT_R: f32 = 14.0;
/// Alpha for top-level dots — the root chain plus the effective
/// top-level branches: solid, matching the reference's bold branch dots.
const ALPHA_TOP: u32 = 0xFF;
/// Alpha for nested child dots (slight translucency so the hierarchy
/// reads and links/labels stay legible).
const ALPHA_NESTED: u32 = 0xCC;

/// Layout the subtree under `node` as a radial mind map.
///
/// # Errors
/// - [`CoreError::InvalidGeometry`] when `width`/`height` are zero.
/// - [`CoreError::NodeNotFound`] when `node` is not in the arena.
#[allow(clippy::too_many_arguments)]
pub fn mindmap(
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
    let cx = width / 2.0;
    let cy = height / 2.0;
    // Reserve the largest possible dot + air so dots and their labels
    // never clip the canvas edge (the deepest ring sits AT r_max; with
    // only a 6 px margin, 20-26 px dots at the 12-o'clock start angle
    // rendered half-off-canvas — VLM audit: "labels clipped by the
    // container"). Floor keeps tiny windows usable.
    let r_max = (width.min(height) / 2.0 - DOT_BASE - 8.0).max(48.0);
    let mut cells: Vec<Cell> = Vec::with_capacity(512);
    let mut truncated = false;
    // By-folder families attach at the effective branch root (descend
    // single-sizeable-child chains like "This PC" → "C:"); dots at or
    // above that level form the solid "top-level" alpha tier.
    let branch_root = effective_branch_root(tree, node);
    let branch_level = depth_below(tree, branch_root, node) + 1;
    // Root dot. 14 px (not 12) so the JS label gate (r ≥ 13) names the
    // root — the map reads as anchored on "This PC", not an anonymous
    // gray blob.
    cells.push(Cell::dot(
        node,
        0,
        pack_rgba(crate::layout::ANCHOR_GRAY),
        cx,
        cy,
        ROOT_DOT_R,
        cx,
        cy,
    ));
    if depth > 0 && total > 0 {
        layout_branches(
            tree,
            node,
            cx,
            cy,
            r_max,
            1,
            depth,
            total as f32,
            -std::f32::consts::FRAC_PI_2,
            -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU,
            color,
            now,
            &mut cells,
            &mut truncated,
            0,
            branch_root,
            branch_level,
        );
    }
    Ok(LayoutBuffer {
        cells,
        meta: LayoutMeta {
            mode: "mindmap".into(),
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

/// Place the children of `node` on the ring at radius `ring_r`, within
/// the inherited angular sector `[a0, a1)` — spans ∝ weights, and every
/// descendant stays inside its ancestor's wedge (children used to start
/// at 12 o'clock regardless of the parent's direction, letting deep
/// dots cross back over the root hub). `top_index` is the inherited
/// by-folder family; `branch_root`'s children re-assign it. Dot radii ∝
/// sqrt(share of the ROOT total) — share-of-parent let a 99 %-of-parent
/// child of a small branch render 4× its parent's size, floating over
/// the root hub (dwarfed hierarchy inversions).
#[allow(clippy::too_many_arguments)]
fn layout_branches(
    tree: &Tree,
    node: u32,
    cx: f32,
    cy: f32,
    ring_r: f32,
    depth_here: u32,
    depth_left: u32,
    root_total: f32,
    a0: f32,
    a1: f32,
    color: ColorMode,
    now: i64,
    cells: &mut Vec<Cell>,
    truncated: &mut bool,
    top_index: usize,
    branch_root: u32,
    branch_level: u32,
) {
    if depth_left == 0 || ring_r <= 4.0 {
        return;
    }
    let children = tree.children_sorted(node);
    let total: u64 = children
        .iter()
        .map(|&id| tree.node(id).map_or(0, |c| c.on_disk))
        .sum();
    if total == 0 {
        return;
    }
    // Single sizeable child → collapse onto the parent position: a
    // folder with one sizeable child is visually just that child. The
    // old full-TAU span put such a child at exactly 6 o'clock, hanging
    // the whole map below center at single-drive roots (VLM: "crammed
    // into the lower-central portion"). The chain keeps the full ring
    // budget for the real branches below it.
    let sizeable = children
        .iter()
        .filter(|&&id| tree.node(id).map_or(0, |c| c.on_disk) > 0)
        .count();
    let collapsed = sizeable == 1;
    let step_r = ring_r / depth_left as f32; // per-level radius step
    let level_r = ring_r - step_r * (depth_left as f32 - 1.0);
    let mut cursor = a0; // start at the sector's leading edge
    for (i, &id) in children.iter().enumerate() {
        if crate::layout::over_budget(cells, truncated) {
            return;
        }
        let c = tree.node(id).expect("child id");
        if c.is_removed() || c.on_disk == 0 {
            continue;
        }
        let span = c.on_disk as f32 / total as f32 * (a1 - a0);
        let mid = cursor + span / 2.0;
        let x = if collapsed {
            cx
        } else {
            cx + level_r * mid.cos()
        };
        let y = if collapsed {
            cy
        } else {
            cy + level_r * mid.sin()
        };
        // Dot radius: see the signature note — share of the ROOT keeps
        // every dot's area comparable across the map and monotone down
        // every chain. The cap scales with the ring step (≈ r_max/depth)
        // so small canvases don't blob adjacent levels together; ring-1
        // dots also clear the root hub (largest child vs hub overlap).
        let mut cap = (step_r * 0.8).clamp(10.0, DOT_BASE);
        if depth_here == 1 {
            cap = cap.min((level_r - ROOT_DOT_R - 2.0).max(6.0));
        }
        // Radius ∝ sqrt(share of the ROOT) — area comparable across the
        // whole map and monotone along every chain (child ≤ parent).
        let r = (cap * (c.on_disk as f32 / root_total).sqrt()).max(MIN_R);
        // Visibility floor: sub-2.5 px dots are invisible specks that
        // only add overplotting noise (VLM: "too many micro-dots").
        // Culling sets `truncated` so the UI's "showing top N" hint
        // stays honest.
        if r < 2.5 {
            *truncated = true;
            cursor += span;
            continue;
        }
        // One pastel family per effective top-level branch, inherited by
        // every descendant (shade still varies by depth + sibling index).
        let fam = crate::layout::family_of(node, branch_root, i, top_index);
        let rgb = match color {
            ColorMode::ByFolder => node_color(tree, id, color, now, fam, depth_here as u16, i),
            ColorMode::ByType => c.category().color(),
            ColorMode::ByAge => node_color(tree, id, color, now, 0, 0, i),
        };
        // Top-level dots (root chain + branches) stay solid; nested child
        // dots get the slightly translucent tier.
        let alpha = if depth_here <= branch_level {
            ALPHA_TOP
        } else {
            ALPHA_NESTED
        };
        let rgba = (rgb << 8) | alpha;
        cells.push(Cell::dot(id, depth_here as u16, rgba, x, y, r, cx, cy));
        if c.is_dir() && c.child_count > 0 && depth_left > 1 {
            layout_branches(
                tree,
                id,
                x,
                y,
                // The child's annulus is the OUTER remainder of ours —
                // this level consumed `step_r`. Passing `step_r` (the
                // pre-fix bug) shrank each level geometrically
                // (r_max/depth → /(depth-1) → …), collapsing the whole
                // map into a concentric blob around the root and
                // cutting every level past ~4 via the `ring_r <= 4`
                // guard at the default depth 7. A collapsed chain node
                // consumed no ring, so it passes the budget through.
                if collapsed { ring_r } else { ring_r - step_r },
                depth_here + 1,
                depth_left - 1,
                root_total,
                cursor,
                cursor + span,
                color,
                now,
                cells,
                truncated,
                fam,
                branch_root,
                branch_level,
            );
        }
        cursor += span;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::node::{BatchEntry, Node, Tree};
    use crate::scan::rollup;

    fn build() -> Tree {
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\M");
        t.append_batch(0, vec![dir("a"), dir("b")]);
        t.append_batch(1, vec![file("a1", 75, 75, 1), file("a2", 25, 25, 1)]);
        t.append_batch(2, vec![file("b1", 30, 30, 1)]);
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
    fn dots_ring_around_root_with_weight_spans() {
        let t = build();
        let buf = mindmap(&t, 0, 900.0, 700.0, 3, ColorMode::ByType, 1).unwrap();
        let l1: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 1).collect();
        assert_eq!(l1.len(), 2);
        // a=100, b=30 → angular span ratio 100/30.
        let (a, b) = (l1[0], l1[1]);
        // Positions radiate from center; parent link points at the center.
        for c in &l1 {
            let d = ((c.g[0] - 450.0).powi(2) + (c.g[1] - 350.0).powi(2)).sqrt();
            assert!(d > 40.0, "level-1 dot should sit off-center");
            assert!(
                (c.g[3] - 450.0).abs() < 0.01 && (c.g[4] - 350.0).abs() < 0.01,
                "dot parent link should point at the root center"
            );
        }
        // Dot radii ∝ sqrt(share): share a=0.769, b=0.231.
        let ra = a.g[2] / DOT_BASE;
        let rb = b.g[2] / DOT_BASE;
        assert!((ra * ra - 100.0 / 130.0).abs() < 0.01);
        assert!((rb * rb - 30.0 / 130.0).abs() < 0.01);
    }

    #[test]
    fn invalid_geometry_rejected() {
        let t = build();
        assert!(mindmap(&t, 0, 10.0, 0.0, 2, ColorMode::ByAge, 1).is_err());
    }

    #[test]
    fn dots_and_labels_never_clip_the_canvas_bounds() {
        // Margin regression: the deepest ring sits AT r_max, so the
        // canvas edge must reserve the largest possible dot radius —
        // every emitted dot (x, y ± r) must stay inside the canvas.
        let t = build_single_drive();
        let w = 900.0f32;
        let h = 700.0f32;
        let buf = mindmap(&t, 0, w, h, 3, ColorMode::ByFolder, 1).unwrap();
        assert!(buf.cells.len() > 4, "tree must emit dots");
        for c in &buf.cells {
            if (c.flags & 0b111) != crate::layout::cell_kind::DOT {
                continue;
            }
            let (dot_x, dot_y, dot_r) = (c.g[0], c.g[1], c.g[2]);
            assert!(
                dot_x - dot_r >= -0.5
                    && dot_y - dot_r >= -0.5
                    && dot_x + dot_r <= w + 0.5
                    && dot_y + dot_r <= h + 0.5,
                "dot clips the canvas: ({dot_x},{dot_y}) r={dot_r} in {w}x{h}"
            );
        }
    }

    /// "This PC" → single "C:" drive → 6 folders with distinct sizes
    /// (each holding one file) — the shape that collapsed the whole
    /// mind map into one pastel family.
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
    fn single_child_root_assigns_branch_families_and_nested_alpha() {
        let t = build_single_drive();
        let buf = mindmap(&t, 0, 900.0, 700.0, 3, ColorMode::ByFolder, 1).unwrap();
        // Depth-2 dots = C:'s children (the effective top-level branches):
        // distinct pastel families, solid top-level alpha.
        let branch: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 2).collect();
        assert!(branch.len() >= 3, "all branch dots must emit");
        let mut rgb: Vec<u32> = branch.iter().map(|c| c.rgba >> 8).collect();
        rgb.sort_unstable();
        rgb.dedup();
        assert!(rgb.len() >= 3, "branch dots must span >= 3 pastel families");
        assert!(
            branch.iter().all(|c| c.rgba & 0xFF == ALPHA_TOP),
            "top-level dots stay solid"
        );
        // Nested child dots (files inside the branches) inherit their
        // branch family and use the translucent nested tier.
        let nested: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 3).collect();
        assert!(nested.len() >= 3, "nested dots must emit");
        let mut rgb: Vec<u32> = nested.iter().map(|c| c.rgba >> 8).collect();
        rgb.sort_unstable();
        rgb.dedup();
        assert!(
            rgb.len() >= 3,
            "nested dots must inherit distinct branch families"
        );
        assert!(nested.iter().all(|c| c.rgba & 0xFF == ALPHA_NESTED));
    }

    /// "This PC" → "C:" → three folders, one holding a nested home
    /// dir with files — the shape that the pre-fix ring decay collapsed.
    fn build_deep_chain() -> Tree {
        let mut t = Tree::new(1);
        t.add_root_path(0, "This PC");
        t.append_batch(0, vec![dir("C:")]); // 1
        t.append_batch(1, vec![dir("Users"), dir("Win"), dir("Tools")]); // 2, 3, 4
        t.append_batch(2, vec![dir("me")]); // 5
        t.append_batch(
            5,
            vec![
                file("a.bin", 100, 100, 1),
                file("b.bin", 50, 50, 1),
                file("c.bin", 25, 25, 1),
            ], // 6..8
        );
        t.append_batch(3, vec![file("w.bin", 200, 200, 1)]); // 9
                                                             // Tools: a SMALL branch (20/395) whose single child holds ~all
                                                             // of it — the share-of-parent radius dwarfed the parent (child
                                                             // rendered 4× the parent's dot, floating over the root hub).
        t.append_batch(4, vec![dir("kit")]); // 10
        t.append_batch(10, vec![file("k.bin", 19, 19, 1)]); // 11
        rollup::finalize(&mut t);
        t
    }

    #[test]
    fn deep_levels_render_across_the_full_radius_at_default_depth() {
        // Regression: the recursion used to pass `step_r` as the child
        // ring radius, shrinking each level geometrically (r_max/depth,
        // then /(depth-1), …) — at the app's default request depth 7 the
        // whole map collapsed into a ~60 px concentric blob around the
        // root and every level past 3 vanished via the `ring_r <= 4`
        // guard.
        let t = build_deep_chain();
        let w = 900.0f32;
        let h = 700.0f32;
        let req_depth = 7u32;
        let buf = mindmap(&t, 0, w, h, req_depth, ColorMode::ByFolder, 1).unwrap();
        // Level 4+ must actually emit (pre-fix: nothing past depth 3 —
        // the ring decay hit the `ring_r <= 4` guard by level 4).
        assert!(
            buf.cells.iter().any(|c| c.depth >= 4),
            "deep levels must render at default depth 7"
        );
        let r_max = (w.min(h) / 2.0 - DOT_BASE - 8.0).max(48.0);
        let (cx, cy) = (w / 2.0, h / 2.0);
        // Every dot sits ONE ring step out from its parent (level_r ==
        // step_r algebraically); the pre-fix decay put level-2 dots
        // 7.6 px and level-3 dots 1.5 px from their parents — concentric.
        // Collapsed single-child chain dots sit AT their parent by
        // design (d ≈ 0) and are skipped.
        let min_step = r_max / req_depth as f32 * 0.75;
        for c in &buf.cells {
            if c.depth == 0 || (c.flags & 0b111) != crate::layout::cell_kind::DOT {
                continue;
            }
            let d = ((c.g[0] - c.g[3]).powi(2) + (c.g[1] - c.g[4]).powi(2)).sqrt();
            if d < 1.0 {
                continue; // collapsed chain dot
            }
            assert!(
                d >= min_step,
                "dot at depth {} sits {d:.1}px from its parent (min ring step {min_step:.1}px) — rings must advance outward one step per level",
                c.depth
            );
        }
        // And the map must span the canvas, not hug the root: the
        // farthest dot+radius reaches ≥ 40% of r_max (pre-fix ~19%).
        let reach = buf
            .cells
            .iter()
            .filter(|c| (c.flags & 0b111) == crate::layout::cell_kind::DOT)
            .map(|c| ((c.g[0] - cx).powi(2) + (c.g[1] - cy).powi(2)).sqrt() + c.g[2])
            .fold(0.0_f32, f32::max);
        assert!(
            reach >= r_max * 0.4,
            "map must span the canvas (reach={reach}, r_max={r_max})"
        );
        // Share-of-ROOT radius: no child dot may exceed its parent's
        // dot — the pre-fix share-of-parent let Tools' dominant child
        // "kit" (95% of Tools, but Tools is 5% of the disk) render at
        // ~26 px against Tools' ~6 px, floating over the root hub.
        let by_id: std::collections::HashMap<u32, f32> =
            buf.cells.iter().map(|c| (c.id, c.g[2])).collect();
        let tools_r = by_id[&4];
        let kit_r = by_id[&10];
        assert!(
            kit_r <= tools_r + 0.01,
            "child dot (kit {kit_r}px) must not exceed its parent (Tools {tools_r}px)"
        );
        let users_r = by_id[&2];
        let me_r = by_id[&5];
        assert!(
            me_r <= users_r + 0.01,
            "collapsed chain dot (me {me_r}px) must not exceed its parent (Users {users_r}px)"
        );
        // Hub clearance: with sector-inheriting recursion, no branching
        // dot may cross back over the root hub (pre-fix: deep dots
        // landed 7 px from center, on top of the hub, because children
        // always started at 12 o'clock instead of inside the parent's
        // wedge). Collapsed chain dots sit ON their parent by design.
        for c in &buf.cells {
            if c.depth == 0 || (c.flags & 0b111) != crate::layout::cell_kind::DOT {
                continue;
            }
            let from_parent = ((c.g[0] - c.g[3]).powi(2) + (c.g[1] - c.g[4]).powi(2)).sqrt();
            if from_parent < 1.0 {
                continue; // collapsed chain dot
            }
            let from_center = ((c.g[0] - cx).powi(2) + (c.g[1] - cy).powi(2)).sqrt();
            assert!(
                from_center >= ROOT_DOT_R + c.g[2] - 1.0,
                "dot at depth {} crosses the root hub (center-dist {:.1}, r {:.1})",
                c.depth,
                from_center,
                c.g[2]
            );
        }
    }
}
