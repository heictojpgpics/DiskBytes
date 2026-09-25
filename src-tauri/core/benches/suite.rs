//! Criterion performance suite (production perf gates — doc 09 §2 bench
//! battery). Measures the hot paths that matter at 1M+ nodes: tree
//! building (batch append), roll-up finalize (reverse pass + CSR
//! order), layout engines (treemap/sunburst/flame), surgery (the
//! post-cleanup `CoW` path), quickwins resolve, and duplicate ranking.
//!
//! Run locally: `cargo bench -p diskbytes-core`
//! CI: `.github/workflows/test-matrix.yml` (benchmark job, both OSes).

// Bench-only lint posture: criterion's `b.iter(|| {...})` closures end
// every bench function (semicolon_if_nothing_returned fires on each),
// and macro expansions trip missing_docs. These allow the SUITE to keep
// its idiomatic criterion shape without per-site annotations.
#![allow(missing_docs, clippy::semicolon_if_nothing_returned)]

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use diskbytes_core::scan::node::{BatchEntry, Node, Tree};
use diskbytes_core::scan::rollup;

/// A deterministic synthetic tree: `dirs` directories × `files_per_dir`
/// files, sizes derived from a cheap LCG so every run is identical.
fn synthetic_tree(dirs: u32, files_per_dir: u32) -> Tree {
    let mut t = Tree::new(1);
    t.add_root_path(0, "C:\\bench");
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        seed >> 33
    };
    let mut entries: Vec<BatchEntry> = Vec::with_capacity(files_per_dir as usize);
    let dir_chunk: Vec<BatchEntry> = (0..dirs)
        .map(|d| BatchEntry {
            name: format!("dir_{d:04}").encode_utf16().collect(),
            node: Node::new_dir(),
        })
        .collect();
    let base = t.append_batch(0, dir_chunk);
    for d in 0..dirs {
        entries.clear();
        let mut i = 0u32;
        while i < files_per_dir {
            let sz = next().max(1) % (8 * 1024 * 1024);
            let mut n = Node::new_file();
            n.logical = sz;
            n.on_disk = sz.div_ceil(4096) * 4096;
            n.modified = 1_700_000_000 + i64::from(d * files_per_dir + i);
            n.set_category(diskbytes_core::scan::categories::FileCategory::from_name(
                &format!("f_{i}.bin").encode_utf16().collect::<Vec<u16>>(),
            ));
            entries.push(BatchEntry {
                name: format!("file_{i:05}.bin").encode_utf16().collect(),
                node: n,
            });
            i += 1;
        }
        t.append_batch(base + d, entries.clone());
    }
    t
}

/// Build + finalize in one go (the full scan-pipeline shape minus IO).
fn bench_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("pipeline");
    group.throughput(Throughput::Elements(1_000_000));
    group.bench_function("build_1m_append+finalize", |b| {
        b.iter_with_large_drop(|| {
            let mut t = synthetic_tree(1000, 1000);
            rollup::finalize(&mut t);
            t
        })
    });
    group.finish();
}

/// Roll-up finalize alone (the reverse linear pass + per-folder CSR
/// sort) on a pre-built 1M-node tree.
fn bench_finalize(c: &mut Criterion) {
    let mut group = c.benchmark_group("pipeline");
    group.throughput(Throughput::Elements(1_000_000));
    let template = synthetic_tree(1000, 1000);
    group.bench_function("finalize_1m", |b| {
        b.iter_batched_ref(
            || Tree::deep_from(&template),
            rollup::finalize,
            criterion::BatchSize::LargeInput,
        )
    });
    group.finish();
}

/// Layout engines over the full 1M-node tree (MAX_CELLS-bounded output,
/// but the traversal + geometry cost is real).
fn bench_layout(c: &mut Criterion) {
    let mut t = synthetic_tree(1000, 1000);
    rollup::finalize(&mut t);
    let mut group = c.benchmark_group("layout");
    group.throughput(Throughput::Elements(1_000_000));
    group.bench_function("treemap_1m_depth4", |b| {
        b.iter(|| {
            diskbytes_core::layout::treemap::treemap(
                &t,
                0,
                1600.0,
                1000.0,
                4,
                diskbytes_core::layout::ColorMode::ByFolder,
                1,
            )
        })
    });
    group.bench_function("sunburst_1m_depth4", |b| {
        b.iter(|| {
            diskbytes_core::layout::sunburst::sunburst(
                &t,
                0,
                900.0,
                900.0,
                4,
                diskbytes_core::layout::ColorMode::ByFolder,
                1,
            )
        })
    });
    group.bench_function("flame_1m_depth4", |b| {
        b.iter(|| {
            diskbytes_core::layout::flame::flame(
                &t,
                0,
                1600.0,
                1000.0,
                4,
                diskbytes_core::layout::ColorMode::ByFolder,
                1,
            )
        })
    });
    group.finish();
}

/// Treemap scaling with sibling count (squarify's row-splitting kernel
/// exercised through the public API: wide folders vs deep trees).
fn bench_treemap_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("treemap_scaling");
    for &(dirs, files) in &[(1u32, 10_000u32), (10, 1_000), (100, 100), (1_000, 10)] {
        let mut t = synthetic_tree(dirs, files);
        rollup::finalize(&mut t);
        group.throughput(Throughput::Elements(u64::from(dirs * files)));
        group.bench_with_input(
            BenchmarkId::new("treemap", format!("{dirs}x{files}")),
            &t,
            |b, t| {
                b.iter(|| {
                    diskbytes_core::layout::treemap::treemap(
                        t,
                        0,
                        1600.0,
                        1000.0,
                        4,
                        diskbytes_core::layout::ColorMode::ByFolder,
                        1,
                    )
                })
            },
        );
    }
    group.finish();
}

/// Post-cleanup surgery on a 1M-node tree (`CoW` deep-copy + remove +
/// compact) — the `commit_cleanup` hot path.
fn bench_surgery(c: &mut Criterion) {
    let mut t = synthetic_tree(1000, 1000);
    rollup::finalize(&mut t);
    let ids: Vec<u32> = (0..100).map(|d| 1 + d).collect();
    let mut group = c.benchmark_group("surgery");
    group.throughput(Throughput::Elements(1_000_000));
    group.bench_function("deep_copy+remove_100k_nodes", |b| {
        b.iter_batched_ref(
            || Tree::deep_from(&t),
            |copy| {
                diskbytes_core::scan::surgery::remove_subtrees(copy, &ids);
            },
            criterion::BatchSize::LargeInput,
        )
    });
    group.finish();
}

/// Quickwins resolve over the whole tree (the sidebar first-paint cost).
fn bench_quickwins(c: &mut Criterion) {
    let mut t = synthetic_tree(200, 200);
    rollup::finalize(&mut t);
    let mut env = std::collections::HashMap::new();
    env.insert(
        "LOCALAPPDATA".to_string(),
        "C:\\Users\\bench\\AppData\\Local".to_string(),
    );
    env.insert("USERPROFILE".to_string(), "C:\\Users\\bench".to_string());
    let mut group = c.benchmark_group("quickwins");
    group.throughput(Throughput::Elements(40_200));
    group.bench_function("resolve_40k_nodes", |b| {
        b.iter(|| diskbytes_core::quickwins::resolve(&t, &env, 1))
    });
    group.finish();
}

/// Duplicate ranking (size bucketing + wasted-space ranking) over a
/// hashed-file list — pass-2 of the duplicates pipeline.
fn bench_dupes(c: &mut Criterion) {
    // 50k files, sizes cycling over 10k distinct values -> 5-member
    // buckets on average (the realistic dupe density).
    let files: Vec<diskbytes_core::dupes::HashedFile> = (0..50_000u32)
        .map(|i| diskbytes_core::dupes::HashedFile {
            path: format!(r"C:\bench\file_{i:05}.bin"),
            size: 4096 + u64::from(i % 10_000) * 512,
            volume_serial: 1,
            file_index: u64::from(i),
            sha256: [u8::try_from(i % 256).unwrap_or(0); 32],
        })
        .collect();
    let mut group = c.benchmark_group("dupes");
    group.throughput(Throughput::Elements(50_000));
    group.bench_function("rank_50k_files", |b| {
        b.iter(|| diskbytes_core::dupes::rank(&files))
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_build,
    bench_finalize,
    bench_layout,
    bench_treemap_scaling,
    bench_surgery,
    bench_quickwins,
    bench_dupes
);
criterion_main!(benches);
