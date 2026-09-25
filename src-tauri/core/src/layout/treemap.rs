//! Squarified treemap (Bruls, Huizing & van Wijk 2000) — spec §7 mode 1.
//!
//! "Every file as a rectangle, sized by bytes": nested labeled groups with
//! a header strip down to the chosen depth; cells under ~6 px² are skipped
//! and the remainder stays blank so proportions stay honest.
//!
//! Algorithm reference: `WinDirStat` `TreeMapLayout.cpp` `ArrangeSquarified`
//! (annotated in `resources/windirstat-master/documentation/
//! 03_treemap_implementation.md`). `WinDirStat` is GPL-2 — REFERENCE ONLY;
//! this is an original implementation of the published algorithm.

use crate::error::CoreError;
use crate::layout::{
    check_geometry, depth_below, effective_branch_root, node_color, pack_rgba, rect_visible, Cell,
    ColorMode, GroupDesc, GroupTuple, LayoutBuffer, LayoutMeta, MAX_CELLS,
};
use crate::scan::node::Tree;

/// Header strip height when a folder rect can afford a label (px).
const HEADER_H: f32 = 14.0;
/// Minimum rect width to draw a header.
const HEADER_MIN_W: f32 = 42.0;
/// Minimum rect height to draw a header (header + at least one row).
const HEADER_MIN_H: f32 = 26.0;
/// Minimum folder rect area to recurse into (smaller → leaf cell).
const RECURSE_MIN_AREA: f32 = 48.0;

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// Layout the subtree under `node` as a squarified treemap.
///
/// `top_index` colors the root's top-level branches in By-folder mode.
///
/// # Errors
/// - [`CoreError::InvalidGeometry`] when `width`/`height` are zero.
/// - [`CoreError::NodeNotFound`] when `node` is not in the arena.
/// - [`CoreError::TooManyCells`] when the cell budget would be exceeded
///   (the caller must lower `depth`).
#[allow(clippy::too_many_arguments)]
pub fn treemap(
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
    let mut cells: Vec<Cell> = Vec::with_capacity(1024);
    let mut truncated = false;
    let root_rect = Rect {
        x: 0.0,
        y: 0.0,
        w: width,
        h: height,
    };
    // By-folder families attach at the effective branch root: descend
    // single-sizeable-child chains ("This PC" → "C:") so C:'s children
    // become the top-level branches (spec §7 color modes).
    let branch_root = effective_branch_root(tree, node);
    layout_children(
        tree,
        node,
        root_rect,
        depth,
        color,
        now,
        node,
        branch_root,
        0,
        &mut cells,
        &mut truncated,
    );
    Ok(LayoutBuffer {
        cells,
        meta: LayoutMeta {
            mode: "treemap".into(),
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

/// Recurse into `parent`'s children inside `rect`. `family` is the
/// inherited by-folder family; `branch_root`'s children re-assign it.
#[allow(clippy::too_many_arguments)]
fn layout_children(
    tree: &Tree,
    parent: u32,
    rect: Rect,
    depth_left: u32,
    color: ColorMode,
    now: i64,
    layout_root: u32,
    branch_root: u32,
    family: usize,
    cells: &mut Vec<Cell>,
    truncated: &mut bool,
) {
    if depth_left == 0 || !rect_visible(rect.w, rect.h) {
        return;
    }
    let children = tree.children_sorted(parent).to_vec();
    // Sizes for the squarify pass: skip zero-size children (degenerate).
    let ids: Vec<u32> = children
        .iter()
        .copied()
        .filter(|&id| tree.node(id).map_or(0, |n| n.on_disk) > 0)
        .collect();
    let sizes: Vec<u64> = ids
        .iter()
        .map(|&id| tree.node(id).map_or(0, |n| n.on_disk))
        .collect();
    let rects = squarify(&sizes, rect);
    // Squarify can DROP tail siblings when the remaining rect's f32
    // extent collapses to zero (a multi-GB sibling's row rounds to the
    // full extent; the next row's zero thickness yields an INF
    // worst-ratio and the walk stops). The dropped cells are genuinely
    // invisible, but the drop must not be silent — `truncated` is the
    // UI's only "not everything is drawn" signal (found by the proptest
    // area-proportionality property: 2 sizes, 1 cell, truncated=false).
    if rects.len() < ids.len() {
        *truncated = true;
    }
    let depth_here = depth_below(tree, parent, layout_root) + 1;
    for (i, (&id, r)) in ids.iter().zip(rects.iter()).enumerate() {
        if cells.len() >= MAX_CELLS {
            *truncated = true;
            return;
        }
        let node = tree.node(id).expect("id from children slice");
        if node.is_removed() {
            continue;
        }
        // One pastel family per effective top-level branch, inherited by
        // every descendant (shade still varies by depth + sibling index).
        let fam = if parent == branch_root { i } else { family };
        let rgba = pack_rgba(match color {
            ColorMode::ByFolder => node_color(tree, id, color, now, fam, depth_here as u16, i),
            ColorMode::ByType => node.category().color(),
            ColorMode::ByAge => node_color(tree, id, color, now, 0, 0, i),
        });
        if node.is_dir() {
            // Reserve a header strip when the rect can afford one.
            let (body, hdr) = if r.w >= HEADER_MIN_W && r.h >= HEADER_MIN_H {
                (
                    Rect {
                        x: r.x,
                        y: r.y + HEADER_H,
                        w: r.w,
                        h: r.h - HEADER_H,
                    },
                    Some(*r),
                )
            } else {
                (*r, None)
            };
            if let Some(h) = hdr {
                cells.push(Cell::header(
                    id,
                    depth_here as u16,
                    rgba,
                    h.x,
                    h.y,
                    h.w,
                    HEADER_H,
                ));
            }
            let recurse =
                depth_left > 1 && body.w * body.h >= RECURSE_MIN_AREA && node.child_count > 0;
            if recurse {
                layout_children(
                    tree,
                    id,
                    body,
                    depth_left - 1,
                    color,
                    now,
                    layout_root,
                    branch_root,
                    fam,
                    cells,
                    truncated,
                );
            } else if rect_visible(body.w, body.h) {
                cells.push(Cell::rect(
                    id,
                    depth_here as u16,
                    rgba,
                    body.x,
                    body.y,
                    body.w,
                    body.h,
                ));
            } else {
                // Sub-visible body dropped: flag it — never silent.
                *truncated = true;
            }
        } else if rect_visible(r.w, r.h) {
            cells.push(Cell::rect(id, depth_here as u16, rgba, r.x, r.y, r.w, r.h));
        } else {
            // Sub-visible file dropped: flag it — never silent (the
            // proptest area property caught this second drop path:
            // squarify emitted the rect, the visibility gate ate it).
            *truncated = true;
        }
    }
}

/// The core squarified strip algorithm. Input sizes MUST be sorted
/// descending (`children_sorted` guarantees that); zero sizes are skipped by
/// callers. Output rects match input order.
fn squarify(sizes: &[u64], rect: Rect) -> Vec<Rect> {
    // Saturating total: a tree of sizes summing past u64::MAX (2 EB)
    // would panic in debug and wrap in release, corrupting every
    // proportion (found by the proptest area property with near-MAX
    // sizes; rollup is already saturating — this matches it).
    let total: u64 = sizes.iter().copied().fold(0u64, u64::saturating_add);
    let mut out = Vec::with_capacity(sizes.len());
    if total == 0 || rect.w <= 0.0 || rect.h <= 0.0 || sizes.is_empty() {
        return out;
    }
    let mut remaining = rect;
    let mut rem_weight = total as f64;
    let mut head = 0usize;
    while head < sizes.len() {
        // Rows grow along the SHORTER side of the remaining rect.
        let horizontal = remaining.w >= remaining.h;
        let thickness = f64::from(if horizontal { remaining.h } else { remaining.w });
        let area = f64::from(remaining.w) * f64::from(remaining.h);
        let wpp = rem_weight / area.max(1.0);
        // Greedily grow the row while the worst aspect ratio improves.
        let row_begin = head;
        let mut row_end = head;
        let largest = sizes[head];
        let mut row_weight = 0u64;
        let mut worst = f64::MAX;
        while row_end < sizes.len() {
            let cw = sizes[row_end];
            if cw == 0 {
                break;
            }
            let next_w = row_weight + cw;
            // Square in f64 — the u64 form (`next_w * next_w`) overflows
            // above ~4.29 GiB of cumulative row weight, i.e. on every real
            // disk's root rows (debug: panic, release: garbage ratios that
            // quietly wreck squarification quality).
            let next_w_f = next_w as f64;
            let sq = next_w_f * next_w_f;
            let thickness_sq = thickness * thickness;
            let next_worst =
                (thickness_sq * wpp * largest as f64 / sq).max(sq / thickness_sq / wpp / cw as f64);
            if next_worst > worst {
                break;
            }
            row_weight = next_w;
            row_end += 1;
            worst = next_worst;
        }
        if row_weight == 0 {
            break; // all remaining zero-size; done.
        }
        // Row strip along the shorter side. The 1px minimum only applies
        // when the extent can still hold it: with many tiny children the
        // rows exhaust the remaining rect below 1px and clamp(1.0, extent)
        // PANICKED (min > max — the CI run that first executed a REAL
        // layout crashed the process here; sub-pixel rows are fine, they
        // are simply invisible).
        let extent = f64::from(if horizontal { remaining.w } else { remaining.h });
        let row_width =
            ((row_weight as f64 / rem_weight) * extent).clamp(extent.min(1.0), extent) as f32;
        let row_rect = if horizontal {
            Rect {
                x: remaining.x,
                y: remaining.y,
                w: row_width,
                h: remaining.h,
            }
        } else {
            Rect {
                x: remaining.x,
                y: remaining.y,
                w: remaining.w,
                h: row_width,
            }
        };
        // Lay row members along the LONG side, proportional to weight.
        let long_len = f64::from(if horizontal { row_rect.h } else { row_rect.w });
        let mut begin = 0.0f64;
        for &size in sizes.iter().take(row_end).skip(row_begin) {
            let len = size as f64 / row_weight as f64 * long_len;
            let len = len.max(0.0) as f32;
            out.push(if horizontal {
                Rect {
                    x: row_rect.x,
                    y: row_rect.y + begin as f32,
                    w: row_width,
                    h: len,
                }
            } else {
                Rect {
                    x: row_rect.x + begin as f32,
                    y: row_rect.y,
                    w: len,
                    h: row_width,
                }
            });
            begin += f64::from(len);
        }
        // Shrink the remaining rect.
        if horizontal {
            remaining.x += row_width;
            remaining.w = (remaining.w - row_width).max(0.0);
        } else {
            remaining.y += row_width;
            remaining.h = (remaining.h - row_width).max(0.0);
        }
        rem_weight -= row_weight as f64;
        head = row_end;
    }
    out
}

/// Group-layout variant for regrouped (By-type / By-age) treemaps: groups
/// as labeled top rects, member files inside (spec §7 color modes).
/// Group tuple: `(id, name, size, color, members)`.
///
/// # Errors
/// - [`CoreError::InvalidGeometry`] when `width`/`height` are zero.
/// - [`CoreError::TooManyCells`] when the cell budget would be exceeded.
pub fn treemap_groups(
    groups: &[GroupTuple],
    width: f32,
    height: f32,
    generation: u64,
    node: u32,
    color: ColorMode,
) -> Result<LayoutBuffer, CoreError> {
    check_geometry(width, height)?;
    let total: u64 = groups.iter().map(|g| g.2).sum();
    let mut cells: Vec<Cell> = Vec::with_capacity(256);
    let mut truncated = false;
    let rects = squarify(
        &groups.iter().map(|g| g.2).collect::<Vec<u64>>(),
        Rect {
            x: 0.0,
            y: 0.0,
            w: width,
            h: height,
        },
    );
    let mut descs = Vec::new();
    for ((id, name, gsize, gcolor, members), r) in groups.iter().zip(rects.iter()) {
        let rgba = pack_rgba(*gcolor);
        descs.push(GroupDesc {
            id: *id,
            name: name.clone(),
            color: *gcolor,
            size: *gsize,
        });
        if r.w >= HEADER_MIN_W && r.h >= HEADER_MIN_H {
            cells.push(Cell::header(*id, 1, rgba, r.x, r.y, r.w, HEADER_H));
            let body = Rect {
                x: r.x,
                y: r.y + HEADER_H,
                w: r.w,
                h: r.h - HEADER_H,
            };
            layout_group_members(*id, members, body, rgba, &mut cells, &mut truncated);
        } else if rect_visible(r.w, r.h) {
            cells.push(Cell::rect(*id, 1, rgba, r.x, r.y, r.w, r.h));
        }
    }
    Ok(LayoutBuffer {
        cells,
        meta: LayoutMeta {
            mode: "treemap".into(),
            generation,
            node,
            width,
            height,
            depth: 2,
            color_mode: color,
            cell_count: 0,
            truncated,
            center: None,
            groups: descs,
            total_bytes: total,
        },
    })
}

/// Squarify member files inside a group body.
fn layout_group_members(
    group_id: u32,
    members: &[(u32, u64)],
    body: Rect,
    group_rgba: u32,
    cells: &mut Vec<Cell>,
    truncated: &mut bool,
) {
    let sizes: Vec<u64> = members.iter().map(|m| m.1).collect();
    let rects = squarify(&sizes, body);
    for ((mid, _), r) in members.iter().zip(rects.iter()) {
        if cells.len() >= MAX_CELLS {
            *truncated = true;
            return;
        }
        if rect_visible(r.w, r.h) {
            cells.push(Cell::rect(*mid, 2, group_rgba, r.x, r.y, r.w, r.h));
        }
    }
    let _ = group_id;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::node::{BatchEntry, Node, Tree};
    use crate::scan::rollup;

    fn build_tree() -> Tree {
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\T");
        t.append_batch(
            0,
            vec![
                dir("a"),
                dir("b"),
                dir("c"),
                file("z1.bin", 100, 100, 1),
                file("z2.bin", 50, 50, 1),
            ],
        );
        t.append_batch(
            1,
            vec![
                file("a1", 60, 60, 1),
                file("a2", 30, 30, 1),
                file("a3", 50, 50, 1),
            ],
        );
        t.append_batch(2, vec![file("b1", 40, 40, 1), file("b2", 20, 20, 1)]);
        t.append_batch(3, vec![file("c1", 70, 70, 1)]);
        rollup::finalize(&mut t);
        t
    }

    fn dir(name: &str) -> BatchEntry {
        let mut node = Node::new_dir();
        node.modified = 10;
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
    fn squarify_areas_proportional_no_overlap_in_bounds() {
        // Spec §17: areas ∝ sizes, no overlaps, all inside the bounds.
        let sizes = vec![100u64, 60, 40, 30, 20, 10, 5, 1];
        let rects = squarify(
            &sizes,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 1000.0,
                h: 600.0,
            },
        );
        assert_eq!(rects.len(), sizes.len());
        let total: u64 = sizes.iter().sum();
        let total_area: f32 = rects.iter().map(|r| r.w * r.h).sum();
        for (i, r) in rects.iter().enumerate() {
            // Proportional areas (within rounding).
            let expected = sizes[i] as f32 / total as f32 * (1000.0 * 600.0);
            assert!(
                (r.w * r.h - expected).abs() < 4.0,
                "rect {i} area {} vs expected {expected}",
                r.w * r.h
            );
            // In bounds.
            assert!(r.x >= -0.01 && r.y >= -0.01);
            assert!(r.x + r.w <= 1000.01 && r.y + r.h <= 600.01);
        }
        let _ = total_area;
        // No pairwise overlap (1 px tolerance for float edges).
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                let a = rects[i];
                let b = rects[j];
                let overlap = a.x < b.x + b.w - 1.0
                    && b.x < a.x + a.w - 1.0
                    && a.y < b.y + b.h - 1.0
                    && b.y < a.y + a.h - 1.0;
                assert!(!overlap, "rects {i} and {j} overlap");
            }
        }
    }

    #[test]
    #[allow(clippy::float_cmp)] // exact by construction: the single rect IS the input rect
    fn squarify_single_child_fills_rect() {
        let rects = squarify(
            &[100],
            Rect {
                x: 10.0,
                y: 20.0,
                w: 300.0,
                h: 200.0,
            },
        );
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].w, 300.0);
        assert_eq!(rects[0].h, 200.0);
    }

    #[test]
    fn squarify_degenerate_inputs() {
        assert!(squarify(
            &[],
            Rect {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 100.0
            }
        )
        .is_empty());
        assert!(squarify(
            &[0, 0],
            Rect {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 100.0
            }
        )
        .is_empty());
        assert!(squarify(
            &[5],
            Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0
            }
        )
        .is_empty());
    }

    #[test]
    fn treemap_emits_cells_within_budget() {
        let t = build_tree();
        let buf = treemap(&t, 0, 1200.0, 800.0, 4, ColorMode::ByFolder, 100).unwrap();
        assert!(!buf.cells.is_empty());
        assert!(buf.cells.len() <= MAX_CELLS);
        // All cells in bounds.
        for c in &buf.cells {
            if c.flags == crate::layout::cell_kind::RECT
                || c.flags == crate::layout::cell_kind::HEADER
            {
                assert!(c.g[0] >= -0.5 && c.g[1] >= -0.5);
                assert!(c.g[0] + c.g[2] <= 1200.5 && c.g[1] + c.g[3] <= 800.5);
            }
        }
        // Root total flows through.
        assert_eq!(buf.meta.total_bytes, 420);
    }

    #[test]
    fn treemap_invalid_geometry_errors() {
        let t = build_tree();
        assert!(treemap(&t, 0, 0.0, 100.0, 3, ColorMode::ByType, 1).is_err());
        assert!(treemap(&t, 0, -5.0, 100.0, 3, ColorMode::ByAge, 1).is_err());
    }

    #[test]
    fn squarify_survives_degenerate_subpixel_extents() {
        // Regression (CI run 35688294160, first REAL layout execution):
        // many tiny children + the 1px row minimum exhaust the remaining
        // rect below 1px → clamp(1.0, extent) panicked (min > max) and
        // panic=abort killed the whole app. Sub-pixel rows must simply
        // render invisible.
        let mut sizes = vec![10_000u64];
        sizes.extend(std::iter::repeat_n(1, 120)); // 120 one-byte children
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            w: 300.0,
            h: 200.0,
        };
        let out = squarify(&sizes, rect);
        assert_eq!(out.len(), sizes.len(), "every child gets a rect");
        // Tiny canvas with many children: even harsher — no panic; the
        // rect exhausts and the invisible remainder is dropped (the
        // algorithm's existing contract: sub-pixel cells are omitted,
        // never crash).
        let out2 = squarify(
            &sizes,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 12.0,
                h: 9.0,
            },
        );
        assert!(!out2.is_empty() && out2.len() <= sizes.len());
        // Single-pixel canvas still must not panic (children beyond the
        // exhausted rect are dropped).
        let out3 = squarify(
            &[5u64, 3, 2],
            Rect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
            },
        );
        assert!(!out3.is_empty());
        // Zero-extent is rejected up front.
        assert!(squarify(
            &[1u64, 2],
            Rect {
                x: 0.0,
                y: 0.0,
                w: 0.5,
                h: 0.0
            }
        )
        .is_empty());
    }

    /// "This PC" → single "C:" drive → 6 folders with distinct sizes
    /// (each holding one file) — the single-child-root shape that used to
    /// collapse the whole map into one pastel family / re-assign families
    /// per level instead of one family per top-level branch.
    fn build_single_drive_tree() -> Tree {
        let mut t = Tree::new(1);
        t.add_root_path(0, "This PC");
        t.append_batch(0, vec![dir("C:")]); // id 1
        t.append_batch(
            1,
            vec![
                dir("Users"),       // 2
                dir("Windows"),     // 3
                dir("Programs"),    // 4
                dir("ProgramData"), // 5
                dir("Temp"),        // 6
                dir("Logs"),        // 7
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
    fn effective_branch_root_descends_single_child_chains() {
        // A root that already branches is its own branch root.
        let t = build_tree();
        assert_eq!(effective_branch_root(&t, 0), 0);
        // "This PC" → "C:" (single sizeable child) → 6 folders: the
        // branch root is C:, so families attach at C:'s children.
        let d = build_single_drive_tree();
        assert_eq!(effective_branch_root(&d, 0), 1);
    }

    #[test]
    fn treemap_single_child_root_assigns_branch_families() {
        let t = build_single_drive_tree();
        let buf = treemap(&t, 0, 1600.0, 1000.0, 4, ColorMode::ByFolder, 1).unwrap();
        let distinct = |depth: u16| {
            let mut v: Vec<u32> = buf
                .cells
                .iter()
                .filter(|c| c.depth == depth)
                .map(|c| c.rgba)
                .collect();
            v.sort_unstable();
            v.dedup();
            v.len()
        };
        // Depth-2 cells = C:'s children (the effective top-level branches):
        // they must span distinct pastel families, not one inherited family.
        assert!(
            distinct(2) >= 3,
            "branch cells must span >= 3 pastel families"
        );
        // Depth-3 cells (files inside the branches) INHERIT their branch's
        // family — the old per-level re-assignment collapsed them onto
        // sibling indices instead of keeping branch cohesion.
        assert!(
            distinct(3) >= 3,
            "descendants must inherit their branch families"
        );
    }

    /// Regression: cumulative row weights are BYTES — the old
    /// `(next_w * next_w) as f64` squared in u64 first, which overflows
    /// above `sqrt(u64::MAX)` ≈ 4.29 GiB. Every real-world disk's root rows
    /// (e.g. a 60 GB row) hit this: debug builds panicked, release builds
    /// produced garbage worst-ratios that quietly wrecked layout quality.
    #[test]
    fn squarify_gigabyte_weights_no_u64_overflow() {
        let sizes = vec![
            60_000_000_000u64, // 60 GB
            40_000_000_000,    // 40 GB
            25_000_000_000,    // 25 GB
            12_000_000_000,    // 12 GB
            8_000_000_000,     // 8 GB — cumulative 145 GB, far past the
                               // 4.29 GiB u64-square overflow point
        ];
        let rects = squarify(
            &sizes,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 1600.0,
                h: 1000.0,
            },
        );
        assert_eq!(rects.len(), sizes.len());
        let total: u64 = sizes.iter().sum();
        // Areas stay proportional (within rounding) — with the overflow,
        // ratios exploded and the assertions below fail or panic.
        for (i, r) in rects.iter().enumerate() {
            let expected = sizes[i] as f64 / total as f64 * (1600.0 * 1000.0);
            let area = f64::from(r.w) * f64::from(r.h);
            assert!(
                (area - expected).abs() < 600.0,
                "rect {i} area {area} vs expected {expected}"
            );
            assert!(r.x >= -0.01 && r.y >= -0.01);
            assert!(r.x + r.w <= 1600.01 && r.y + r.h <= 1000.01);
        }
    }

    /// Companion: the row-growth decision itself must not overflow — a
    /// single file above 4 GiB alone exceeds the old u64 square.
    #[test]
    fn squarify_single_file_above_4gib() {
        let sizes = vec![5_500_000_000u64]; // > sqrt(u64::MAX)
        let rects = squarify(
            &sizes,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 800.0,
                h: 600.0,
            },
        );
        assert_eq!(rects.len(), 1);
        let r = rects[0];
        assert!((r.w - 800.0).abs() < 0.01 && (r.h - 600.0).abs() < 0.01);
    }
}
