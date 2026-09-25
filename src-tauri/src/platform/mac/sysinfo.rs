//! Volume snapshot, elevation, turbo stubs (turbo is Windows-only).

use std::ffi::{c_void, CString};

use objc2::msg_send;
use objc2::runtime::AnyObject;

use super::dir::statfs_of;
use super::ffi::libc_geteuid;
use super::objc::Block1;
use super::objc::Id;
use super::objc::{
    cf_array_of, cf_string_to_string, file_url, run_loop_current, run_loop_run_mode,
    workspace_shared,
};

/// Per-volume Trash policy: Finder's Trash always works on writable
/// local volumes (the BitBucket registry concept is Windows-only).
#[derive(Debug, Clone, Copy)]
pub struct BinPolicy {
    /// Always false on macOS (Trash is the only path).
    pub nuke_on_delete: bool,
    /// None — the Finder manages capacity.
    pub max_capacity_mb: Option<u64>,
}

/// The Trash policy for the volume containing `display_path`.
///
/// # Errors
/// Never — Finder's Trash is always recyclable (the signature mirrors
/// win.rs's registry-backed policy query).
#[must_use]
pub fn bin_policy_for(_display_path: &str) -> BinPolicy {
    BinPolicy {
        nuke_on_delete: false,
        max_capacity_mb: None,
    }
}

/// True when the path sits on a local volume.
#[must_use]
pub fn path_on_fixed_drive(display_path: &str) -> bool {
    let Ok(c) = CString::new(display_path) else {
        return false;
    };
    statfs_of(&c).is_some_and(|st| st.f_flags & 0x0000_0800 != 0 /* MNT_LOCAL */)
}

/// True when the path no longer exists.
#[must_use]
pub fn path_missing(display_path: &str) -> bool {
    std::fs::symlink_metadata(display_path).is_err()
}

/// No COM on macOS — a no-op guard with the same API.
#[allow(dead_code)] // API parity: the only constructor call (recycle's COM pass)
                    // is Windows-gated; macOS uses NSWorkspace recycleURLs
pub struct ComApartment;

impl ComApartment {
    /// Nothing to initialize on macOS (the Result is the win.rs signature —
    /// the recycle layer calls it identically on both platforms).
    ///
    /// # Errors
    /// Never on macOS (see the signature note above).
    #[allow(dead_code, clippy::unnecessary_wraps)] // the only caller is Windows-gated
    pub fn init() -> Result<Self, String> {
        Ok(ComApartment)
    }
}

/// Per-path trash outcome: the path and whether the move succeeded
/// (with the OS error string when it did not).
pub type TrashOutcome = (String, Result<(), String>);

/// The Trash move: `NSWorkspace.recycleURLs:completionHandler:` runs
/// asynchronously; we pump the run loop until the handler fires
/// (bounded wait, ≤ 60 s).
///
/// # Errors
/// When the run-loop pump times out (60 s) without the completion
/// handler firing.
pub fn recycle_to_trash(paths: &[String]) -> Result<Vec<TrashOutcome>, String> {
    unsafe {
        let ws = workspace_shared();
        let urls: Vec<Id> = paths.iter().map(|p| file_url(p)).collect();
        let arr = cf_array_of(&urls);
        let done = std::rc::Rc::new(std::cell::Cell::new(false));
        let errors = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let d2 = done.clone();
        let e2 = errors.clone();
        let handler = Block1::new(move |ns_error: *mut AnyObject| {
            if ns_error.is_null() {
                e2.borrow_mut().clear();
            } else {
                // SAFETY: localizedDescription returns an autoreleased
                // NSString (toll-free CFString).
                let desc: Id = unsafe { msg_send![ns_error, localizedDescription] };
                if let Some(s) = unsafe { cf_string_to_string(desc as *const c_void) } {
                    e2.borrow_mut().push(s);
                }
            }
            d2.set(true);
        });
        let raw_block = handler as *const c_void;
        // SAFETY: recycleURLs:completionHandler: with our URLs and the
        // escaping block; we pump the run loop until done below.
        let _: () = unsafe { msg_send![ws, recycleURLs: arr, completionHandler: raw_block] };
        let rl: Id = run_loop_current();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !done.get() && std::time::Instant::now() < deadline {
            // SAFETY: runMode:beforeDate: pumps the default mode.
            unsafe { run_loop_run_mode(rl, 0.2) };
        }
        let err_list = errors.borrow().clone();
        if !done.get() {
            return Err("Trash move timed out".into());
        }
        if err_list.is_empty() {
            Ok(paths.iter().map(|p| (p.clone(), Ok(()))).collect())
        } else {
            Ok(paths
                .iter()
                .map(|p| (p.clone(), Err(err_list.join("; "))))
                .collect())
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Storage / elevation / turbo / apps / monitor / licensing surfaces.
// ─────────────────────────────────────────────────────────────────────

/// Volume snapshot for the sidebar ring.
pub struct StorageSnapshot {
    /// Volume label (last mount-from component).
    pub label: String16,
    /// Total bytes.
    pub total: u64,
    /// Used bytes (total − free).
    pub used: u64,
    /// Free bytes.
    pub free: u64,
}

/// UTF-16 helper mirroring win.rs's String16.
pub struct String16(
    /// The label's UTF-16 code units (NUL-free).
    pub Vec<u16>,
);

/// Read the storage snapshot for the volume containing `display_path`.
#[must_use]
pub fn disk_storage(display_path: &str) -> Option<StorageSnapshot> {
    let c = CString::new(display_path).ok()?;
    let st = statfs_of(&c)?;
    let label = String::from_utf8_lossy(
        &st.f_mntfromname[..st.f_mntfromname.iter().position(|&b| b == 0).unwrap_or(16)],
    )
    .rsplit('/')
    .next()
    .unwrap_or("Macintosh HD")
    .to_string();
    let total = st.f_blocks.saturating_mul(u64::from(st.f_bsize));
    let free = st.f_bavail.saturating_mul(u64::from(st.f_bsize));
    Some(StorageSnapshot {
        label: String16(label.encode_utf16().collect()),
        total,
        used: total.saturating_sub(free),
        free,
    })
}

/// macOS has no elevation prompt flow (Full Disk Access is the grant).
#[must_use]
pub fn is_elevated() -> bool {
    // SAFETY: geteuid has no failure mode.
    unsafe { libc_geteuid() == 0 }
}

/// Relaunching "as administrator" is a Windows concept; the macOS
/// answer is Full Disk Access (the UI shows Open Privacy Settings).
///
/// # Errors
/// Always — with the guidance string the UI shows.
pub fn relaunch_elevated_with(_scan_target: &str, _extra_args: &str) -> Result<(), String> {
    Err("macOS uses Full Disk Access instead of elevation. Open System Settings → Privacy & Security → Full Disk Access and rescan.".into())
}

/// The Turbo engine is NTFS-only by spec (§5); on macOS the standard
/// engine always runs (the existing fallback path explains it). The
/// geometry fields mirror win.rs exactly (u32 ×3 + u64) so the
/// command layer compiles; the constructors on this platform always
/// error before they are read.
#[derive(Debug, Clone, Copy)]
pub struct TurboGeometry {
    /// Bytes per logical sector.
    pub bytes_per_sector: u32,
    /// Bytes per filesystem cluster.
    pub bytes_per_cluster: u32,
    /// Bytes per MFT record.
    pub bytes_per_record: u32,
    /// Valid data length of the $MFT stream.
    pub mft_valid_data_length: u64,
}

#[allow(non_snake_case)]
// names mirror the win.rs surface byte-for-byte: the command layer is
// platform-generic and calls os::turbo_geometry / os::turbo_read_mft
///
/// # Errors
/// Always — the fast NTFS engine is Windows-only.
pub fn turbo_geometry(_drive_root: &str) -> Result<(std::fs::File, TurboGeometry), String> {
    Err("The fast NTFS engine is Windows-only; the standard engine runs on macOS.".into())
}

#[allow(non_snake_case)] // see turbo_geometry: the win.rs name contract
///
/// # Errors
/// Always — the fast NTFS engine is Windows-only.
pub fn turbo_read_mft(
    _volume: &mut std::fs::File,
    _geo: &TurboGeometry,
) -> Result<Vec<u8>, String> {
    Err("The fast NTFS engine is Windows-only.".into())
}

/// No backup privilege on macOS (Full Disk Access is the grant).
#[must_use]
pub fn enable_backup_privilege() -> bool {
    false
}
