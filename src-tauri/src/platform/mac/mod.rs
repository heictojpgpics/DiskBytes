//! The macOS implementation of the platform seam (doc 02 §2; the
//! cross-platform architecture's `platform-macos`, Option A: the SAME
//! Tauri app runs on macOS with this module behind the trait).
//!
//! Mirrors the exact free-function surface of `win.rs` so the command
//! layer compiles on both operating systems through `platform::os`.
//!
//! - Scanning: `getattrlistbulk(2)` per the Mac BuildPrompt §4 (256 KiB
//!   buffer, `FSOPT_NOFOLLOW`, packed attribute records).
//! - Trash: `NSWorkspace.recycleURLs:` (Finder Trash — never a hard
//!   delete; the safety promise holds on both platforms).
//! - Reveal/open/clipboard: NSWorkspace / NSPasteboard via the ObjC
//!   runtime (`objc2` msg_send) with toll-free CoreFoundation strings.
//! - Monitor: Mach/BSD (`host_statistics64`, `getifaddrs`, `proc_*`).
//! - Licensing: `IOPlatformUUID` (IOKit) hardware id + Keychain
//!   (Security.framework) local persistence — the DPAPI analogue.
//!
//! SAFETY discipline: every unsafe block below documents its contract.
//! Hand-declared FFI keeps the dependency set at ONE macos-only crate
//! (`objc2`, decision-log S5).

#![allow(clippy::missing_safety_doc)]
// FFI seams: each unsafe block carries its own proof comment
// unsafe fn + explicit inner unsafe blocks (Rust 2021 style, same as
// the win.rs COM boundary): the double-unsafe is deliberate so every
// call site reads as an unsafe operation.
#![allow(unused_unsafe)]
#![allow(clippy::undocumented_unsafe_blocks)]
// The macOS FFI seam is unsafe by design (same posture as win/mod.rs —
// the crate denies unsafe_code everywhere else).
#![allow(unsafe_code)]
// FFI-mirror realities, same posture as win/mod.rs: the struct field
// names mirror the published C layouts (f_bsize, ifa_next, ri_uuid…),
// the pointer casts bridge Rust borrows to C pointers, and the integer
// casts translate between C types (ssize_t, size_t, c_int) and Rust's.
#![allow(clippy::struct_field_names)]
#![allow(clippy::ptr_as_ptr)]
#![allow(clippy::ptr_cast_constness)]
#![allow(clippy::borrow_as_ptr)]
#![allow(clippy::ref_as_ptr)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]

// ─────────────────────────────────────────────────────────────────────
// Darwin / CoreFoundation / IOKit / Security FFI (hand-declared; the
// struct layouts are asserted in the tests module).
// ─────────────────────────────────────────────────────────────────────

// Split into cohesive submodules (worklog wave 2b): ffi is the
// shared hand-declared surface; the glob re-exports keep the
// `crate::platform::os::X` alias surface byte-identical. ffi/objc
// hold only `pub(crate)` internals (never part of the os:: surface)
// so they are NOT glob-re-exported — submodules and tests reach them
// via `super::ffi::X` / `super::objc::X` paths. (shell.rs holds only
// `impl MacPlatform` blocks — no items to re-export.)
pub mod apps;
pub mod dir;
pub mod ffi;
pub mod license;
pub mod monitor;
pub mod objc;
pub mod shell;
pub mod sysinfo;

pub use apps::*;
pub use dir::*;
pub use license::*;
pub use monitor::*;
pub use sysinfo::*;

#[cfg(test)]
mod tests {
    use diskbytes_core::platform::{KnownFolder, Platform};

    use std::os::unix::fs::PermissionsExt;

    /// Restores 0755 on drop so a failed assert never leaves the staged
    /// tree unreadable.
    struct PermRestore<'a>(&'a std::path::Path);
    impl Drop for PermRestore<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    use super::bin_policy_for;
    use super::dir::{parse_bulk_record, std_read_dir_listing};
    use super::ffi::{
        AttrList, IfAddrs, StatFs, Timeval, ATTR_CMN_CRTIME, ATTR_CMN_ERROR, ATTR_CMN_MODTIME,
        ATTR_CMN_NAME, ATTR_CMN_OBJTYPE, ATTR_CMN_RETURNED_ATTRS, ATTR_FILE_ALLOCSIZE,
        ATTR_FILE_TOTALSIZE, VDIR, VLNK, VREG,
    };
    use super::objc::BlockLiteral;
    use super::MacPlatform;

    /// The hand-declared FFI structs must match the published layouts
    /// (the spec's "declare it locally + assert size" rule).
    #[test]
    fn ffi_layout_sizes() {
        assert_eq!(std::mem::size_of::<Timeval>(), 16);
        // arm64 truth (verified on the mac runner): 8+1024×2+16 header
        // and names + f_flags_ext + f_reserved[7] = 2168.
        assert_eq!(std::mem::size_of::<StatFs>(), 2168, "statfs 64-bit layout");
        assert_eq!(std::mem::align_of::<StatFs>(), 8);
        // repr(C) pads ifa_flags→ifa_addr to 8: 8+8+4(+4)+8×4 = 56.
        assert_eq!(std::mem::size_of::<IfAddrs>(), 56);
        assert_eq!(std::mem::size_of::<AttrList>(), 24);
        assert_eq!(std::mem::size_of::<BlockLiteral>(), 32);
        assert_eq!(std::mem::align_of::<BlockLiteral>(), 8);
    }

    /// A REAL-shaped `getattrlistbulk` record (the layout the kernel
    /// actually emits, per dua-cli): 24-byte header (length + returned
    /// attribute_set_t), sequential values, the name referenced by an
    /// attrreference at the END of the record.
    fn build_record(
        name: &[u8],
        objtype: u32,
        requested_common: u32,
        requested_file: u32,
        logical: u64,
        alloc: u64,
    ) -> Vec<u8> {
        let mut rec = Vec::new();
        let mut cursor = 24usize; // header: length u32 + attribute_set_t (5×u32)
                                  // error u32 (always requested by the engine)
        if requested_common & ATTR_CMN_ERROR != 0 {
            cursor += 4;
        }
        // attrreference_t { i32 offset, u32 length } — points FORWARD to
        // the name bytes; the kernel places them after the fixed values.
        let name_ref_at = cursor;
        if requested_common & ATTR_CMN_NAME != 0 {
            cursor += 8;
        }
        let objtype_at = cursor;
        if requested_common & ATTR_CMN_OBJTYPE != 0 {
            cursor += 4;
        }
        let cr_at = cursor;
        if requested_common & ATTR_CMN_CRTIME != 0 {
            cursor += 16;
        }
        let md_at = cursor;
        if requested_common & ATTR_CMN_MODTIME != 0 {
            cursor += 16;
        }
        let total_at = cursor;
        if requested_file & ATTR_FILE_TOTALSIZE != 0 {
            cursor += 8;
        }
        let alloc_at = cursor;
        if requested_file & ATTR_FILE_ALLOCSIZE != 0 {
            cursor += 8;
        }
        let name_at = cursor; // variable data after the fixed values
                              // NUL terminator + pad to 4: the +1 must be INSIDE the ceil —
                              // a 12-byte name needs 12+1 = 13 bytes rounded up to 16, not 12
                              // (a multiple-of-4 name length used to drop the NUL byte and
                              // panic the write; CI caught it via the list-only test's
                              // "degraded.bin").
        let padded_name_len = (name.len() + 1).div_ceil(4) * 4;
        let total = name_at + padded_name_len;

        rec.resize(total, 0);
        // header
        rec[0..4].copy_from_slice(&(total as u32).to_le_bytes());
        rec[4..8].copy_from_slice(&requested_common.to_le_bytes());
        rec[8..12].copy_from_slice(&0u32.to_le_bytes()); // volattr
        rec[12..16].copy_from_slice(&0u32.to_le_bytes()); // dirattr
        rec[16..20].copy_from_slice(&requested_file.to_le_bytes());
        rec[20..24].copy_from_slice(&0u32.to_le_bytes()); // forkattr
                                                          // values
        if requested_common & ATTR_CMN_ERROR != 0 {
            rec[name_ref_at - 4..name_ref_at].copy_from_slice(&0u32.to_le_bytes());
        }
        if requested_common & ATTR_CMN_NAME != 0 {
            let data_offset = (name_at as isize) - (name_ref_at as isize);
            rec[name_ref_at..name_ref_at + 4].copy_from_slice(&(data_offset as i32).to_le_bytes());
            rec[name_ref_at + 4..name_ref_at + 8]
                .copy_from_slice(&((name.len() + 1) as u32).to_le_bytes()); // includes NUL
            rec[name_at..name_at + name.len()].copy_from_slice(name);
            rec[name_at + name.len()] = 0; // NUL terminator
        }
        if requested_common & ATTR_CMN_OBJTYPE != 0 {
            rec[objtype_at..objtype_at + 4].copy_from_slice(&objtype.to_le_bytes());
        }
        if requested_common & ATTR_CMN_CRTIME != 0 {
            rec[cr_at..cr_at + 8].copy_from_slice(&2i64.to_le_bytes()); // cr sec
            rec[cr_at + 8..cr_at + 16].copy_from_slice(&0i64.to_le_bytes());
        }
        if requested_common & ATTR_CMN_MODTIME != 0 {
            rec[md_at..md_at + 8].copy_from_slice(&1i64.to_le_bytes()); // mod sec
            rec[md_at + 8..md_at + 16].copy_from_slice(&0i64.to_le_bytes());
        }
        if requested_file & ATTR_FILE_TOTALSIZE != 0 {
            rec[total_at..total_at + 8].copy_from_slice(&logical.to_le_bytes());
        }
        if requested_file & ATTR_FILE_ALLOCSIZE != 0 {
            rec[alloc_at..alloc_at + 8].copy_from_slice(&alloc.to_le_bytes());
        }
        rec
    }

    const REQ_COMMON: u32 = ATTR_CMN_RETURNED_ATTRS
        | ATTR_CMN_ERROR
        | ATTR_CMN_NAME
        | ATTR_CMN_OBJTYPE
        | ATTR_CMN_CRTIME
        | ATTR_CMN_MODTIME;
    const REQ_FILE: u32 = ATTR_FILE_TOTALSIZE | ATTR_FILE_ALLOCSIZE;

    #[test]
    fn bulk_record_parses_a_directory() {
        let rec = build_record(b"abc", VDIR, REQ_COMMON, REQ_FILE, 0, 0);
        let e = parse_bulk_record(&rec).expect("record parses");
        assert_eq!(
            e.name,
            vec![u16::from(b'a'), u16::from(b'b'), u16::from(b'c')]
        );
        assert!(e.is_dir);
        assert!(e.reparse_tag.is_none());
        assert_eq!(e.modified, 1); // modtime AFTER crtime in the record
        assert_eq!(e.created, 2);
        // Directories: the kernel leaves the file-size bits clear.
        assert_eq!(e.logical, 0);
        assert_eq!(e.on_disk, 0);
    }

    #[test]
    fn bulk_record_parses_a_file_with_sizes() {
        let rec = build_record(b"video.mp4", VREG, REQ_COMMON, REQ_FILE, 1_100, 1_100);
        let e = parse_bulk_record(&rec).expect("record parses");
        assert_eq!(e.name, "video.mp4".encode_utf16().collect::<Vec<u16>>());
        assert!(!e.is_dir);
        assert_eq!(e.logical, 1_100);
        assert_eq!(e.on_disk, 1_100);
        assert_eq!(e.modified, 1);
        assert_eq!(e.created, 2);
    }

    #[test]
    fn bulk_record_parses_a_symlink_with_reparse_tag() {
        let rec = build_record(b"alias", VLNK, REQ_COMMON, REQ_FILE, 7, 7);
        let e = parse_bulk_record(&rec).expect("record parses");
        assert!(e.reparse_tag.is_some(), "symlinks never descend");
        assert_eq!(e.name, "alias".encode_utf16().collect::<Vec<u16>>());
    }

    #[test]
    fn bulk_record_survives_partial_attribute_sets() {
        // The returned bitmap — not the request — decides the layout: a
        // record missing CRTIME + both sizes must still parse.
        let partial_common = ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_ERROR
            | ATTR_CMN_NAME
            | ATTR_CMN_OBJTYPE
            | ATTR_CMN_MODTIME;
        let rec = build_record(b"x.txt", VREG, partial_common, 0, 0, 0);
        let e = parse_bulk_record(&rec).expect("partial record parses");
        assert_eq!(e.created, 0);
        assert_eq!(e.logical, 0);
        assert_eq!(e.on_disk, 0);
        assert_eq!(e.modified, 1);
    }

    #[test]
    fn bulk_record_list_only_mode_parses() {
        // The EACCES degradation path: names + types + per-record error,
        // NOTHING else (the request the kernel authorizes with
        // read-but-not-search permission). Sizes and times fall to 0 —
        // the entry still lists instead of hard-failing the directory.
        let list_only = ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_ERROR | ATTR_CMN_NAME | ATTR_CMN_OBJTYPE;
        let rec = build_record(b"degraded.bin", VREG, list_only, 0, 9_999, 9_999);
        let e = parse_bulk_record(&rec).expect("list-only record parses");
        assert_eq!(e.name, "degraded.bin".encode_utf16().collect::<Vec<u16>>());
        assert!(!e.is_dir);
        assert_eq!(e.logical, 0, "no sizes in list-only mode");
        assert_eq!(e.on_disk, 0, "no sizes in list-only mode");
        assert_eq!(e.modified, 0);
        assert_eq!(e.created, 0);
    }

    #[test]
    fn std_fallback_listing_matches_a_staged_tree() {
        // The not-supported-here ladder rung: std::fs::read_dir +
        // symlink_metadata per entry (SMB/FAT/exFAT volumes). The names,
        // directory flags, symlink markers and du-parity sizing must
        // match what the bulk engine would report.
        let dir = std::env::temp_dir().join(format!(
            "db-stdfb-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(dir.join("nested")).expect("stage nested");
        std::fs::write(dir.join("data.bin"), [0u8; 8192]).expect("stage data");
        std::os::unix::fs::symlink("data.bin", dir.join("link.bin")).expect("stage symlink");
        let listing = std_read_dir_listing(&dir.to_string_lossy());
        assert!(listing.error.is_none(), "{:?}", listing.error);
        let by_name = |n: &str| {
            listing
                .entries
                .iter()
                .find(|e| e.name == n.encode_utf16().collect::<Vec<u16>>())
        };
        let data = by_name("data.bin").expect("data.bin listed");
        assert!(!data.is_dir);
        assert_eq!(data.logical, 8192);
        assert!(data.on_disk >= 4096, "st_blocks×512: {}", data.on_disk);
        assert!(
            data.on_disk % 512 == 0,
            "du-parity multiple: {}",
            data.on_disk
        );
        let nested = by_name("nested").expect("nested listed");
        assert!(nested.is_dir);
        let link = by_name("link.bin").expect("link listed");
        assert!(
            link.reparse_tag.is_some(),
            "symlinks carry the never-descend marker"
        );
        assert!(
            link.logical <= "data.bin".len() as u64,
            "symlink size is the target path length, not followed"
        );
    }

    #[test]
    fn eaccess_dir_degrades_to_names_not_a_hard_error() {
        // THE EACCES LADDER, end to end: a directory with read but no
        // search permission (mode 0o444) must still enumerate NAMES —
        // either the kernel serves the full request (more lenient
        // policy) or the engine degrades to list-only attributes. The
        // user-visible contract: names present, no hard error.
        let dir = std::env::temp_dir().join(format!(
            "db-eacces-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("stage dir");
        std::fs::write(dir.join("visible.txt"), b"name enumerable").expect("stage file");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o444))
            .expect("chmod 444: read without search");
        // Restore search permission regardless of assertion outcome so
        // the staged tree stays reapable.
        let _guard = PermRestore(&dir);
        let listing = MacPlatform.list_dir(&dir.to_string_lossy());
        assert!(
            listing.error.is_none(),
            "readable-but-not-searchable must not hard-fail: {:?}",
            listing.error
        );
        let names: Vec<String> = listing
            .entries
            .iter()
            .map(|e| String::from_utf16_lossy(&e.name))
            .collect();
        assert!(
            names.contains(&"visible.txt".to_string()),
            "names must enumerate in degraded mode: {names:?}"
        );
    }

    #[test]
    fn bulk_record_rejects_truncated_header() {
        assert!(parse_bulk_record(&[0u8; 16]).is_none());
    }

    /// THE REAL SYSCALL, end to end: stage a directory with known files,
    /// run `MacPlatform::list_dir` (open + getattrlistbulk + parser) and
    /// assert the entries. The parser tests use synthetic records — this
    /// one catches kernel-contract drift (the mac UI sat at "0 B" for
    /// three CI rounds while the unit tests stayed green). Runs on the
    /// mac CI host; a no-op assertion on other platforms is unnecessary
    /// (the test is mac-gated with the module).
    #[test]
    fn real_getattrlistbulk_enumerates_a_staged_directory() {
        let dir = std::env::temp_dir().join(format!(
            "db-bulk-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).expect("stage dirs");
        std::fs::write(dir.join("alpha.txt"), b"hello world").expect("stage alpha");
        std::fs::write(dir.join("beta.bin"), [0u8; 4096]).expect("stage beta");
        std::fs::write(sub.join("gamma.log"), b"12345678").expect("stage gamma");
        // Non-ASCII names (the mojibake regression: ATTR_CMN_NAME is
        // UTF-8; the old byte-widening turned "café" into "cafÃ©" and
        // mangled every supplementary-plane name).
        std::fs::write(dir.join("café.txt"), b"accents").expect("stage café");
        std::fs::write(sub.join("日本語.md"), b"cjk").expect("stage 日本語");
        std::fs::write(sub.join("emoji-📁.txt"), b"non-bmp").expect("stage emoji");
        let listing = MacPlatform.list_dir(&dir.to_string_lossy());
        assert!(listing.error.is_none(), "engine error: {:?}", listing.error);
        let names: Vec<String> = listing
            .entries
            .iter()
            .map(|e| String::from_utf16_lossy(&e.name))
            .collect();
        assert!(names.contains(&"alpha.txt".to_string()), "names: {names:?}");
        assert!(names.contains(&"beta.bin".to_string()), "names: {names:?}");
        assert!(names.contains(&"sub".to_string()), "names: {names:?}");
        assert!(
            names.contains(&"café.txt".to_string()),
            "mojibake regression (BMP): {names:?}"
        );
        // list_dir is ONE directory level — the CJK and non-BMP names
        // live in sub/, so the engine is exercised a second time on
        // that directory (the mojibake regression classes: ATTR_CMN_NAME
        // is UTF-8; the old byte-widening mangled every non-ASCII name).
        let sub_listing = MacPlatform.list_dir(&sub.to_string_lossy());
        assert!(
            sub_listing.error.is_none(),
            "engine error: {:?}",
            sub_listing.error
        );
        let sub_names: Vec<String> = sub_listing
            .entries
            .iter()
            .map(|e| String::from_utf16_lossy(&e.name))
            .collect();
        assert!(
            sub_names.contains(&"gamma.log".to_string()),
            "sub names: {sub_names:?}"
        );
        assert!(
            sub_names.contains(&"日本語.md".to_string()),
            "mojibake regression (CJK): {sub_names:?}"
        );
        assert!(
            sub_names
                .iter()
                .any(|n| n.starts_with("emoji-") && n.rsplit('.').next() == Some("txt")),
            "mojibake regression (non-BMP surrogate pair): {sub_names:?}"
        );
        let beta = listing
            .entries
            .iter()
            .find(|e| e.name == "beta.bin".encode_utf16().collect::<Vec<u16>>())
            .expect("beta entry");
        assert!(!beta.is_dir);
        assert!(beta.logical >= 4096, "logical: {}", beta.logical);
        assert!(beta.on_disk >= 4096, "on_disk: {}", beta.on_disk);
        assert!(beta.modified > 0, "mtime: {}", beta.modified);
        let sub_e = listing
            .entries
            .iter()
            .find(|e| e.name == "sub".encode_utf16().collect::<Vec<u16>>())
            .expect("sub entry");
        assert!(sub_e.is_dir, "sub must be a directory");
        // Deliberately no direct-delete cleanup here: the R7.1 grep bans
        // those APIs anywhere in src-tauri/src (tests included) — and the
        // staged dir lives in $TMPDIR, which the OS reaps.
    }

    #[test]
    fn known_folders_resolve_under_home() {
        let home = std::env::var("HOME").unwrap_or_default();
        let platform = MacPlatform;
        assert_eq!(
            platform.known_folder(KnownFolder::Profile).as_deref(),
            Some(home.as_str())
        );
        assert!(platform.known_folder(KnownFolder::LocalAppData).is_some());
    }

    #[test]
    fn trash_policy_is_always_recyclable() {
        let p = bin_policy_for("/Users/dev/anything");
        assert!(!p.nuke_on_delete);
        assert!(p.max_capacity_mb.is_none());
    }
}
