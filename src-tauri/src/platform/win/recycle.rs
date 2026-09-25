//! Recycle pre-flight policy: `BinPolicy`, fixed-drive and missing
//! checks. Verbatim move from `win.rs`.

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    GetDriveTypeW, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
};

use super::wide;

/// The windows-rs surface the recycle module is allowed to call
/// (doc 02 §2: src/recycle.rs calls windows-rs only through these).
pub mod recycle_seam {
    pub use windows::core::implement;
    pub use windows::Win32::System::Com::CLSCTX_ALL;
    pub use windows::Win32::UI::Shell::{
        FileOperation, IFileOperation, IFileOperationProgressSink, IFileOperationProgressSink_Impl,
        IShellItem, FOFX_RECYCLEONDELETE, FOF_ALLOWUNDO,
    };
    pub use windows::Win32::UI::Shell::{
        SHCreateItemFromParsingName, SIGDN_DESKTOPABSOLUTEPARSING,
    };
}

/// Per-drive Recycle Bin policy (spec §9 pre-flight rules).
#[derive(Debug, Clone, Copy)]
pub struct BinPolicy {
    /// True when Windows silently permanent-deletes (NukeOnDelete == 1).
    pub nuke_on_delete: bool,
    /// Bin capacity in MB (when configured).
    pub max_capacity_mb: Option<u64>,
}

/// Read the BitBucket policy for the volume containing `display_path`
/// (any path on it). Registry:
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{GUID}`
/// values `NukeOnDelete` (DWORD) and `MaxCapacity` (MB, per the spec).
#[must_use]
pub fn bin_policy_for(display_path: &str) -> BinPolicy {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_READ, REG_VALUE_TYPE,
    };
    let root = drive_root_of(display_path);
    let Some(root) = root else {
        return BinPolicy {
            nuke_on_delete: false,
            max_capacity_mb: None,
        };
    };
    // \\?\Volume{GUID}\ from the drive root.
    let wide_root = wide(&format!(r"\\?\{root}"));
    let mut vol_buf = [0u16; 64];
    // SAFETY: NUL-terminated root; output buffer valid and sized.
    let ok = unsafe {
        windows::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW(
            PCWSTR(wide_root.as_ptr()),
            &mut vol_buf,
        )
    };
    if ok.is_err() {
        return BinPolicy {
            nuke_on_delete: false,
            max_capacity_mb: None,
        };
    }
    let vol = String::from_utf16_lossy(&vol_buf);
    // \?\Volume{GUID}\ → {GUID} (empty when the buffer is junk).
    let guid: String = vol
        .chars()
        .filter(|c| *c == '{' || *c == '}' || c.is_ascii_hexdigit())
        .collect();
    if !guid.starts_with('{') || !guid.ends_with('}') || guid.len() < 38 {
        return BinPolicy {
            nuke_on_delete: false,
            max_capacity_mb: None,
        };
    }
    let subkey = format!(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\BitBucket\\Volume\\{guid}"
    );
    let wide_sub = wide(&subkey);
    let mut hkey = windows::Win32::System::Registry::HKEY::default();
    // SAFETY: NUL-terminated subkey; HKEY out-pointer valid; closed on
    // every path below.
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_sub.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        )
    };
    if opened.is_err() {
        return BinPolicy {
            nuke_on_delete: false,
            max_capacity_mb: None,
        };
    }
    let read_dword = |name: &str| -> Option<u32> {
        let wide_name = wide(name);
        let mut ty = REG_VALUE_TYPE::default();
        let mut data: u32 = 0;
        let mut len = 4u32;
        // SAFETY: NUL-terminated value name; typed out-params sized to
        // DWORD; the key is open (closed by the caller).
        let q = unsafe {
            RegQueryValueExW(
                hkey,
                PCWSTR(wide_name.as_ptr()),
                None,
                Some(&mut ty),
                Some(std::ptr::addr_of_mut!(data).cast::<u8>()),
                Some(&mut len),
            )
        };
        (q.is_ok()).then_some(data)
    };
    let nuke_on_delete = read_dword("NukeOnDelete").is_some_and(|v| v == 1);
    let max_capacity_mb = read_dword("MaxCapacity").map(u64::from);
    // SAFETY: handle balance — key opened above.
    let _ = unsafe { RegCloseKey(hkey) };
    BinPolicy {
        nuke_on_delete,
        max_capacity_mb,
    }
}

/// The drive root of a display path (`C:\` from `C:\foo\bar`).
pub(crate) fn drive_root_of(path: &str) -> Option<String> {
    let b = path.as_bytes();
    if b.len() >= 3 && b[1] == b':' && b[2] == b'\\' && b[0].is_ascii_alphabetic() {
        return Some(format!("{}:\\", b[0] as char));
    }
    if path.starts_with(r"\\?\Volume") {
        // Volume-guid path: root is up to the trailing backslash.
        let trimmed = path.trim_end_matches('\\');
        return Some(format!("{trimmed}\\"));
    }
    None
}

/// True when the item's drive is DRIVE_FIXED (spec §9 pre-flight).
#[must_use]
pub fn path_on_fixed_drive(display_path: &str) -> bool {
    use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;
    let Some(root) = drive_root_of(display_path) else {
        return false;
    };
    let wide_root = wide(&root);
    // SAFETY: NUL-terminated root path.
    let ty = unsafe { GetDriveTypeW(PCWSTR(wide_root.as_ptr())) };
    ty == DRIVE_FIXED
}

/// True when the path no longer exists (spec §9: missing items count as
/// already gone).
#[must_use]
pub fn path_missing(display_path: &str) -> bool {
    let wide_path = wide(display_path);
    // SAFETY: NUL-terminated path.
    let attrs = unsafe { GetFileAttributesW(PCWSTR(wide_path.as_ptr())) };
    attrs == INVALID_FILE_ATTRIBUTES
}
