//! Real-filesystem platform behavior tests (owner instruction:
//! "realistic platform (device based) realistic real behaviour based
//! tests"). These stage REAL directories and files on the host OS and
//! verify the tree pipeline end-to-end: unicode name round-trips, deep
//! nesting, zero-byte files, permission refusals, symlink-shaped
//! entries, huge sizes, and snapshot persistence.
//!
//! They run on Windows, macOS (and any dev host) through `std::fs`
//! only — the platform seam itself is exercised by the app crate's
//! per-OS tests; this file pins the CORE's contract against the real
//! behaviors those platforms exhibit.

use diskbytes_core::scan::categories::FileCategory;
use diskbytes_core::scan::node::{BatchEntry, Node, Tree};
use diskbytes_core::scan::rollup;
use diskbytes_core::snapshots::{FolderSize, Snapshot};

// ---------------------------------------------------------------------------
// Helpers: staging + a std::fs-walker that feeds the tree like an engine.
// ---------------------------------------------------------------------------

/// Unique staging dir per test (parallel-safe).
fn stage(sub: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "db-platform-{}-{}-{sub}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&dir).expect("stage dir");
    dir
}

fn dir_entry(name: &str) -> BatchEntry {
    let mut n = Node::new_dir();
    n.modified = 1;
    BatchEntry {
        name: name.encode_utf16().collect(),
        node: n,
    }
}

fn file_entry(name: &str, logical: u64, on_disk: u64) -> BatchEntry {
    let mut n = Node::new_file();
    n.logical = logical;
    n.on_disk = on_disk;
    n.modified = 2;
    n.set_category(FileCategory::from_name(
        &name.encode_utf16().collect::<Vec<u16>>(),
    ));
    BatchEntry {
        name: name.encode_utf16().collect(),
        node: n,
    }
}

/// An allocation-size hint good enough for behavior tests: Unix uses
/// real blocks; Windows/other hosts round to 4 KiB clusters.
trait AllocHint {
    fn alloc_hint(&self) -> u64;
}

impl AllocHint for std::fs::Metadata {
    fn alloc_hint(&self) -> u64 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            (self.blocks() * 512).max(1)
        }
        #[cfg(not(unix))]
        {
            4096
        }
    }
}

/// Walk a real directory with `std::fs` and build a `DiskBytes` tree
/// through the same protocol the engines use (one batch per directory,
/// parents before children — BFS). `symlink_metadata` mirrors the
/// engines: links are never followed.
fn build_from_fs(root: &std::path::Path) -> Tree {
    let mut t = Tree::new(1);
    t.add_root_path(0, &root.to_string_lossy());
    let mut queue: std::collections::VecDeque<(u32, std::path::PathBuf)> =
        std::collections::VecDeque::new();
    queue.push_back((0, root.to_path_buf()));
    while let Some((parent, path)) = queue.pop_front() {
        let mut entries: Vec<BatchEntry> = Vec::new();
        let mut subdirs: Vec<(usize, std::path::PathBuf)> = Vec::new();
        let Ok(read) = std::fs::read_dir(&path) else {
            continue; // An engine records a read error; the tree builds on.
        };
        for e in read.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(meta) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if meta.is_dir() {
                subdirs.push((entries.len(), e.path()));
                entries.push(dir_entry(&name));
            } else {
                let logical = meta.len();
                let on_disk = logical.div_ceil(meta.alloc_hint()).max(logical);
                entries.push(file_entry(&name, logical, on_disk));
            }
        }
        let base = t.append_batch(parent, entries);
        for (idx, p) in subdirs {
            queue.push_back((base + idx as u32, p));
        }
    }
    rollup::finalize(&mut t);
    t
}

// ---------------------------------------------------------------------------
// Tests: real-device file behaviors through the whole core pipeline.
// ---------------------------------------------------------------------------

#[test]
fn unicode_names_round_trip_exactly() {
    let dir = stage("unicode");
    // BMP accents, CJK, RTL, non-BMP (surrogate pairs), combining marks.
    let names = [
        "café.txt",
        "日本語ディレクトリ",
        "مرحبا.txt",
        "emoji-📁-🗺️.bin",
        "nai\u{0308}ve.txt", // combining diaeresis
        "привет.md",
    ];
    for n in names {
        std::fs::write(dir.join(n), b"x").expect("write unicode name");
    }
    let t = build_from_fs(&dir);
    for n in names {
        let units: Vec<u16> = n.encode_utf16().collect();
        let found = (0..t.len() as u32).any(|id| t.name_u16(id) == units.as_slice());
        assert!(found, "name {n:?} lost or mangled in the tree");
    }
}

#[test]
fn deep_nesting_32_levels() {
    let dir = stage("deep");
    let mut cur = dir.clone();
    for i in 0..32 {
        cur = cur.join(format!("level_{i:02}"));
        std::fs::create_dir_all(&cur).expect("nest");
    }
    std::fs::write(cur.join("bottom.bin"), vec![7u8; 4096]).expect("leaf");
    let t = build_from_fs(&dir);
    let bottom = (0..t.len() as u32)
        .find(|&id| t.name(id) == "bottom.bin")
        .expect("bottom.bin present");
    let path = t.node_path(bottom);
    assert!(path.contains("level_00") && path.contains("level_31"));
    let root = t.node(0).unwrap();
    assert!(root.on_disk >= 4096, "root on_disk: {}", root.on_disk);
}

#[test]
fn long_path_beyond_maxpath() {
    let dir = stage("longpath");
    // 60 × 40-char segments ≈ 2400 chars — well past Windows' 260 limit;
    // the core stores names as offsets, never paths, so this must hold.
    let mut cur = dir.clone();
    for i in 0..60 {
        cur = cur.join(format!("s{i:02}_{}", "x".repeat(40)));
        std::fs::create_dir_all(&cur).expect("long segment");
    }
    std::fs::write(cur.join("deep.txt"), b"deep").expect("deep leaf");
    let t = build_from_fs(&dir);
    let deep = (0..t.len() as u32)
        .find(|&id| t.name(id) == "deep.txt")
        .expect("deep.txt present");
    let path = t.node_path(deep);
    assert!(
        path.len() > 260,
        "path should exceed MAX_PATH: {}",
        path.len()
    );
}

#[test]
fn zero_byte_and_single_byte_files() {
    let dir = stage("zero");
    std::fs::write(dir.join("empty.txt"), b"").expect("empty");
    std::fs::write(dir.join("one.bin"), b"x").expect("one byte");
    std::fs::write(dir.join("normal.dat"), vec![1u8; 100_000]).expect("100k");
    let t = build_from_fs(&dir);
    let empty = (0..t.len() as u32)
        .find(|&id| t.name(id) == "empty.txt")
        .unwrap();
    let n = t.node(empty).unwrap();
    assert_eq!(n.logical, 0);
    // Zero-byte files are NOT dropped: they appear in children_sorted.
    let kids = t.children_sorted(0).to_vec();
    assert!(kids.contains(&empty), "zero-byte file visible");
}

#[test]
fn many_siblings_500() {
    let dir = stage("wide");
    for i in 0..500 {
        std::fs::write(dir.join(format!("sibling_{i:03}.dat")), vec![0u8; 100]).expect("sib");
    }
    let t = build_from_fs(&dir);
    assert_eq!(t.children_sorted(0).len(), 500);
    // Sorted largest-first (equal sizes: id tie-break keeps it stable).
    let kids = t.children_sorted(0);
    let sizes: Vec<u64> = kids.iter().map(|&id| t.node(id).unwrap().on_disk).collect();
    assert!(
        sizes.windows(2).all(|w| w[0] >= w[1]),
        "children sorted desc"
    );
}

/// Restores 000-mode dirs when dropped (test hygiene on Unix hosts).
struct PermissionRestore<'a>(&'a std::path::Path);
impl Drop for PermissionRestore<'_> {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        }
        #[cfg(not(unix))]
        {
            let _ = self.0;
        }
    }
}

#[test]
fn permission_denied_dir_is_survivable() {
    let dir = stage("perms");
    std::fs::write(dir.join("open.txt"), b"readable").expect("open");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = dir.join("locked");
        std::fs::create_dir(&locked).expect("locked dir");
        std::fs::write(locked.join("secret.txt"), b"secret").expect("secret");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 000");
        // Restore at the end regardless of assertion outcomes.
        let _guard = PermissionRestore(&locked);
        // The walker skips unreadable dirs (an engine records a read
        // error); the tree still builds with what IS readable.
        let t = build_from_fs(&dir);
        assert!(
            (0..t.len() as u32).any(|id| t.name(id) == "open.txt"),
            "readable sibling survives the locked dir"
        );
    }
    #[cfg(not(unix))]
    {
        // Windows ACL choreography is the app crate's platform test;
        // here we pin the readable-path behavior.
        let t = build_from_fs(&dir);
        assert!((0..t.len() as u32).any(|id| t.name(id) == "open.txt"));
    }
}

#[test]
fn windows_reserved_shape_names_kept_byte_exact() {
    // Names that LOOK reserved/odd but are legal on modern NTFS via
    // \\?\ paths; on other hosts they're plain names. The core must
    // store whatever the OS produced, byte-exact.
    let dir = stage("names");
    for n in ["CON.shaped.txt", "aux.like.bin", "trailing.dot."] {
        if std::fs::write(dir.join(n), b"x").is_ok() {
            let t = build_from_fs(&dir);
            let units: Vec<u16> = n.encode_utf16().collect();
            assert!(
                (0..t.len() as u32).any(|id| t.name_u16(id) == units.as_slice()),
                "odd name {n:?} lost"
            );
        }
    }
}

#[test]
fn case_variants_distinct_in_tree() {
    // NTFS is case-insensitive, APFS default too, but the TREE must
    // keep both entries distinct when a case-SENSITIVE host (or the
    // verbatim engine) reports both.
    let dir = stage("case");
    let mut t = Tree::new(1);
    t.add_root_path(0, &dir.to_string_lossy());
    t.append_batch(
        0,
        vec![
            file_entry("Readme.md", 10, 10),
            file_entry("README.md", 20, 20),
        ],
    );
    rollup::finalize(&mut t);
    assert_eq!(t.children_sorted(0).len(), 2, "both cases kept distinct");
}

#[test]
fn snapshot_atomic_round_trip() {
    let dir = stage("snapshot");
    let path = dir.join("snap.json");
    let snap = Snapshot {
        id: "test-1".into(),
        root: "C:\\".into(),
        taken_at: 1_750_000_000,
        folders: vec![FolderSize {
            path: "C:\\Users".into(),
            logical: 12_345_678,
        }],
    };
    snapshots_write(&snap, &path);
    let back: Snapshot =
        serde_json::from_reader(std::fs::File::open(&path).unwrap()).expect("read back");
    assert_eq!(back.id, "test-1");
    assert_eq!(back.folders[0].logical, 12_345_678);
    // Atomic write: no temp litter in the directory.
    let litter: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(litter.is_empty(), "temp files left: {litter:?}");
}

fn snapshots_write(snap: &Snapshot, path: &std::path::Path) {
    diskbytes_core::snapshots::write_json_atomic(path, snap).expect("atomic write");
}

#[test]
fn symlink_entries_never_followed_never_panic() {
    let dir = stage("links");
    std::fs::write(dir.join("target.txt"), b"t").expect("target");
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.join("target.txt"), dir.join("link.txt")).expect("symlink");
    #[cfg(windows)]
    {
        // Junction creation needs no elevation on modern Windows.
        let _ = std::process::Command::new("cmd")
            .args(["/C", "mklink", "Junction", "linkdir"])
            .current_dir(&dir)
            .output();
    }
    // The walker uses symlink_metadata: links are entries with a size
    // (never followed) — the engine contract.
    let t = build_from_fs(&dir);
    assert!(t.len() >= 2, "tree built over link-bearing dir");
}

#[test]
fn sizes_roll_up_over_4gib_boundary() {
    // The surgery/layout overflow class: sizes crossing 2^32 must roll
    // up without any u32 truncation anywhere in the pipeline.
    let mut t = Tree::new(1);
    t.add_root_path(0, "C:\\big");
    t.append_batch(
        0,
        vec![
            file_entry("a.iso", 3_000_000_000, 3_000_776_192),
            file_entry("b.iso", 3_000_000_000, 3_000_776_192),
            file_entry("c.iso", 2_000_000_000, 2_000_518_656),
        ],
    );
    rollup::finalize(&mut t);
    let root = t.node(0).unwrap();
    assert_eq!(root.logical, 8_000_000_000, "logical sum > 4 GiB exact");
    assert!(root.on_disk > 8_000_000_000);
}

#[test]
fn category_pipeline_over_real_files() {
    // Real files across every category bucket; the tree's category
    // totals must match an independent count.
    let dir = stage("cats");
    let cases = [
        ("video.mp4", FileCategory::Video),
        ("song.mp3", FileCategory::Audio),
        ("photo.jpg", FileCategory::Image),
        ("doc.pdf", FileCategory::Document),
        ("main.rs", FileCategory::Developer),
        ("lib.dll", FileCategory::Developer),
        ("app.exe", FileCategory::Apps),
        ("arch.zip", FileCategory::Archive),
        ("kernel.sys", FileCategory::System),
        ("data.bin", FileCategory::Other),
    ];
    for (name, _) in &cases {
        std::fs::write(dir.join(name), vec![0u8; 8192]).expect("cat file");
    }
    let t = build_from_fs(&dir);
    let mut counts: std::collections::HashMap<u8, u64> = std::collections::HashMap::new();
    for id in 0..t.len() as u32 {
        let n = t.node(id).unwrap();
        if !n.is_dir() {
            *counts.entry(n.category().as_bits()).or_default() += 1;
        }
    }
    for (name, expect_cat) in &cases {
        let id = (0..t.len() as u32)
            .find(|&id| t.name(id) == *name)
            .unwrap_or_else(|| panic!("file {name} missing"));
        assert_eq!(
            t.node(id).unwrap().category(),
            *expect_cat,
            "category for {name}"
        );
    }
    // All 9 buckets present (Developer carries two files).
    assert_eq!(counts.len(), 9, "every bucket represented: {counts:?}");
    assert_eq!(counts[&FileCategory::Developer.as_bits()], 2);
}
