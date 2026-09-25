//! macOS directory enumeration engine: getattrlistbulk on an
//! O_DIRECTORY fd, record parser, volume inventory, statfs.

use super::ffi::*;
use super::*;

pub struct MacPlatform;

const BUFFER_SIZE: usize = 256 * 1024;

// Per-thread scratch buffer (dua-cli's per-walker-buffer pattern): the
// old global-mutex buffer serialized every scanner worker on ONE buffer —
// correct but a scan-wide bottleneck. thread_local gives each worker its
// own 256 KiB.
thread_local! {
    static LIST_BUFFER: std::cell::RefCell<Vec<u8>> =
        std::cell::RefCell::new(vec![0u8; BUFFER_SIZE]);
}

impl Platform for MacPlatform {
    fn list_dir(&self, verbatim_dir: &str) -> DirListing {
        // A caller may pass a Windows-style verbatim marker; the mac
        // engine takes plain POSIX paths.
        let path = verbatim_dir.trim_start_matches("\\\\?\\");
        let c_path = match CString::new(path.replace('\\', "/")) {
            Ok(c) => c,
            Err(_) => {
                return DirListing {
                    entries: Vec::new(),
                    error: Some(ListError::Other("path contained NUL".into())),
                }
            }
        };
        // SAFETY: open() on a NUL-terminated path, O_RDONLY|O_DIRECTORY.
        let fd = unsafe { libc_open(c_path.as_ptr(), 0x0000 | 0x0010_0000) };
        if fd < 0 {
            eprintln!("[mac-engine] open failed errno={} path={path}", errno());
            return DirListing {
                entries: Vec::new(),
                error: Some(errno_list_error()),
            };
        }
        let out = LIST_BUFFER.with(|b| {
            let mut guard = b.borrow_mut();
            list_dir_fd(fd, guard.as_mut_slice())
        });
        // SAFETY: close the fd we opened on THIS thread.
        unsafe { libc_close(fd) };
        out
    }

    fn fixed_drive_roots(&self) -> Vec<String> {
        volume_inventory()
            .into_iter()
            .filter(|(_, mount, _)| is_browsable_volume(mount))
            .map(|(_, mount, _)| mount)
            .collect()
    }

    fn known_folder(&self, folder: KnownFolder) -> Option<String> {
        let home = std::env::var("HOME").ok()?;
        let p = match folder {
            KnownFolder::Profile => home,
            KnownFolder::LocalAppData => format!("{home}/Library/Application Support"),
            KnownFolder::RoamingAppData => format!("{home}/Library/Application Support"),
            KnownFolder::ProgramData => "/Library".into(),
            KnownFolder::ProgramFiles => "/Applications".into(),
            KnownFolder::ProgramFilesX86 => "/Applications".into(),
            KnownFolder::ProgramFilesWindowsApps => "/System/Applications".into(),
            KnownFolder::UserPrograms => format!("{home}/Applications"),
        };
        Some(p)
    }

    fn volume_serial(&self, verbatim_path: &str) -> Option<u64> {
        let c = CString::new(
            verbatim_path
                .trim_start_matches("\\\\?\\")
                .replace('\\', "/"),
        )
        .ok()?;
        let mut st: StatFs = unsafe { std::mem::zeroed() };
        // SAFETY: statfs into a valid StatFs-sized out-struct.
        let rc = unsafe { statfs(c.as_ptr(), &mut st) };
        if rc != 0 {
            return None;
        }
        Some((u64::from(st.f_fsid[0]) << 32) | u64::from(st.f_fsid[1]))
    }
}

fn errno() -> i32 {
    // SAFETY: errno is thread-local; reading it is safe.
    unsafe { *libc_errno() }
}

fn errno_list_error() -> ListError {
    match errno() {
        13 | 1 => ListError::AccessDenied, // EACCES / EPERM
        2 => ListError::Vanished,          // ENOENT
        e => ListError::Other(format!("macOS error {e}")),
    }
}

/// The core `getattrlistbulk` loop over one directory fd.
fn list_dir_fd(fd: c_int, buffer: &mut [u8]) -> DirListing {
    let mut entries: Vec<DirEntryData> = Vec::new();
    loop {
        // REQUEST: RETURNED_ATTRS must be set — the kernel then prefixes
        // every record with the attribute_set_t bitmap actually returned,
        // which is what `parse_bulk_record` gates each field on. ERROR
        // leads the common block; name, type and times follow, then the
        // file sizes (Mac BuildPrompt §4; layout per dua-cli).
        let attrs = AttrList {
            bitmapcount: 5,
            reserved: 0,
            commonattr: ATTR_CMN_RETURNED_ATTRS
                | ATTR_CMN_ERROR
                | ATTR_CMN_NAME
                | ATTR_CMN_OBJTYPE
                | ATTR_CMN_CRTIME
                | ATTR_CMN_MODTIME,
            volattr: 0,
            dirattr: 0,
            fileattr: ATTR_FILE_TOTALSIZE | ATTR_FILE_ALLOCSIZE,
            forkattr: 0,
        };
        // SAFETY: attrs + aligned 256 KiB buffer, both valid for the call.
        let n = unsafe {
            getattrlistbulk(
                fd,
                &attrs as *const AttrList as *mut AttrList,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                FSOPT_NOFOLLOW,
            )
        };
        if n < 0 {
            eprintln!(
                "[mac-engine] getattrlistbulk failed errno={} bitmapcount={} commonattr={:#010x} fileattr={:#010x}",
                errno(),
                attrs.bitmapcount,
                attrs.commonattr,
                attrs.fileattr
            );
            return DirListing {
                entries,
                error: Some(errno_list_error()),
            };
        }
        if n == 0 {
            return DirListing {
                entries,
                error: None,
            };
        }
        // Walk the packed records: each record is a length-prefixed blob
        // of naturally-aligned attribute values in request order.
        let mut off = 0usize;
        let mut parsed = 0usize;
        for _ in 0..n {
            if off + 20 > buffer.len() {
                break;
            }
            let rec_len = u32::from_le_bytes([
                buffer[off],
                buffer[off + 1],
                buffer[off + 2],
                buffer[off + 3],
            ]) as usize;
            if rec_len == 0 || off + rec_len > buffer.len() {
                break;
            }
            let rec = &buffer[off..off + rec_len];
            if let Some(e) = parse_bulk_record(rec) {
                // Skip "." / "..".
                if !(e.name.len() == 1 && e.name[0] == 0x2E)
                    && !(e.name.len() == 2 && e.name[0] == 0x2E && e.name[1] == 0x2E)
                {
                    entries.push(e);
                }
                parsed += 1;
            }
            off += rec_len;
        }
        if parsed == 0 {
            // Every record failed to parse — the layout contract drifted
            // again. Dump enough of the first record to diagnose without
            // flooding stderr (64 bytes).
            let dump: Vec<String> = buffer.iter().take(64).map(|b| format!("{b:02x}")).collect();
            eprintln!(
                "[mac-engine] 0/{n} records parsed (rec_len={} first 64 B: {})",
                u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]),
                dump.join(" ")
            );
        }
    }
}

/// Parse one packed `getattrlistbulk` record, per the kernel's real
/// layout (borrowed from dua-cli, MIT — see the constants comment):
///
/// ```text
/// [u32 total_length]
/// [u32 returned.commonattr][u32 volattr][u32 dirattr]
/// [u32 returned.fileattr][u32 forkattr]          <- 24-byte fixed header
/// [u32 error]            if returned & ATTR_CMN_ERROR
/// [i32 name_offset][u32 name_length]             <- attrreference_t; the
///    name BYTES live at (position of this field) + name_offset, length
///    includes the trailing NUL
/// [u32 objtype]          if returned & ATTR_CMN_OBJTYPE  (VREG/VDIR/VLNK…)
/// [i64 cr_sec, i64 cr_nsec]                      if returned & CRTIME
/// [i64 md_sec, i64 md_nsec]                      if returned & MODTIME
/// [u64 total_size]       if returned & ATTR_FILE_TOTALSIZE
/// [u64 alloc_size]       if returned & ATTR_FILE_ALLOCSIZE
/// ```
///
/// Values pack sequentially in the listed (canonical) order — CRTIME
/// BEFORE MODTIME — with no inter-attribute padding. The returned bitmap
/// (not the request) decides which fields are present; a missing field
/// falls back to its default instead of failing the whole record.
pub(crate) fn parse_bulk_record(rec: &[u8]) -> Option<DirEntryData> {
    if rec.len() < 24 {
        return None;
    }
    // Fixed header: length + the returned attribute_set_t.
    let ret_common = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
    let ret_file = u32::from_le_bytes([rec[16], rec[17], rec[18], rec[19]]);
    let mut o = 24usize;

    // error (u32) — first in the common block.
    if ret_common & ATTR_CMN_ERROR != 0 {
        if o + 4 > rec.len() {
            return None;
        }
        o += 4;
    }

    // name: attrreference_t { i32 attr_dataoffset, u32 attr_length }.
    let mut name: Vec<u16> = Vec::new();
    if ret_common & ATTR_CMN_NAME != 0 {
        if o + 8 > rec.len() {
            return None;
        }
        let reference_offset = o;
        let data_offset = i32::from_le_bytes([rec[o], rec[o + 1], rec[o + 2], rec[o + 3]]) as isize;
        let data_length =
            u32::from_le_bytes([rec[o + 4], rec[o + 5], rec[o + 6], rec[o + 7]]) as usize;
        o += 8;
        let start = reference_offset as isize + data_offset;
        if start < 0 {
            return None;
        }
        let start = start as usize;
        let end = start.checked_add(data_length)?;
        if end > rec.len() {
            return None;
        }
        let mut bytes = &rec[start..end];
        if bytes.last() == Some(&0) {
            bytes = &bytes[..bytes.len() - 1];
        }
        // ATTR_CMN_NAME is a UTF-8 string: decode UTF-8, then encode to
        // the tree's UTF-16. The old byte-widening (`u16::from(b)`)
        // mojibake'd every non-ASCII filename ("café" → "cafÃ©").
        name = String::from_utf8_lossy(bytes).encode_utf16().collect();
    }

    // objtype (u32) — a vnode type VALUE (VREG=1, VDIR=2, VLNK=5…).
    let mut objtype = VNON;
    if ret_common & ATTR_CMN_OBJTYPE != 0 {
        if o + 4 > rec.len() {
            return None;
        }
        objtype = u32::from_le_bytes([rec[o], rec[o + 1], rec[o + 2], rec[o + 3]]);
        o += 4;
    }

    // created timespec {i64 sec, i64 nsec} — BEFORE modified.
    let mut crt_sec = 0i64;
    if ret_common & ATTR_CMN_CRTIME != 0 {
        if o + 16 > rec.len() {
            return None;
        }
        crt_sec = read_i64(&rec[o..]);
        o += 16;
    }

    // modified timespec.
    let mut mod_sec = 0i64;
    if ret_common & ATTR_CMN_MODTIME != 0 {
        if o + 16 > rec.len() {
            return None;
        }
        mod_sec = read_i64(&rec[o..]);
        o += 16;
    }

    // File sizes — the kernel leaves these bits clear for directories.
    let mut logical = 0u64;
    let mut on_disk = 0u64;
    if ret_file & ATTR_FILE_TOTALSIZE != 0 {
        if o + 8 > rec.len() {
            return None;
        }
        logical = read_u64(&rec[o..]);
        o += 8;
    }
    if ret_file & ATTR_FILE_ALLOCSIZE != 0 {
        if o + 8 > rec.len() {
            return None;
        }
        on_disk = read_u64(&rec[o..]);
    }

    let is_dir = objtype == VDIR;
    let is_link = objtype == VLNK;

    Some(DirEntryData {
        name,
        is_dir,
        logical,
        on_disk,
        modified: mod_sec,
        created: crt_sec,
        // Symlinks carry the reparse-tag slot so the scanner's
        // never-descend rule applies unchanged (mac: no cloud tags).
        reparse_tag: is_link.then_some(0xA000_0009),
        cloud: false,
        file_id: 0,
    })
}

fn read_u64(b: &[u8]) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[..8]);
    u64::from_le_bytes(v)
}

fn read_i64(b: &[u8]) -> i64 {
    read_u64(b) as i64
}

/// Mounted volumes: (mount-from, mount-point, label).
pub(crate) fn volume_inventory() -> Vec<(String, String, String)> {
    // SAFETY: count query with a null buffer.
    let count = unsafe {
        getfsstat(std::ptr::null_mut(), 0, 1 /* MNT_NOWAIT */)
    };
    if count <= 0 {
        return Vec::new();
    }
    let mut bufs: Vec<StatFs> = (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
    // SAFETY: buffer sized for the reported count.
    let got = unsafe {
        getfsstat(
            bufs.as_mut_ptr(),
            std::mem::size_of::<StatFs>() as c_int * count,
            1,
        )
    };
    if got <= 0 {
        return Vec::new();
    }
    bufs.truncate(got as usize);
    bufs.iter()
        .map(|st| {
            let cstr = |buf: &[u8]| -> String {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                String::from_utf8_lossy(&buf[..end]).into_owned()
            };
            let from = cstr(&st.f_mntfromname);
            let on = cstr(&st.f_mntonname);
            let label = from.rsplit('/').next().unwrap_or(from.as_str()).to_string();
            (from, on, label)
        })
        .collect()
}

/// Browsable user volumes: the root + /Volumes mounts.
pub(crate) fn is_browsable_volume(mount: &str) -> bool {
    mount == "/" || (mount.starts_with("/Volumes/") && !mount.contains("/."))
}

/// `statfs` on one path (c_str must be NUL-terminated).
pub(crate) fn statfs_of(c_path: &CStr) -> Option<StatFs> {
    let mut st: StatFs = unsafe { std::mem::zeroed() };
    // SAFETY: valid out-struct; c_str NUL-terminated.
    let rc = unsafe { statfs(c_path.as_ptr(), &mut st) };
    (rc == 0).then_some(st)
}

// ─────────────────────────────────────────────────────────────────────
// The shell surface (mirrors win.rs's impl block + free functions).
// ─────────────────────────────────────────────────────────────────────
