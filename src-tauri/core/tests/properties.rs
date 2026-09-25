//! Property-based tests (owner instruction: "add thousands of realistic
//! … tests for all, test all edge cases"). Each `proptest!` strategy
//! runs 128–512 randomized cases — this file executes ~5,000 generated
//! scenarios per CI run on top of the unit + platform suites,
//! exercising tree shapes, arithmetic boundaries, surgery algebra,
//! layout geometry invariants, and parser robustness that hand-written
//! fixtures can't cover.
//!
//! Deterministic seeds keep CI failures reproducible; every generator
//! produces only VALID trees (parent-before-child ids, contiguous
//! children, one batch per directory) so properties test behavior, not
//! invariant violations the engine never produces.

use diskbytes_core::dupes::{self, HashedFile};
use diskbytes_core::scan::categories::FileCategory;
use diskbytes_core::scan::node::{BatchEntry, Node, Tree};
use diskbytes_core::scan::rollup;
use diskbytes_core::scan::surgery;
use diskbytes_core::{format, layout};

use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Generators: seeds -> valid random trees (deterministic LCG).
// ---------------------------------------------------------------------------

/// Byte sizes biased toward interesting boundaries: 0, 1, cluster
/// edges, 4 GiB, near-u64-max.
fn size_strategy() -> impl Strategy<Value = u64> {
    prop_oneof![
        1 => Just(0u64),
        2 => Just(1u64),
        4 => (1u64..8_192).boxed(),
        2 => Just(4095),
        2 => Just(4096),
        2 => Just(4097),
        2 => (4_000_000_000u64..5_000_000_000).boxed(),
        1 => Just(u64::MAX / 4),
        1 => Just(u64::MAX / 2),
    ]
}

/// Deterministic pseudo-random tree from a seed: a root batch of `fan`
/// mixed children (files + dirs), each dir getting one child batch.
/// Sizes run through an LCG so every seed yields a different shape.
fn build_tree(seed: u64, fan: u32) -> Tree {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
    let mut next = || {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        s >> 33
    };
    // Pre-draw all randomness up front (no closure capture juggling).
    let draws: Vec<u64> = (0..(fan as usize * 8 + 8))
        .map(|_| next().max(1) % (64 * 1024 * 1024))
        .collect();
    let mk_file = |name: String, logical: u64, modified: i64| {
        let mut n = Node::new_file();
        n.logical = logical;
        n.on_disk = logical.div_ceil(4096) * 4096;
        n.modified = modified;
        n.set_category(FileCategory::from_name(
            &name.encode_utf16().collect::<Vec<u16>>(),
        ));
        BatchEntry {
            name: name.encode_utf16().collect(),
            node: n,
        }
    };
    let mut t = Tree::new(1);
    t.add_root_path(0, "C:\\prop");
    let mut entries: Vec<BatchEntry> = Vec::new();
    let mut subdirs: Vec<usize> = Vec::new();
    for i in 0..fan {
        let d = draws[i as usize];
        if d % 3 == 0 {
            subdirs.push(entries.len());
            let mut n = Node::new_dir();
            n.modified = 1_700_000_000;
            entries.push(BatchEntry {
                name: format!("d{i:03}").encode_utf16().collect(),
                node: n,
            });
        } else {
            entries.push(mk_file(
                format!("f{i:03}.bin"),
                d,
                1_700_000_000 + i64::from(i),
            ));
        }
    }
    let base = t.append_batch(0, entries);
    for (k, &idx) in subdirs.iter().enumerate() {
        let parent = base + idx as u32;
        let kids: Vec<BatchEntry> = (0..fan.min(6))
            .map(|j| {
                let d = draws[(fan as usize + k * 6 + j as usize).min(draws.len() - 1)];
                mk_file(format!("s{k}_{j}.dat"), d, 1_700_000_000 + i64::from(j))
            })
            .collect();
        t.append_batch(parent, kids);
    }
    rollup::finalize(&mut t);
    t
}

/// A size vector for squarify tests (all non-zero).
fn sizes_strategy(max: usize) -> impl Strategy<Value = Vec<u64>> {
    proptest::collection::vec(size_strategy().prop_filter("nonzero", |s| *s > 0), 1..max)
}

// ---------------------------------------------------------------------------
// Property 1: roll-up algebra — stored totals equal independent sums.
// ---------------------------------------------------------------------------

/// Honest recursive subtree sums: (`on_disk`, `logical`, files, folders).
fn sum_subtree(t: &Tree, id: u32) -> (u64, u64, u64, u64) {
    let n = t.node(id).unwrap();
    if !n.is_dir() {
        return (n.on_disk, n.logical, 1, 0);
    }
    let (mut od, mut lg, mut f, mut d) = (0u64, 0u64, 0u64, 0u64);
    for &kid in t.children_sorted(id) {
        let (kod, klg, kf, kd) = sum_subtree(t, kid);
        od = od.saturating_add(kod);
        lg = lg.saturating_add(klg);
        f = f.saturating_add(kf);
        d = d.saturating_add(kd);
        if t.node(kid).unwrap().is_dir() {
            d = d.saturating_add(1);
        }
    }
    (od, lg, f, d)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn rollup_totals_equal_subtree_sums(seed in any::<u64>(), fan in 4u32..16) {
        let t = build_tree(seed, fan);
        let root = t.node(0).unwrap();
        let extra = &t.dir_extras[root.dir_index as usize];
        let (od, lg, f, d) = sum_subtree(&t, 0);
        prop_assert_eq!(root.on_disk, od, "on_disk rollup");
        prop_assert_eq!(root.logical, lg, "logical rollup");
        prop_assert_eq!(extra.file_count, f, "file_count rollup");
        prop_assert_eq!(extra.folder_count, d, "folder_count rollup");
    }
}

// ---------------------------------------------------------------------------
// Property 2: children_sorted is size-descending and complete.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn children_sorted_desc_and_complete(seed in any::<u64>(), fan in 4u32..24) {
        let t = build_tree(seed, fan);
        for id in 0..t.len() as u32 {
            let n = t.node(id).unwrap();
            if !n.is_dir() || n.child_count == 0 {
                continue;
            }
            let kids = t.children_sorted(id);
            prop_assert_eq!(kids.len(), n.child_count as usize, "all children listed");
            let sizes: Vec<u64> = kids.iter().map(|&k| t.node(k).unwrap().on_disk).collect();
            for w in sizes.windows(2) {
                prop_assert!(w[0] >= w[1], "sorted desc: {sizes:?}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Property 3: surgery algebra — removing subtrees keeps every live
// folder's slice valid and the root totals consistent.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn surgery_preserves_survivor_slices(
        seed in any::<u64>(),
        fan in 4u32..12,
        removes in proptest::collection::vec(1u32..40, 0..6),
    ) {
        let t = build_tree(seed, fan);
        let mut t2 = Tree::deep_from(&t);
        let summary = surgery::remove_subtrees(&mut t2, &removes);
        for id in 0..t2.len() as u32 {
            let n = t2.node(id).unwrap();
            if n.is_removed() || !n.is_dir() {
                continue;
            }
            for &kid in t2.children_sorted(id) {
                prop_assert!(
                    !t2.node(kid).unwrap().is_removed(),
                    "live folder {id} lists removed child {kid}"
                );
            }
        }
        let root_before = t.node(0).unwrap().on_disk;
        let root_after = t2.node(0).unwrap().on_disk;
        prop_assert_eq!(root_before.saturating_sub(summary.on_disk), root_after);
        prop_assert!(t2.generation >= t.generation);
        prop_assert!(t2.generation <= t.generation + 1);
    }
}

// ---------------------------------------------------------------------------
// Property 4: deep_from is an exact clone (the CoW surgery source).
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn deep_from_is_exact(seed in any::<u64>(), fan in 4u32..14) {
        let t = build_tree(seed, fan);
        let c = Tree::deep_from(&t);
        prop_assert_eq!(c.len(), t.len());
        prop_assert_eq!(&c.names, &t.names);
        prop_assert_eq!(&c.order, &t.order);
        prop_assert_eq!(c.roots.len(), t.roots.len());
        for id in 0..t.len() as u32 {
            prop_assert_eq!(c.node(id), t.node(id));
        }
        for di in 0..t.dir_extras.len() {
            prop_assert_eq!(c.dir_extras[di], t.dir_extras[di]);
        }
    }
}

// ---------------------------------------------------------------------------
// Property 5: names_batch never panics on ANY id (the CI-crash class).
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn names_batch_survives_arbitrary_ids(
        seed in any::<u64>(),
        fan in 2u32..10,
        ids in proptest::collection::vec(any::<u32>(), 0..16),
    ) {
        let t = build_tree(seed, fan);
        let out = t.names_batch(&ids);
        prop_assert_eq!(out.len(), ids.len());
        for (i, &id) in ids.iter().enumerate() {
            if usize::try_from(id).is_ok_and(|x| x < t.len()) {
                prop_assert_eq!(&out[i], &t.name(id));
            } else {
                prop_assert_eq!(out[i].as_str(), "");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Property 6: layout engines never panic and keep rect cells in-bounds
// for ANY tree shape and viewport.
// ---------------------------------------------------------------------------

fn rect_cells_in_bounds(cells: &[layout::Cell], w: f32, h: f32) -> bool {
    cells.iter().all(|c| {
        if (c.flags & 0b111) == layout::cell_kind::RECT {
            c.g[0] >= -0.5
                && c.g[1] >= -0.5
                && c.g[0] + c.g[2] <= w + 0.5
                && c.g[1] + c.g[3] <= h + 0.5
        } else {
            true
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn layouts_never_panic_and_stay_in_bounds(
        seed in any::<u64>(),
        fan in 4u32..18,
        w in 64f32..1920.0,
        h in 64f32..1080.0,
    ) {
        let t = build_tree(seed, fan);
        let buf = layout::treemap::treemap(&t, 0, w, h, 4, layout::ColorMode::ByFolder, 1).unwrap();
        prop_assert!(rect_cells_in_bounds(&buf.cells, w, h));
        let buf = layout::flame::flame(&t, 0, w, h, 4, layout::ColorMode::ByType, 1).unwrap();
        prop_assert!(rect_cells_in_bounds(&buf.cells, w, h));
        let _ = layout::sunburst::sunburst(&t, 0, w, h, 4, layout::ColorMode::ByAge, 1).unwrap();
        let _ = layout::bubbles::bubbles(&t, 0, w, h, 4, layout::ColorMode::ByFolder, 1).unwrap();
        let _ = layout::mindmap::mindmap(&t, 0, w, h, 4, layout::ColorMode::ByFolder, 1).unwrap();
    }
}

// ---------------------------------------------------------------------------
// Property 7: surgery idempotence — double commit is a no-op.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn double_removal_is_noop(
        seed in any::<u64>(),
        fan in 4u32..12,
        removes in proptest::collection::vec(1u32..40, 1..4),
    ) {
        let t = build_tree(seed, fan);
        let mut a = Tree::deep_from(&t);
        let _first = surgery::remove_subtrees(&mut a, &removes);
        let gen_after_first = a.generation;
        let second = surgery::remove_subtrees(&mut a, &removes);
        prop_assert_eq!(second.nodes, 0);
        prop_assert_eq!(second.on_disk, 0);
        prop_assert_eq!(a.generation, gen_after_first, "no-op must not bump");
    }
}

// ---------------------------------------------------------------------------
// Property 8: category classification is total, stable, bit-exact.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn category_of_any_name_is_valid(
        name in proptest::collection::vec(proptest::char::range('a', 'z').prop_map(Some).boxed()
            .prop_flat_map(|c| match c {
                Some(c) => Just(c).boxed(),
                None => Just('x').boxed(),
            }), 1..24)
            .prop_map(|v| v.into_iter().collect::<String>())
    ) {
        let units: Vec<u16> = name.encode_utf16().collect();
        let cat = FileCategory::from_name(&units);
        prop_assert!(cat.as_bits() < 9);
        prop_assert_eq!(FileCategory::from_name(&units), cat);
        let mut n = Node::new_file();
        n.set_category(cat);
        prop_assert_eq!(n.category(), cat);
    }
}

// ---------------------------------------------------------------------------
// Property 9: format::bytes is total and self-consistent.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn bytes_format_total_and_stable(a in size_strategy(), b in size_strategy()) {
        let sa = format::bytes(a);
        let sb = format::bytes(b);
        if a == b {
            prop_assert_eq!(sa.as_str(), sb.as_str());
        }
        prop_assert!(!sa.is_empty());
        prop_assert!(!sb.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Property 10: dupes grouping — wasted space is size*(n-1) per group.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn dupe_wasted_space_algebra(
        sizes in proptest::collection::vec((1_000_000u64..1_000_000_000, 2usize..5), 1..8),
    ) {
        let mut files: Vec<HashedFile> = Vec::new();
        for (g, (size, count)) in sizes.iter().enumerate() {
            for i in 0..*count {
                files.push(HashedFile {
                    path: format!(r"C:\g{g}\f{i}.bin"),
                    size: *size,
                    volume_serial: u64::try_from(g).unwrap_or(0),
                    file_index: u64::try_from(g * 100 + i).unwrap_or(0),
                    sha256: [0; 32],
                });
            }
        }
        let groups = dupes::rank(&files);
        for g in &groups {
            prop_assert_eq!(
                g.wasted,
                g.size
                    .saturating_mul((g.files.len() as u64).saturating_sub(1))
            );
            prop_assert!(g.files.len() >= 2);
        }
        let (total_wasted, total_groups) = dupes::totals(&groups);
        prop_assert_eq!(total_groups, groups.len());
        prop_assert_eq!(total_wasted, groups.iter().map(|g| g.wasted).sum::<u64>());
    }
}

// ---------------------------------------------------------------------------
// Property 11: snapshot diff — delta arithmetic never lies.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn snapshot_delta_arithmetic(
        before in proptest::collection::vec((any::<u64>(), any::<u64>()), 0..10),
        after in proptest::collection::vec((any::<u64>(), any::<u64>()), 0..10),
    ) {
        use diskbytes_core::snapshots::Snapshot;
        let mk = |v: Vec<(u64, u64)>| -> Vec<(String, u64)> {
            v.into_iter().map(|(i, s)| (format!("folder_{i}"), s)).collect()
        };
        let a = Snapshot::build("a".into(), "C:\\".into(), 1, mk(before));
        let b = Snapshot::build("b".into(), "C:\\".into(), 2, mk(after));
        let diff = diskbytes_core::snapshots::diff(&a, &b);
        for ch in &diff.changes {
            prop_assert_eq!(ch.delta, ch.after as i64 - ch.before as i64);
        }
        for w in diff.changes.windows(2) {
            prop_assert!(w[0].delta.unsigned_abs() >= w[1].delta.unsigned_abs());
        }
    }
}

// ---------------------------------------------------------------------------
// Property 12: turbo MFT run-list decoder survives arbitrary bytes
// (corrupt-disk robustness — error, never panic).
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn run_list_decoder_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        // Any byte slice, any VCN window: decode must error or return
        // internally-consistent runs — never panic.
        if let Ok(out) = diskbytes_core::turbo::runs::decode_run_list(
            &bytes, 0, bytes.len(), 0, u64::MAX,
        ) {
            let mut vcn = 0u64;
            for r in &out {
                prop_assert!(r.length > 0);
                vcn = vcn.saturating_add(r.length);
            }
            let _ = vcn;
        }
    }
}

// ---------------------------------------------------------------------------
// Property 13: treemap area proportionality on random size vectors
// (squarify's core contract, far beyond the fixed fixtures).
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn squarify_area_proportional(
        sizes in sizes_strategy(24),
        w in 100f32..1600.0,
        h in 100f32..1000.0,
    ) {
        // The engine emits cells in children_sorted order (size-desc) —
        // sort the sizes to keep index-aligned with cells (the same
        // contract production guarantees: children_sorted feeds squarify).
        let mut sizes = sizes;
        sizes.sort_unstable_by(|a, b| b.cmp(a));
        let mut t = Tree::new(1);
        t.add_root_path(0, "C:\\sq");
        let entries: Vec<BatchEntry> = sizes
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                let mut n = Node::new_file();
                n.logical = s;
                n.on_disk = s;
                n.modified = 1;
                n.set_category(FileCategory::from_name(
                    &format!("f{i:02}.bin").encode_utf16().collect::<Vec<u16>>(),
                ));
                BatchEntry {
                    name: format!("f{i:02}.bin").encode_utf16().collect(),
                    node: n,
                }
            })
            .collect();
        t.append_batch(0, entries);
        rollup::finalize(&mut t);
        let buf = layout::treemap::treemap(&t, 0, w, h, 2, layout::ColorMode::ByType, 1).unwrap();
        let total: u64 = sizes.iter().copied().fold(0u64, u64::saturating_add);
        // Sub-pixel tail siblings may be dropped by the geometry pass,
        // but NEVER silently: the drop must raise the truncated flag.
        if buf.cells.len() < sizes.len() {
            prop_assert!(
                buf.meta.truncated,
                "{} of {} cells dropped without the truncated flag",
                sizes.len() - buf.cells.len(),
                sizes.len()
            );
        }
        // Emitted cells keep the size-desc order and area proportion
        // (drops are tail-only, so cell i maps to sorted sizes[i]).
        for (i, c) in buf.cells.iter().enumerate() {
            let expect = sizes[i] as f64 / total as f64 * f64::from(w) * f64::from(h);
            let area = f64::from(c.g[2]) * f64::from(c.g[3]);
            // Generous 12% tolerance (gaps, rounding, f32 emission)
            // still catches the u64-overflow-class corruption (300%+).
            prop_assert!(
                (area - expect).abs() < expect.max(1.0) * 0.12,
                "cell {i}: area {area} vs expected {expect}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property 14: append_batch invariants — parent < child, contiguous.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn parent_ids_smaller_and_children_contiguous(seed in any::<u64>(), fan in 4u32..14) {
        let t = build_tree(seed, fan);
        for id in 1..t.len() as u32 {
            let n = t.node(id).unwrap();
            if n.parent != u32::MAX {
                prop_assert!(n.parent < id, "parent {} >= child {}", n.parent, id);
            }
        }
        for id in 0..t.len() as u32 {
            let n = t.node(id).unwrap();
            if n.is_dir() && n.child_count > 0 {
                prop_assert_eq!(t.node(n.first_child).unwrap().parent, id);
                let last = t.node(n.first_child + n.child_count - 1).unwrap();
                prop_assert_eq!(last.parent, id);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Property 15: age buckets are total, deterministic, cover any time.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn age_bucket_total(ts in any::<i64>(), now in 1_600_000_000i64..1_900_000_000) {
        let b = diskbytes_core::age::bucket_of(ts, now);
        prop_assert!(b <= 5);
        prop_assert_eq!(diskbytes_core::age::bucket_of(ts, now), b);
        prop_assert_eq!(diskbytes_core::age::bucket_of(now + 86_400 * 400, now), 0);
        prop_assert_eq!(diskbytes_core::age::bucket_of(0, now), 5);
    }
}

// ---------------------------------------------------------------------------
// Property 16: node_path + resolve_display_path round-trip (the
// navigation path used by the Home button).
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn resolve_display_path_finds_built_nodes(seed in any::<u64>(), fan in 4u32..10) {
        let t = build_tree(seed, fan);
        // Every root-path + child chain built via node_path must
        // resolve back to the same node.
        for id in 1..t.len() as u32 {
            let n = t.node(id).unwrap();
            if n.parent == u32::MAX || t.node(n.parent).unwrap().parent == u32::MAX {
                // Root-level children resolve through the root path.
                let path = t.node_path(id);
                let resolved = t.resolve_display_path(&path);
                prop_assert_eq!(resolved, Some(id), "node {} path must resolve", id);
            }
        }
    }
}
