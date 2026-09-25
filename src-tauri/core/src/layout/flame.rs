//! Flame (spec §7 mode 4): "Depth top to bottom, size left to right".
//!
//! Each row is a depth level; each block sits beneath its parent's span;
//! blocks under 1 px wide are skipped. Cell geometry is a rect
//! `[x, y, w, h]` with the row height `H / depth`.

use crate::error::CoreError;
use crate::layout::{
    check_geometry, effective_branch_root, node_color, pack_rgba, Cell, ColorMode, LayoutBuffer,
    LayoutMeta,
};
use crate::scan::node::Tree;

/// Minimum block width in px (spec: "Skip blocks under 1px wide").
pub(crate) const MIN_W: f32 = 1.0;
/// Horizontal gap between sibling blocks.
pub(crate) const GAP_X: f32 = 0.5;
/// Blocks narrower than this sit flush (no gap) — the fine texture of
/// many small files renders solid instead of striped (picket-fence fix).
pub(crate) const GAP_MIN_W: f32 = 3.0;

/// Layout the subtree under `node` as a flame/icicle chart.
///
/// # Errors
/// - [`CoreError::InvalidGeometry`] when `width`/`height` are zero.
/// - [`CoreError::NodeNotFound`] when `node` is not in the arena.
#[allow(clippy::too_many_arguments)]
pub fn flame(
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
    let row_h = if depth > 0 {
        height / depth as f32
    } else {
        height
    };
    let mut cells: Vec<Cell> = Vec::with_capacity(512);
    let mut truncated = false;
    // By-folder families attach at the effective branch root: descend
    // single-sizeable-child chains ("This PC" → "C:") so C:'s children
    // become the top-level branches (spec §7 color modes).
    let branch_root = effective_branch_root(tree, node);
    // The root block spans the full width on row 0 when depth >= 1.
    if depth > 0 && total > 0 {
        cells.push(Cell::rect(
            node,
            0,
            pack_rgba(crate::layout::ANCHOR_GRAY),
            0.0,
            0.0,
            width,
            row_h,
        ));
        layout_row(
            tree,
            node,
            0.0,
            width,
            1,
            depth,
            row_h,
            color,
            now,
            &mut cells,
            &mut truncated,
            0,
            branch_root,
        );
    }
    Ok(LayoutBuffer {
        cells,
        meta: LayoutMeta {
            mode: "flame".into(),
            generation: tree.generation,
            node,
            width,
            height,
            depth,
            color_mode: color,
            cell_count: 0,
            truncated,
            center: None,
            groups: Vec::new(),
            total_bytes: total,
        },
    })
}

/// Recursive row layout: children of `node` inside x-span `(x0..x1)` on
/// row `depth_here`, each beneath its parent's span. `top_index` is the
/// inherited by-folder family; `branch_root`'s children re-assign it.
#[allow(clippy::too_many_arguments)]
fn layout_row(
    tree: &Tree,
    node: u32,
    x0: f32,
    x1: f32,
    depth_here: u32,
    depth_left: u32,
    row_h: f32,
    color: ColorMode,
    now: i64,
    cells: &mut Vec<Cell>,
    truncated: &mut bool,
    top_index: usize,
    branch_root: u32,
) {
    if depth_left == 0 {
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
    let y = (depth_here - 1) as f32 * row_h;
    let span = x1 - x0;
    // Pre-pass: kept children (share of `total`, ≥ MIN_W) with widths.
    // Gaps are only inserted between adjacent WIDE blocks (≥ GAP_MIN_W):
    // the fine "picket fence" of narrow file blocks renders flush (solid
    // texture) instead of striped by half-pixel gutters — with hundreds
    // of siblings the old GAP_X-per-pair burned ~100 px of span and
    // amplified the comb effect (VLM: "picket fence noise").
    let kept: Vec<(u32, f32)> = children
        .iter()
        .filter_map(|&id| {
            let c = tree.node(id)?;
            if c.is_removed() || c.on_disk == 0 {
                return None;
            }
            let w = c.on_disk as f32 / total as f32 * span;
            (w >= MIN_W).then_some((id, w))
        })
        .collect();
    if kept.is_empty() {
        return;
    }
    let gaps: Vec<f32> = kept
        .windows(2)
        .map(|p| {
            if p[0].1 >= GAP_MIN_W && p[1].1 >= GAP_MIN_W {
                GAP_X
            } else {
                0.0
            }
        })
        .collect();
    let gap_total: f32 = gaps.iter().sum();
    // Rescale kept widths to the gap-adjusted usable span (ratios among
    // drawn blocks stay exact).
    let kept_total: f32 = kept.iter().map(|k| k.1).sum();
    let usable = (span - gap_total).max(0.0);
    let scale = if kept_total > 0.0 {
        usable / kept_total
    } else {
        0.0
    };
    let mut cursor = x0;
    for (slot, &(id, w_raw)) in kept.iter().enumerate() {
        if crate::layout::over_budget(cells, truncated) {
            return;
        }
        let c = tree.node(id).expect("kept child id");
        let w = w_raw * scale;
        if w < MIN_W {
            continue; // Rounding edge after rescale.
        }
        // One pastel family per effective top-level branch, inherited by
        // every descendant (shade still varies by depth + sibling index).
        let fam = crate::layout::family_of(node, branch_root, slot, top_index);
        let rgba = pack_rgba(match color {
            ColorMode::ByFolder => node_color(tree, id, color, now, fam, depth_here as u16, slot),
            ColorMode::ByType => c.category().color(),
            ColorMode::ByAge => node_color(tree, id, color, now, 0, 0, slot),
        });
        cells.push(Cell::rect(id, depth_here as u16, rgba, cursor, y, w, row_h));
        if c.is_dir() && c.child_count > 0 && depth_left > 1 {
            layout_row(
                tree,
                id,
                cursor,
                cursor + w,
                depth_here + 1,
                depth_left - 1,
                row_h,
                color,
                now,
                cells,
                truncated,
                fam,
                branch_root,
            );
        }
        cursor += w + gaps.get(slot).copied().unwrap_or(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::node::{BatchEntry, Node, Tree};
    use crate::scan::rollup;

    fn build() -> Tree {
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\F");
        t.append_batch(
            0,
            vec![
                dir("a"),
                file("f1.bin", 100, 100, 1),
                file("f2.bin", 50, 50, 1),
            ],
        );
        t.append_batch(1, vec![file("a1", 60, 60, 1), file("a2", 30, 30, 1)]);
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
    fn blocks_align_under_parents() {
        let t = build();
        let buf = flame(&t, 0, 1000.0, 300.0, 3, ColorMode::ByType, 1).unwrap();
        let row1: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 1).collect();
        assert_eq!(row1.len(), 3);
        // Sizes: a=90, f1=100, f2=50 → widths 375, 416.67, 208.3.
        let wa = row1.iter().find(|c| c.id == 1).unwrap().g[2];
        let wf1 = row1.iter().find(|c| c.id == 2).unwrap().g[2];
        assert!((wa / wf1 - 0.9).abs() < 0.01);
        // Children of `a` sit within a's x-span on row 2.
        let a_x = row1.iter().find(|c| c.id == 1).unwrap().g[0];
        let a_w = row1.iter().find(|c| c.id == 1).unwrap().g[2];
        let row2: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 2).collect();
        for c in row2 {
            assert!(c.g[0] >= a_x - 0.5 && c.g[0] + c.g[2] <= a_x + a_w + 0.5);
            assert!((c.g[1] - 100.0).abs() < 0.5); // row 2 y
        }
        // No sub-1px blocks.
        assert!(buf.cells.iter().all(|c| c.g[2] >= MIN_W - f32::EPSILON));
    }

    #[test]
    fn narrow_siblings_sit_flush_no_picket_fence_gaps() {
        // Picket-fence regression: many narrow file blocks must render
        // flush (no half-pixel gutters between them); gaps only appear
        // between WIDE blocks. Also verifies the row still fills its
        // span (kept blocks rescaled to the gap-adjusted usable span).
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\P");
        let mut batch = vec![dir("many")];
        for i in 0..80 {
            batch.push(file(&format!("f{i:03}.bin"), 10, 10, 1));
        }
        t.append_batch(0, batch);
        t.append_batch(1, vec![file("big", 4000, 4000, 1)]);
        rollup::finalize(&mut t);
        let buf = flame(&t, 0, 1000.0, 300.0, 2, ColorMode::ByType, 1).unwrap();
        let row1: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 1).collect();
        // The narrow files (10/4800 share ≈ 2.1 px < GAP_MIN_W) sit flush:
        // consecutive narrow blocks touch (next.x == prev.x + prev.w).
        let mut narrow_pairs = 0;
        for w in row1.windows(2) {
            let (a, b) = (w[0], w[1]);
            if a.g[2] < GAP_MIN_W && b.g[2] < GAP_MIN_W {
                narrow_pairs += 1;
                assert!(
                    (b.g[0] - (a.g[0] + a.g[2])).abs() < 0.05,
                    "narrow blocks must sit flush: a ends {} b starts {}",
                    a.g[0] + a.g[2],
                    b.g[0]
                );
            }
        }
        assert!(narrow_pairs >= 10, "expected many flush narrow pairs");
        // The row fills the span: last block's right edge ≈ width (the
        // wide dir block takes the gap-adjusted remainder).
        let right = row1.iter().map(|c| c.g[0] + c.g[2]).fold(0.0f32, f32::max);
        assert!(
            (1000.0 - right).abs() <= 1.0,
            "row must fill the span, right edge {right}"
        );
    }
}
