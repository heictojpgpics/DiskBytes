//! Regrouped canvas layouts (spec §7 color modes): the By-type / By-age
//! variants of Sunburst, Flame, Bubbles and Mind Map.
//!
//! Each takes the synthetic groups from [`crate::layout::regroup`] —
//! `(id, name, size, color, members)` — and lays them out with the same
//! geometry semantics as the real-tree engines, so the JS renderer and
//! hit-testing stay identical. Groups sit at depth 1; member files at
//! depth 2. The treemap variant lives in
//! [`crate::layout::treemap::treemap_groups`].
//!
//! Sizing law per mode matches its engine: arc ∝ size (sunburst), width ∝
//! size (flame), area ∝ size (bubbles), dot area ∝ share (mind map).

use super::{check_geometry, Cell, ColorMode, GroupDesc, GroupTuple, LayoutBuffer, LayoutMeta};
use crate::error::CoreError;
use crate::layout::over_budget;

/// Emit the group legend shared by every variant.
fn group_descs(groups: &[GroupTuple]) -> Vec<GroupDesc> {
    groups
        .iter()
        .map(|g| GroupDesc {
            id: g.0,
            name: g.1.clone(),
            color: g.3,
            size: g.2,
        })
        .collect()
}

/// # Errors
/// - [`CoreError::InvalidGeometry`] when `width`/`height` are zero.
fn checked(width: f32, height: f32) -> Result<(), CoreError> {
    check_geometry(width, height)
}

// Engine-parity constants: IMPORTED from the engines they mirror so
// parity is structural, not aspirational. These used to be re-declared
// with "matches the real-tree engine" comments that had already
// silently drifted — the sunburst center fraction (0.12 vs the
// engine's 0.16 after the coral-center enlargement) and the mind-map
// root dot (hardcoded 12 vs ROOT_DOT_R's 14, which the JS label gate
// r >= 13 depends on to even name the root).
/// Bubbles gap between sibling circles (engine parity).
use crate::layout::bubbles::GAP as BUB_GAP;
/// Bubbles minimum radius (engine parity).
use crate::layout::bubbles::MIN_R as BUB_MIN_R;
/// Bubbles padding between a parent rim and its content (engine parity).
use crate::layout::bubbles::PAD as BUB_PAD;
/// Flame horizontal gap (engine parity).
use crate::layout::flame::GAP_X as FLAME_GAP_X;
/// Flame minimum block width (engine parity).
use crate::layout::flame::MIN_W as FLAME_MIN_W;
/// Mind Map group dot base scale (engine parity).
use crate::layout::mindmap::DOT_BASE as MIND_GROUP_DOT;
/// Mind Map minimum dot radius (engine parity).
use crate::layout::mindmap::MIN_R as MIND_MIN_R;
/// Mind Map root hub dot radius (engine parity — 14px clears the JS
/// label gate at r >= 13 so the regrouped root is named, like the
/// real-tree engine's).
use crate::layout::mindmap::ROOT_DOT_R;
/// Sunburst arc gap (engine parity).
use crate::layout::sunburst::ARC_GAP as SUN_ARC_GAP;
/// Sunburst center disc radius fraction (engine: [`crate::layout::sunburst`]).
use crate::layout::sunburst::CENTER_R_FRACTION as SUN_CENTER_R_FRACTION;
/// Minimum sunburst arc span (engine parity).
use crate::layout::sunburst::MIN_ARC as SUN_MIN_ARC;
/// Sunburst ring gap (engine parity).
use crate::layout::sunburst::RING_GAP as SUN_RING_GAP;
/// Parity proofs: the groups twins import the engine constants, so a
/// future engine change that forgets the twin breaks COMPILE here (the
/// old re-declared "matches" comments drifted silently for months).
const _: () = assert!(SUN_CENTER_R_FRACTION == crate::layout::sunburst::CENTER_R_FRACTION);
const _: () = assert!(SUN_RING_GAP == crate::layout::sunburst::RING_GAP);
const _: () = assert!(SUN_ARC_GAP == crate::layout::sunburst::ARC_GAP);
const _: () = assert!(SUN_MIN_ARC == crate::layout::sunburst::MIN_ARC);
const _: () = assert!(FLAME_MIN_W == crate::layout::flame::MIN_W);
const _: () = assert!(FLAME_GAP_X == crate::layout::flame::GAP_X);
const _: () = assert!(BUB_PAD == crate::layout::bubbles::PAD);
const _: () = assert!(BUB_GAP == crate::layout::bubbles::GAP);
const _: () = assert!(BUB_MIN_R == crate::layout::bubbles::MIN_R);
const _: () = assert!(MIND_MIN_R == crate::layout::mindmap::MIN_R);
const _: () = assert!(MIND_GROUP_DOT == crate::layout::mindmap::DOT_BASE);

/// Regrouped sunburst ring count (groups + members).
const SUN_DEPTH_RINGS: u16 = 2;
/// Regrouped flame row count (root + groups + members).
const FLAME_ROWS: u16 = 3;

/// Sunburst, regrouped: groups on the first ring, member files on the
/// second. Arc length ∝ group size, member arcs ∝ file size within the
/// group span. The center disc is the folder node.
///
/// # Errors
/// - [`CoreError::InvalidGeometry`] when `width`/`height` are zero.
#[allow(clippy::too_many_arguments)]
pub fn sunburst_groups(
    groups: &[GroupTuple],
    width: f32,
    height: f32,
    generation: u64,
    node: u32,
    color: ColorMode,
) -> Result<LayoutBuffer, CoreError> {
    checked(width, height)?;
    let total: u64 = groups.iter().map(|g| g.2).sum();
    let cx = width / 2.0;
    let cy = height / 2.0;
    let r_max = width.min(height) / 2.0 - 4.0;
    let r_center = r_max * SUN_CENTER_R_FRACTION;
    let ring_w =
        (r_max - r_center - SUN_RING_GAP * f32::from(SUN_DEPTH_RINGS)) / f32::from(SUN_DEPTH_RINGS);
    let mut cells: Vec<Cell> = Vec::with_capacity(512);
    let mut truncated = false;
    cells.push(Cell::circle(
        node,
        0,
        crate::layout::pack_rgba(crate::layout::ANCHOR_GRAY),
        cx,
        cy,
        r_center,
    ));
    if total > 0 && ring_w > 0.5 {
        // Spill-free gap accounting: every group can emit an arc, so the
        // budget covers all of them (see the sunburst engine's fix note).
        let usable = (std::f32::consts::TAU - SUN_ARC_GAP * groups.len() as f32).max(0.0);
        let mut cursor = 0.0f32;
        for g in groups {
            if over_budget(&cells, &mut truncated) {
                break;
            }
            let arc = g.2 as f32 / total as f32 * usable.max(0.0);
            if arc < SUN_MIN_ARC {
                continue;
            }
            let a0 = cursor;
            let a1 = cursor + arc;
            let rgba = crate::layout::pack_rgba(g.3);
            cells.push(Cell::arc(
                g.0,
                1,
                rgba,
                a0,
                a1,
                r_center + SUN_RING_GAP,
                r_center + SUN_RING_GAP + ring_w,
            ));
            // Members inside the group's angular span on ring 2.
            let m_total: u64 = g.4.iter().map(|m| m.1).sum();
            if m_total > 0 {
                let r1 = r_center + SUN_RING_GAP * 2.0 + ring_w;
                let m_usable = (arc - SUN_ARC_GAP * g.4.len() as f32).max(0.0);
                let mut m_cursor = a0;
                for (mid, msize) in &g.4 {
                    if over_budget(&cells, &mut truncated) {
                        break;
                    }
                    let m_arc = *msize as f32 / m_total as f32 * m_usable.max(0.0);
                    if m_arc < SUN_MIN_ARC {
                        continue;
                    }
                    cells.push(Cell::arc(
                        *mid,
                        2,
                        rgba,
                        m_cursor,
                        m_cursor + m_arc,
                        r1,
                        r1 + ring_w,
                    ));
                    m_cursor += m_arc + SUN_ARC_GAP;
                }
            }
            cursor = a1 + SUN_ARC_GAP;
        }
    }
    Ok(LayoutBuffer {
        cells,
        meta: LayoutMeta {
            mode: "sunburst".into(),
            generation,
            node,
            width,
            height,
            depth: u32::from(SUN_DEPTH_RINGS),
            color_mode: color,
            cell_count: 0,
            truncated,
            center: Some((cx, cy)),
            groups: group_descs(groups),
            total_bytes: total,
        },
    })
}

/// One kept flame group: (id, raw width, color, member slices).
type KeptGroup<'a> = (u32, f32, u32, &'a [(u32, u64)]);

/// Member blocks of one flame-group span on row 2, picket-fence gaps
/// (engine parity with `flame::layout_row`'s member handling).
fn flame_group_members(
    members: &[(u32, u64)],
    w: f32,
    cursor: f32,
    row_h: f32,
    rgba: u32,
    cells: &mut Vec<Cell>,
    truncated: &mut bool,
) {
    let m_total: u64 = members.iter().map(|m| m.1).sum();
    if m_total == 0 {
        return;
    }
    let m_kept: Vec<(u32, f32)> = members
        .iter()
        .filter_map(|(mid, msize)| {
            let mw = *msize as f32 / m_total as f32 * w;
            (mw >= FLAME_MIN_W).then_some((*mid, mw))
        })
        .collect();
    let m_gaps: Vec<f32> = m_kept
        .windows(2)
        .map(|p| {
            if p[0].1 >= crate::layout::flame::GAP_MIN_W
                && p[1].1 >= crate::layout::flame::GAP_MIN_W
            {
                FLAME_GAP_X
            } else {
                0.0
            }
        })
        .collect();
    let m_gap_total: f32 = m_gaps.iter().sum();
    let m_kept_total: f32 = m_kept.iter().map(|k| k.1).sum();
    let m_usable = (w - m_gap_total).max(0.0);
    let m_scale = if m_kept_total > 0.0 {
        m_usable / m_kept_total
    } else {
        0.0
    };
    let mut m_cursor = cursor;
    for (mslot, &(mid, mw_raw)) in m_kept.iter().enumerate() {
        if over_budget(cells, truncated) {
            break;
        }
        let mw = mw_raw * m_scale;
        if mw < FLAME_MIN_W {
            continue;
        }
        cells.push(Cell::rect(mid, 2, rgba, m_cursor, row_h * 2.0, mw, row_h));
        m_cursor += mw + m_gaps.get(mslot).copied().unwrap_or(0.0);
    }
}

/// Flame, regrouped: groups on row 1 (width ∝ group size), member files on
/// row 2 under their group's span.
///
/// # Errors
/// - [`CoreError::InvalidGeometry`] when `width`/`height` are zero.
#[allow(clippy::too_many_arguments)]
pub fn flame_groups(
    groups: &[GroupTuple],
    width: f32,
    height: f32,
    generation: u64,
    node: u32,
    color: ColorMode,
) -> Result<LayoutBuffer, CoreError> {
    checked(width, height)?;
    let total: u64 = groups.iter().map(|g| g.2).sum();
    // Three visual rows: root (0), group spans (1), member blocks (2).
    // This used to divide by 2, placing every member rect at
    // y = row_h * 2.0 = height — fully below the canvas.
    let row_h = height / f32::from(FLAME_ROWS);
    let mut cells: Vec<Cell> = Vec::with_capacity(512);
    let mut truncated = false;
    // Root block spanning the full width on row 0 (the folder).
    cells.push(Cell::rect(
        node,
        0,
        crate::layout::pack_rgba(crate::layout::ANCHOR_GRAY),
        0.0,
        0.0,
        width,
        row_h,
    ));
    if total > 0 {
        // Picket-fence gaps (engine parity with `flame::layout_row`): a
        // gap is charged only BETWEEN adjacent blocks that are both
        // >= GAP_MIN_W wide — thin blocks render flush instead of
        // striped by half-pixel gutters, and with many groups the old
        // unconditional per-pair gap burned real span. Ratios among
        // drawn blocks stay exact through the rescale.
        let kept: Vec<KeptGroup> = groups
            .iter()
            .filter(|g| g.2 > 0)
            .filter_map(|g| {
                let w = g.2 as f32 / total as f32 * width;
                (w >= FLAME_MIN_W).then_some((g.0, w, g.3, g.4.as_slice()))
            })
            .collect();
        let gaps: Vec<f32> = kept
            .windows(2)
            .map(|p| {
                if p[0].1 >= crate::layout::flame::GAP_MIN_W
                    && p[1].1 >= crate::layout::flame::GAP_MIN_W
                {
                    FLAME_GAP_X
                } else {
                    0.0
                }
            })
            .collect();
        let gap_total: f32 = gaps.iter().sum();
        let kept_total: f32 = kept.iter().map(|k| k.1).sum();
        let usable = (width - gap_total).max(0.0);
        let scale = if kept_total > 0.0 {
            usable / kept_total
        } else {
            0.0
        };
        let mut cursor = 0.0f32;
        for (slot, &(gid, w_raw, gcolor, gmembers)) in kept.iter().enumerate() {
            if over_budget(&cells, &mut truncated) {
                break;
            }
            let w = w_raw * scale;
            if w < FLAME_MIN_W {
                continue; // Rounding edge after rescale.
            }
            let rgba = crate::layout::pack_rgba(gcolor);
            cells.push(Cell::rect(gid, 1, rgba, cursor, row_h, w, row_h));
            // Members under the group span on row 2 (same fence rule).
            flame_group_members(gmembers, w, cursor, row_h, rgba, &mut cells, &mut truncated);
            cursor += w + gaps.get(slot).copied().unwrap_or(0.0);
        }
    }
    Ok(LayoutBuffer {
        cells,
        meta: LayoutMeta {
            mode: "flame".into(),
            generation,
            node,
            width,
            height,
            depth: u32::from(FLAME_ROWS),
            color_mode: color,
            cell_count: 0,
            truncated,
            center: None,
            groups: group_descs(groups),
            total_bytes: total,
        },
    })
}

/// Bubbles, regrouped: groups are translucent circles ring-packed in the
/// viewport; member files are circles inside the group (area ∝ bytes). To
/// keep every sibling area ratio exact, each group's member radii use
/// sqrt-share of the group's usable radius and a uniform shrink-to-fit —
/// the same discipline as the real-tree engine.
///
/// # Errors
/// - [`CoreError::InvalidGeometry`] when `width`/`height` are zero.
#[allow(clippy::too_many_arguments)]
pub fn bubbles_groups(
    groups: &[GroupTuple],
    width: f32,
    height: f32,
    generation: u64,
    node: u32,
    color: ColorMode,
) -> Result<LayoutBuffer, CoreError> {
    checked(width, height)?;
    let total: u64 = groups.iter().map(|g| g.2).sum();
    let cx = width / 2.0;
    let cy = height / 2.0;
    let root_r = width.min(height) / 2.0 - 2.0;
    let mut cells: Vec<Cell> = Vec::with_capacity(512);
    let mut truncated = false;
    // Root circle (the folder) at center.
    cells.push(Cell::circle(
        node,
        0,
        crate::layout::pack_rgba(crate::layout::ANCHOR_GRAY),
        cx,
        cy,
        root_r,
    ));
    if total > 0 && root_r > 2.0 * BUB_PAD {
        // Group radii: sqrt-share of the usable root radius (area ∝ bytes;
        // sibling ratios exact). Ring-pack largest-first; when the pack
        // overflows, uniformly shrink every radius by ONE factor and
        // re-place (ratios preserved exactly — same discipline as the
        // real-tree engine).
        let usable = root_r - BUB_PAD;
        let sizes: Vec<f64> = groups.iter().map(|g| g.2 as f64).collect();
        let sum_sqrt: f64 = sizes.iter().map(|s| s.sqrt()).sum();
        let mut shrink = 1.0f32;
        let mut placed: Vec<(f32, f32, f32)> = Vec::new(); // (cx, cy, r)
        for _round in 0..2 {
            let radii: Vec<f32> = sizes
                .iter()
                .map(|&s| {
                    (s.sqrt() / sum_sqrt.max(f64::EPSILON) * f64::from(usable) * f64::from(shrink))
                        .clamp(0.0, 4096.0) as f32
                })
                .collect();
            placed = pack_rings(cx, cy, &radii, BUB_GAP);
            let max_reach = placed
                .iter()
                .filter(|p| p.2 > 0.0)
                .map(|(x, y, r)| {
                    let dx = x - cx;
                    let dy = y - cy;
                    dx.mul_add(dx, dy * dy).sqrt() + r
                })
                .fold(0.0f32, f32::max);
            if max_reach <= usable || placed.is_empty() {
                break;
            }
            shrink *= (usable / max_reach).clamp(0.05, 0.999);
        }
        // Emit group circles + members.
        for (i, g) in groups.iter().enumerate() {
            if over_budget(&cells, &mut truncated) {
                break;
            }
            if i >= placed.len() {
                break;
            }
            let (gx, gy, gr) = placed[i];
            if gr < BUB_MIN_R {
                continue;
            }
            let rgba = crate::layout::pack_rgba(g.3);
            cells.push(Cell::circle(g.0, 1, rgba, gx, gy, gr));
            // Members: sqrt-share of the group's usable radius, ring-packed
            // largest-first with the same uniform shrink discipline.
            let m_usable = (gr - BUB_PAD).max(0.0);
            let m_total: u64 = g.4.iter().map(|m| m.1).sum();
            if m_total > 0 && m_usable > 2.0 * BUB_MIN_R {
                let m_sizes: Vec<f64> = g.4.iter().map(|m| m.1 as f64).collect();
                let m_sum: f64 = m_sizes.iter().map(|s| s.sqrt()).sum();
                let m_radii: Vec<f32> = m_sizes
                    .iter()
                    .map(|&s| {
                        (s.sqrt() / m_sum.max(f64::EPSILON) * f64::from(m_usable))
                            .clamp(0.0, 4096.0) as f32
                    })
                    .collect();
                let m_placed = pack_rings(gx, gy, &m_radii, BUB_GAP);
                for ((mid, _), (mx, my, mr)) in g.4.iter().zip(m_placed.iter()) {
                    if over_budget(&cells, &mut truncated) {
                        break;
                    }
                    if *mr >= BUB_MIN_R {
                        cells.push(Cell::circle(*mid, 2, rgba, *mx, *my, *mr));
                    }
                }
            }
        }
    }
    Ok(LayoutBuffer {
        cells,
        meta: LayoutMeta {
            mode: "bubbles".into(),
            generation,
            node,
            width,
            height,
            depth: 2,
            color_mode: color,
            cell_count: 0,
            truncated,
            center: Some((cx, cy)),
            groups: group_descs(groups),
            total_bytes: total,
        },
    })
}

/// Ring-pack circles (largest-first input assumed) around `(cx, cy)`:
/// concentric rings grow outward; zero/negative radii keep index
/// alignment as `(cx, cy, 0.0)` placeholders.
fn pack_rings(cx: f32, cy: f32, radii: &[f32], gap: f32) -> Vec<(f32, f32, f32)> {
    let mut out: Vec<(f32, f32, f32)> = Vec::with_capacity(radii.len());
    let mut ring_dist = 0.0f32; // center distance of the current ring
    let mut ring_max = 0.0f32; // largest radius placed on the current ring
    let mut prev_r = 0.0f32;
    let mut angle = 0.0f32; // angular position of the LAST placed center
    let mut used = 0.0f32; // total angle consumed on the current ring
    for &r in radii {
        if r <= 0.0 {
            out.push((cx, cy, 0.0));
            continue;
        }
        if out.is_empty() || ring_dist <= 0.0 {
            // First circle: tangent to the center point.
            ring_dist = r;
            ring_max = r;
            prev_r = r;
            angle = 0.0;
            used = 0.0;
            out.push((cx, cy - r, r));
            continue;
        }
        // Chord angle needed between the previous center and this one.
        let need = 2.0 * (((prev_r + r + gap) / (2.0 * ring_dist)).clamp(-1.0, 1.0)).asin();
        let full = std::f32::consts::TAU;
        if used + need < full - 1e-3 {
            used += need;
            angle += need;
            let a = -std::f32::consts::FRAC_PI_2 + angle;
            out.push((cx + ring_dist * a.cos(), cy + ring_dist * a.sin(), r));
            prev_r = r;
        } else {
            // New ring outward: distance grows by the previous ring's
            // largest radius + this radius + gap.
            ring_dist += ring_max + r + gap;
            ring_max = r;
            prev_r = r;
            angle = 0.0;
            used = 0.0;
            out.push((cx, cy - ring_dist, r));
        }
    }
    out
}

/// Mind Map, regrouped: the folder dot at center, group dots on the first
/// ring (angular span ∝ group size), member dots inside each group's span
/// on the second ring. Dot area ∝ share.
///
/// # Errors
/// - [`CoreError::InvalidGeometry`] when `width`/`height` are zero.
#[allow(clippy::too_many_arguments)]
pub fn mindmap_groups(
    groups: &[GroupTuple],
    width: f32,
    height: f32,
    generation: u64,
    node: u32,
    color: ColorMode,
) -> Result<LayoutBuffer, CoreError> {
    checked(width, height)?;
    let total: u64 = groups.iter().map(|g| g.2).sum();
    let cx = width / 2.0;
    let cy = height / 2.0;
    let r_max = width.min(height) / 2.0 - 6.0;
    let mut cells: Vec<Cell> = Vec::with_capacity(512);
    let mut truncated = false;
    cells.push(Cell::dot(
        node,
        0,
        crate::layout::pack_rgba(crate::layout::ANCHOR_GRAY),
        cx,
        cy,
        ROOT_DOT_R, // 14px: clears the JS label gate (r >= 13) — the old hardcoded 12 left the regrouped root unnamed.
        cx,
        cy,
    ));
    if total > 0 && r_max > 8.0 {
        let ring_r = r_max * 0.55; // groups ring; members sit further out
        let g_arc = std::f32::consts::TAU / groups.len().max(1) as f32;
        let mut cursor = -std::f32::consts::FRAC_PI_2;
        for g in groups {
            if over_budget(&cells, &mut truncated) {
                break;
            }
            let span = g.2 as f32 / total as f32 * std::f32::consts::TAU;
            let mid_angle = cursor + span / 2.0;
            let gx = cx + ring_r * mid_angle.cos();
            let gy = cy + ring_r * mid_angle.sin();
            let g_r = (MIND_GROUP_DOT * (g.2 as f32 / total as f32).sqrt() * 4.0).clamp(4.0, 40.0);
            let rgba = crate::layout::pack_rgba(g.3);
            cells.push(Cell::dot(g.0, 1, rgba, gx, gy, g_r, cx, cy));
            // Members on the outer ring inside the group's angular span.
            let m_ring = r_max;
            let m_total: u64 = g.4.iter().map(|m| m.1).sum();
            if m_total > 0 {
                let m_step = span / g.4.len().max(1) as f32;
                let m_start = cursor;
                for (j, (mid, _msize)) in g.4.iter().enumerate() {
                    if over_budget(&cells, &mut truncated) {
                        break;
                    }
                    let ang = m_start + m_step * (j as f32 + 0.5);
                    let mx = cx + m_ring * ang.cos();
                    let my = cy + m_ring * ang.sin();
                    cells.push(Cell::dot(*mid, 2, rgba, mx, my, MIND_MIN_R, gx, gy));
                }
            }
            cursor += span.max(g_arc * 0.0) + 0.0; // spans are exact ∝ size
            let _ = g_arc;
        }
    }
    Ok(LayoutBuffer {
        cells,
        meta: LayoutMeta {
            mode: "mindmap".into(),
            generation,
            node,
            width,
            height,
            depth: 2,
            color_mode: color,
            cell_count: 0,
            truncated,
            center: Some((cx, cy)),
            groups: group_descs(groups),
            total_bytes: total,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::regroup::{by_age, by_type, SYNTH_BASE};

    fn build() -> crate::scan::node::Tree {
        let mut t = crate::scan::node::Tree::new(1);
        t.add_root_path(0, "C:\\G");
        let mk = |name: &str, logical: u64, modified: i64| {
            let mut node = crate::scan::node::Node::new_file();
            node.logical = logical;
            node.on_disk = logical;
            node.modified = modified;
            node.set_category(crate::scan::categories::FileCategory::from_name(
                &name.encode_utf16().collect::<Vec<u16>>(),
            ));
            crate::scan::node::BatchEntry {
                name: name.encode_utf16().collect(),
                node,
            }
        };
        t.append_batch(
            0,
            vec![
                mk("a.mp4", 100, 1),
                mk("b.mp3", 50, 2),
                mk("c.txt", 30, 3),
                mk("d.mp4", 20, 4),
            ],
        );
        crate::scan::rollup::finalize(&mut t);
        t
    }

    #[test]
    fn sunburst_groups_arcs_present() {
        let t = build();
        let r = by_type(&t, 0, 100);
        let buf = sunburst_groups(&r.groups, 800.0, 800.0, 1, 0, ColorMode::ByType).unwrap();
        assert_eq!(buf.meta.mode, "sunburst");
        assert!(buf.meta.center.is_some());
        assert!(buf.cells.iter().any(|c| c.id == SYNTH_BASE));
        // Group arcs at depth 1, member arcs at depth 2.
        assert!(buf
            .cells
            .iter()
            .any(|c| (c.flags & 0b111) == 1 && c.depth == 1));
        assert!(buf
            .cells
            .iter()
            .any(|c| (c.flags & 0b111) == 1 && c.depth == 2));
        // Proportionality: biggest group arc ≈ 120/200 of TAU (minus gaps).
        let g0 = buf.cells.iter().find(|c| c.id == SYNTH_BASE).unwrap();
        let span = g0.g[1] - g0.g[0];
        let expect = std::f32::consts::TAU * 120.0 / 200.0;
        assert!((span - expect).abs() < 0.15, "span {span} vs {expect}");
    }

    #[test]
    fn flame_groups_rows_stack() {
        let t = build();
        let r = by_type(&t, 0, 100);
        let buf = flame_groups(&r.groups, 600.0, 300.0, 1, 0, ColorMode::ByType).unwrap();
        assert_eq!(buf.meta.mode, "flame");
        // Three rows: root y=0, groups y=100, members y=200 (row_h = 300/3).
        // (The old 2-row math put members at y = 300 = height, fully
        // off-canvas — every member block was invisible.)
        let root = buf.cells.iter().find(|c| c.depth == 0).unwrap();
        assert!((root.g[2] - 600.0).abs() < 0.01);
        assert!((root.g[1] - 0.0).abs() < 0.01);
        let groups: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 1).collect();
        assert_eq!(groups.len(), 3);
        assert!(groups.iter().all(|c| (c.g[1] - 100.0).abs() < 0.01));
        // Widths ∝ sizes: 120 / 50 / 30 of 200 → ~300/125/75 px (minus gaps).
        let w0 = groups.iter().map(|c| c.g[2]).fold(0.0f32, f32::max);
        let w_min = groups.iter().map(|c| c.g[2]).fold(f32::MAX, f32::min);
        assert!(w0 / w_min > 3.9, "{w0} vs {w_min}");
        // THE missing property: every cell of every row fits INSIDE the
        // canvas — member blocks live on row 2 (y=200, height 100).
        let members: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 2).collect();
        assert!(!members.is_empty(), "member cells must be emitted");
        for c in &buf.cells {
            assert!(c.g[0] >= -0.01, "x below canvas: {}", c.g[0]);
            assert!(c.g[0] + c.g[2] <= 600.0 + 0.01, "x beyond canvas");
            assert!(c.g[1] >= -0.01, "y below canvas: {}", c.g[1]);
            assert!(
                c.g[1] + c.g[3] <= 300.0 + 0.01,
                "y beyond canvas: {}",
                c.g[1] + c.g[3]
            );
        }
        assert!(
            members.iter().all(|c| (c.g[1] - 200.0).abs() < 0.01),
            "members on row 2"
        );
    }

    #[test]
    fn bubbles_groups_circles_inside_root() {
        let t = build();
        let r = by_age(&t, 0, 1_800_000_000, 100);
        let buf = bubbles_groups(&r.groups, 700.0, 700.0, 1, 0, ColorMode::ByAge).unwrap();
        assert_eq!(buf.meta.mode, "bubbles");
        let cx = 350.0;
        let cy = 350.0;
        // Every group circle (and its members) fits inside the root circle.
        for c in &buf.cells {
            if (c.flags & 0b111) == 2 {
                let reach = ((c.g[0] - cx).powi(2) + (c.g[1] - cy).powi(2)).sqrt() + c.g[2];
                assert!(reach <= 348.0 + 0.5, "circle escapes root: {reach}");
            }
        }
    }

    #[test]
    fn mindmap_groups_dots_and_links() {
        let t = build();
        let r = by_type(&t, 0, 100);
        let buf = mindmap_groups(&r.groups, 800.0, 600.0, 1, 0, ColorMode::ByType).unwrap();
        assert_eq!(buf.meta.mode, "mindmap");
        // Root dot at center, group dots link to it.
        let root = buf.cells.iter().find(|c| c.depth == 0).unwrap();
        assert!((root.g[0] - 400.0).abs() < 0.01);
        let groups: Vec<&Cell> = buf.cells.iter().filter(|c| c.depth == 1).collect();
        assert_eq!(groups.len(), 3);
        assert!(groups
            .iter()
            .all(|g| g.g[3].to_bits() == 400.0f32.to_bits()
                && g.g[4].to_bits() == 300.0f32.to_bits()));
    }

    /// The regrouped mind-map root hub must clear the JS label gate
    /// (r >= 13) exactly like the real-tree engine — the old hardcoded
    /// 12.0 left the regrouped root an unnamed gray blob.
    #[test]
    fn mindmap_groups_root_dot_clears_label_gate() {
        let t = build();
        let r = by_type(&t, 0, 100);
        let buf = mindmap_groups(&r.groups, 800.0, 600.0, 1, 0, ColorMode::ByType).unwrap();
        let root = buf.cells.iter().find(|c| c.depth == 0).unwrap();
        assert!(
            root.g[2] >= 13.0,
            "root hub radius {} must clear the 13px label gate",
            root.g[2]
        );
        assert!(
            (root.g[2] - crate::layout::mindmap::ROOT_DOT_R).abs() < 0.01,
            "root hub radius must equal ROOT_DOT_R (engine parity)"
        );
    }

    #[test]
    fn zero_geometry_rejected_everywhere() {
        let t = build();
        let r = by_type(&t, 0, 100);
        for res in [
            sunburst_groups(&r.groups, 0.0, 10.0, 1, 0, ColorMode::ByType),
            flame_groups(&r.groups, 10.0, 0.0, 1, 0, ColorMode::ByType),
            bubbles_groups(&r.groups, 0.0, 0.0, 1, 0, ColorMode::ByType),
            mindmap_groups(&r.groups, -1.0, 10.0, 1, 0, ColorMode::ByType),
        ] {
            assert!(res.is_err());
        }
    }
}
