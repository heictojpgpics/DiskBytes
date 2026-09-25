//! Volume queries (StorageSnapshot), elevation, cluster size.

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetVolumeInformationW};

use super::{drive_root_of, wide};

pub struct StorageSnapshot {
    /// Volume label (empty when unavailable).
    pub label: String16,
    /// Total bytes.
    pub total: u64,
    /// Used bytes (total − free).
    pub used: u64,
    /// Free bytes.
    pub free: u64,
}

/// A small owned UTF-16 label (avoids String::from_utf16 at the seam).
#[derive(Debug, Clone)]
pub struct String16(pub Vec<u16>);

/// Read the storage snapshot via `GetDiskFreeSpaceExW`
/// (`lpFreeBytesAvailableToCaller` — spec §6.5) + volume label.
#[must_use]
pub fn disk_storage(display_path: &str) -> Option<StorageSnapshot> {
    let root = drive_root_of(display_path)?;
    let wide_root = wide(&root);
    let mut free_caller: u64 = 0;
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    // SAFETY: NUL-terminated root; all out-pointers valid.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide_root.as_ptr()),
            Some(&mut free_caller),
            Some(&mut total),
            Some(&mut free),
        )
    };
    if ok.is_err() || total == 0 {
        return None;
    }
    // Volume label from the same root.
    let mut label = [0u16; 64];
    let wide_root2 = wide(&root);
    // SAFETY: NUL-terminated root; label buffer sized.
    let _ = unsafe {
        GetVolumeInformationW(
            PCWSTR(wide_root2.as_ptr()),
            Some(&mut label),
            None,
            None,
            None,
            None,
        )
    };
    Some(StorageSnapshot {
        label: String16(label.to_vec()),
        total,
        used: total.saturating_sub(free),
        free,
    })
}

/// True when the process token is elevated (member of the local
/// Administrators group AND the token is elevated — the standard
/// CheckTokenMembership probe; spec §6: restart-as-admin hides when
/// elevated).
#[must_use]
pub fn is_elevated() -> bool {
    use windows::Win32::Security::{
        AllocateAndInitializeSid, CheckTokenMembership, FreeSid, SECURITY_NT_AUTHORITY,
    };
    use windows::Win32::System::SystemServices::{
        DOMAIN_ALIAS_RID_ADMINS, SECURITY_BUILTIN_DOMAIN_RID,
    };
    let mut admin = windows::Win32::Security::PSID::default();
    // SAFETY: NT authority SID allocation; freed on every exit path.
    // SAFETY: NT authority SID allocation; freed on every exit path.
    let ok = unsafe {
        AllocateAndInitializeSid(
            &SECURITY_NT_AUTHORITY,
            2,
            u32::try_from(SECURITY_BUILTIN_DOMAIN_RID).unwrap_or(32),
            u32::try_from(DOMAIN_ALIAS_RID_ADMINS).unwrap_or(544),
            0,
            0,
            0,
            0,
            0,
            0,
            &mut admin,
        )
    };
    if ok.is_err() {
        return false;
    }
    let mut is_member = windows::core::BOOL::default();
    // SAFETY: valid SID; out-param valid.
    let check = unsafe { CheckTokenMembership(None, admin, &mut is_member) };
    let result = check.is_ok() && is_member.as_bool();
    // SAFETY: paired FreeSid for the allocation above.
    unsafe { FreeSid(admin) };
    result
}

/// Relaunch the app elevated with `--scan <target>` (spec §6 restart as
/// administrator; ShellExecuteW "runas"). Returns Ok when the elevated
/// launch started; the CALLER decides to exit.
///
/// # Errors
/// User-readable reason when the elevation launch fails (e.g. UAC declined).
pub fn relaunch_elevated_with(scan_target: &str, extra_args: &str) -> Result<(), String> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let exe =
        std::env::current_exe().map_err(|e| format!("Couldn't locate the app executable: {e}"))?;
    let wide_exe = wide(&exe.to_string_lossy());
    let wide_verb = wide("runas");
    let args = if extra_args.is_empty() {
        format!("--scan \"{scan_target}\"")
    } else {
        format!("--scan \"{scan_target}\" {extra_args}")
    };
    let wide_args = wide(&args);
    // SAFETY: NUL-terminated strings; null hwnd = no owner.
    let h = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(wide_verb.as_ptr()),
            PCWSTR(wide_exe.as_ptr()),
            PCWSTR(wide_args.as_ptr()),
            None,
            SW_SHOWNORMAL,
        )
    };
    if h.0 as usize > 32 {
        Ok(())
    } else {
        Err(format!(
            "Elevation was declined or failed (shell error {}).",
            h.0 as i32
        ))
    }
}

pub fn cluster_size(path: &str) -> u32 {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceW;
    let root = root_of_display(path).unwrap_or_else(|| "C:\\".to_string());
    let root_wide = wide(&root);
    let mut sectors = 0u32;
    let mut bytes_per_sector = 0u32;
    let mut free_clusters = 0u32;
    let mut total_clusters = 0u32;
    // SAFETY: NUL-terminated volume root; out pointers valid.
    let ok = unsafe {
        GetDiskFreeSpaceW(
            PCWSTR(root_wide.as_ptr()),
            Some(&mut sectors),
            Some(&mut bytes_per_sector),
            Some(&mut free_clusters),
            Some(&mut total_clusters),
        )
    };
    if ok.is_ok() && sectors > 0 && bytes_per_sector > 0 {
        sectors.saturating_mul(bytes_per_sector)
    } else {
        4096
    }
}

/// The volume root of a DISPLAY path (`C:\foo` → `C:\`).
fn root_of_display(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        let mut root = String::from_utf8_lossy(&bytes[..2]).into_owned();
        root.push('\\');
        return Some(root);
    }
    None
}
