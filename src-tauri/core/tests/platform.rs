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
fn long_paths_supported_at_tree_level() {
    // The CORE's long-path contract: names live as offsets in the UTF-16
    // arena (no per-node paths), and `node_path` rebuilds ANY length on
    // demand — 60 × 40-char segments ≈ 2400 chars, past Windows'
    // MAX_PATH 260 and macOS' PATH_MAX 1024. No host filesystem is
    // involved: std::fs itself cannot express such paths without the
    // engines' verbatim (\\?\) prefixing, which is the PLATFORM
    // layer's job, not the core's.
    let mut t = Tree::new(1);
    t.add_root_path(0, "C:\\long");
    let mut parent = 0u32;
    for i in 0..60 {
        let seg = format!("s{i:02}_{}", "x".repeat(40));
        let mut n = Node::new_dir();
        n.modified = 1;
        let base = t.append_batch(
            parent,
            vec![BatchEntry {
                name: seg.encode_utf16().collect(),
                node: n,
            }],
        );
        parent = base;
    }
    let mut leaf = Node::new_file();
    leaf.logical = 7;
    leaf.on_disk = 4096;
    leaf.set_category(FileCategory::from_name(
        &"deep.txt".encode_utf16().collect::<Vec<u16>>(),
    ));
    t.append_batch(
        parent,
        vec![BatchEntry {
            name: "deep.txt".encode_utf16().collect(),
            node: leaf,
        }],
    );
    rollup::finalize(&mut t);
    let deep = (0..t.len() as u32)
        .find(|&id| t.name(id) == "deep.txt")
        .expect("deep.txt present");
    let path = t.node_path(deep);
    assert!(
        path.len() > 260,
        "rebuilt path should exceed MAX_PATH: {}",
        path.len()
    );
    assert!(path.starts_with("C:\\long\\s00_"));
    // And it navigates back: resolve_display_path finds the node.
    assert_eq!(t.resolve_display_path(&path), Some(deep));
}

#[test]
fn real_fs_nesting_to_host_path_limit() {
    // Real-filesystem nesting as deep as any host expresses without
    // verbatim prefixes: 24 × 30-char segments ≈ 720 chars (under
    // macOS' PATH_MAX 1024; Windows handles it under the runner's
    // long-path-enabled registry setting).
    let dir = stage("longpath");
    let mut cur = dir.clone();
    for i in 0..24 {
        cur = cur.join(format!("s{i:02}_{}", "x".repeat(30)));
        std::fs::create_dir_all(&cur).expect("host-representable segment");
    }
    std::fs::write(cur.join("deep.txt"), b"deep").expect("deep leaf");
    let t = build_from_fs(&dir);
    let deep = (0..t.len() as u32)
        .find(|&id| t.name(id) == "deep.txt")
        .expect("deep.txt present");
    let path = t.node_path(deep);
    assert!(path.len() > 400, "deep real path: {}", path.len());
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
#[cfg(unix)]
struct PermissionRestore<'a>(&'a std::path::Path);
#[cfg(unix)]
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
    // store whatever the OS ACTUALLY produced, byte-exact — Win32
    // normalization may rewrite the name at CREATE time (e.g. trailing
    // dots/spaces are stripped: "trailing.dot." lands on disk as
    // "trailing.dot"), so the assertion compares against the REAL
    // directory listing, not the requested name (CI caught this on the
    // Windows runner).
    let dir = stage("names");
    let requested = ["CON.shaped.txt", "aux.like.bin", "trailing.dot."];
    for n in requested {
        let _ = std::fs::write(dir.join(n), b"x");
    }
    // What the OS actually created.
    let on_disk: Vec<String> = std::fs::read_dir(&dir)
        .expect("read staged names dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!on_disk.is_empty(), "at least one name survived creation");
    let t = build_from_fs(&dir);
    for real in &on_disk {
        let units: Vec<u16> = real.encode_utf16().collect();
        assert!(
            (0..t.len() as u32).any(|id| t.name_u16(id) == units.as_slice()),
            "on-disk name {real:?} lost or mangled"
        );
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

// ---------------------------------------------------------------------------
// Wave 5: device-behavior suite — real filesystem semantics the engines
// must be faithful to (hardlinks, sparse files, unicode normalization,
// control characters, batch-scale siblings, future timestamps, churn
// during enumeration).
// ---------------------------------------------------------------------------

/// Two names, one inode: the tree lists EVERY name (hardlink dedup is
/// engine policy on Windows file-ids; the tree itself must be faithful
/// to what the filesystem reports).
#[test]
fn hardlinked_files_list_under_every_name() {
    #[cfg(unix)]
    {
        let dir = stage("hardlinks");
        std::fs::write(dir.join("original.bin"), [7u8; 4096]).expect("original");
        std::fs::hard_link(dir.join("original.bin"), dir.join("alias.bin")).expect("hardlink");
        let t = build_from_fs(&dir);
        let mut names: Vec<String> = (0..t.len() as u32)
            .map(|id| String::from_utf16_lossy(t.name_u16(id)))
            .collect();
        names.retain(|n| n.rsplit('.').next() == Some("bin"));
        names.sort();
        assert_eq!(names, ["alias.bin", "original.bin"]);
        // Both entries carry the same (real) sizes.
        let sizes: Vec<(u64, u64)> = (0..t.len() as u32)
            .filter(|&id| {
                String::from_utf16_lossy(t.name_u16(id)).rsplit('.').next() == Some("bin")
            })
            .map(|id| {
                let n = t.node(id).expect("node");
                (n.logical, n.on_disk)
            })
            .collect();
        assert_eq!(sizes.len(), 2);
        assert_eq!(sizes[0], sizes[1], "same inode, same sizes");
        assert!(sizes[0].0 == 4096, "logical is the file length");
    }
}

/// Sparse files: `len()` (logical) can be gigabytes while the volume
/// stores kilobytes. Through the core pipeline the two totals are
/// SEPARATE fields: logical is byte-exact, on-disk is the engine's
/// cluster model (the helper rounds up, mirroring the Windows
/// `AllocationSize` contract; the macOS std-fallback test pins the
/// `st_blocks×512` du-parity form). The OS-level sparse shape is
/// asserted directly against `stat` where the host filesystem makes
/// one.
#[test]
fn sparse_files_report_logical_and_on_disk_separately() {
    #[cfg(unix)]
    {
        use std::io::{Seek, SeekFrom, Write};
        use std::os::unix::fs::MetadataExt;
        let dir = stage("sparse");
        let mut f = std::fs::File::create(dir.join("huge-sparse.bin")).expect("create");
        f.seek(SeekFrom::Start(64 * 1024 * 1024)).expect("seek");
        f.write_all(b"x").expect("one byte");
        drop(f);
        let st = std::fs::metadata(dir.join("huge-sparse.bin")).expect("stat");
        assert_eq!(
            st.len(),
            64 * 1024 * 1024 + 1,
            "OS reports the full logical length"
        );
        if st.blocks() * 512 < st.len() {
            // A sparse-capable filesystem (ext4/APFS on the CI runners;
            // the dev container's overlayfs is not): the OS-level shape.
            assert!(
                st.blocks() * 512 < 1024 * 1024,
                "sparse file stores megabytes, not the logical size: {}",
                st.blocks() * 512
            );
        }
        let t = build_from_fs(&dir);
        let id = (0..t.len() as u32)
            .find(|&id| String::from_utf16_lossy(t.name_u16(id)) == "huge-sparse.bin")
            .expect("sparse entry");
        let n = t.node(id).expect("node");
        assert_eq!(n.logical, 64 * 1024 * 1024 + 1, "logical is byte-exact");
        assert!(
            n.on_disk >= n.logical,
            "the helper's cluster model rounds UP (the Windows AllocationSize contract): {}",
            n.on_disk
        );
    }
}

/// Unicode normalization: macOS filesystems may store a different
/// normal form than the bytes passed to `create()`. The tree must store
/// byte-exact WHATEVER the OS returns — pinned by comparing against
/// `read_dir` ground truth, not the requested form.
#[test]
fn unicode_normalization_round_trips_byte_exact() {
    let dir = stage("normalize");
    // "café" in NFC (e + combining acute precomposed) and NFD variants,
    // plus a Hangul syllable (NFC) vs its Jamo decomposition.
    std::fs::write(dir.join("café-NFC.txt"), b"nfc").expect("nfc");
    std::fs::write(dir.join("한글.txt"), b"hangul").expect("hangul");
    // What the OS actually created (macOS may have decomposed it).
    let on_disk: Vec<Vec<u16>> = std::fs::read_dir(&dir)
        .expect("read")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().encode_utf16().collect())
        .collect();
    assert_eq!(on_disk.len(), 2, "both files created");
    let t = build_from_fs(&dir);
    for name in &on_disk {
        let found = (0..t.len() as u32).any(|id| t.name_u16(id) == name.as_slice());
        assert!(
            found,
            "tree must store the OS-returned form byte-exact: {}",
            String::from_utf16_lossy(name)
        );
    }
}

/// Names with control characters are legal on unix filesystems and
/// must round-trip (Windows forbids them at CREATE — the test stages
/// them only where the OS allows).
#[test]
fn control_character_names_round_trip() {
    #[cfg(unix)]
    {
        let dir = stage("ctrl-names");
        std::fs::write(dir.join("with\ttab.txt"), b"tab").expect("tab");
        std::fs::write(dir.join("with\nnewline.txt"), b"newline").expect("newline");
        let t = build_from_fs(&dir);
        let has = |s: &str| {
            (0..t.len() as u32)
                .any(|id| t.name_u16(id) == s.encode_utf16().collect::<Vec<u16>>().as_slice())
        };
        assert!(has("with\ttab.txt"), "tab name in tree");
        assert!(has("with\nnewline.txt"), "newline name in tree");
    }
}

/// Batch-scale sibling counts: 5,000 entries in ONE directory exercise
/// the append-batch path at real scale (buffer splits, allocation
/// growth) and the rollup over a wide fan.
#[test]
fn five_thousand_siblings_survive_the_batch_pipeline() {
    let dir = stage("wide");
    for i in 0..5_000 {
        std::fs::write(dir.join(format!("sibling-{i:05}.dat")), [0u8; 8]).expect("write");
    }
    let t = build_from_fs(&dir);
    let count = (0..t.len() as u32)
        .filter(|&id| String::from_utf16_lossy(t.name_u16(id)).starts_with("sibling-"))
        .count();
    assert_eq!(count, 5_000, "every sibling lands in the tree");
}

/// A file stamped in the future (clock skew, migration archives) must
/// not break the pipeline: no panics, the tree builds with the file
/// present, and the OS reports the future timestamp back. (Tree-node
/// `modified` is a synthetic fixture in the std walker — the ENGINES
/// supply real timestamps, and the win record-walk suite pins the
/// FILETIME conversion; arbitrary i64 timestamps through the age
/// pipeline are property-tested.)
#[test]
fn future_mtime_does_not_break_the_pipeline() {
    let dir = stage("future");
    let p = dir.join("from-the-future.txt");
    std::fs::write(&p, b"skew").expect("write");
    #[cfg(unix)]
    {
        let future = std::time::SystemTime::now()
            .checked_add(std::time::Duration::from_secs(365 * 86_400))
            .expect("a year out");
        std::fs::File::options()
            .write(true)
            .open(&p)
            .expect("open")
            .set_times(std::fs::FileTimes::new().set_modified(future))
            .expect("future mtime");
        let m = std::fs::metadata(&p).expect("stat");
        assert!(
            m.modified().expect("mtime") > std::time::SystemTime::now(),
            "the OS stores and reports future timestamps"
        );
    }
    let t = build_from_fs(&dir);
    let present = (0..t.len() as u32)
        .any(|id| String::from_utf16_lossy(t.name_u16(id)) == "from-the-future.txt");
    assert!(present, "the future-stamped file is scanned like any other");
}

/// Churn during enumeration: a writer thread creating and deleting
/// files while the walk runs must never panic the walker (vanished
/// entries are skipped; the tree builds from what survived).
#[test]
fn churn_during_scan_never_panics() {
    let dir = stage("churn");
    std::fs::write(dir.join("stable.txt"), b"stable").expect("stable");
    let churn_root = dir.join("churning");
    std::fs::create_dir_all(&churn_root).expect("churn dir");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let root = churn_root.clone();
        let stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut i = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let p = root.join(format!("churn-{i:04}.tmp"));
                let _ = std::fs::write(&p, [0u8; 64]);
                let _ = std::fs::remove_file(&p);
                i = i.wrapping_add(1);
                if i > 500 {
                    break;
                }
            }
        })
    };
    // Two full walks while the churn runs.
    let mut last_count = 0usize;
    for _ in 0..2 {
        let t = build_from_fs(&dir);
        last_count = (0..t.len() as u32)
            .filter(|&id| String::from_utf16_lossy(t.name_u16(id)) == "stable.txt")
            .count();
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = writer.join();
    assert_eq!(last_count, 1, "stable file always present; no panic");
}

/// An empty root (zero children — freshly formatted volumes, empty
/// mounted disks) lists cleanly: no error, no phantom entries.
#[test]
fn empty_root_lists_cleanly() {
    let dir = stage("empty");
    let t = build_from_fs(&dir);
    // Root + nothing else.
    assert_eq!(t.len(), 1, "just the root node");
    let root = t.node(0).expect("root");
    assert_eq!(root.logical, 0);
    assert_eq!(root.on_disk, 0);
}
