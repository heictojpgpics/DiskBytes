//! MFT staging: volume geometry, raw $MFT read, SeBackupPrivilege.

use windows::core::PCWSTR;

use super::wide;

/// `AdjustTokenPrivileges` sets this last-error when the privilege was
/// NOT assigned (it still returns TRUE — see `enable_backup_privilege`).
const ERROR_NOT_ALL_ASSIGNED: windows::Win32::Foundation::WIN32_ERROR =
    windows::Win32::Foundation::WIN32_ERROR(1300);

// ---------------------------------------------------------------------------
// Turbo engine volume seam (spec §5; doc 02 §4): FSCTL geometry + raw
// $MFT staging + SeBackupPrivilege. Pure parsing lives in core::turbo.
// ---------------------------------------------------------------------------
/// NTFS volume geometry as the turbo engine needs it (core::turbo::Geometry
/// mirror + volume serial).
#[derive(Debug, Clone, Copy)]
pub struct TurboGeometry {
    pub bytes_per_sector: u32,
    pub bytes_per_cluster: u32,
    pub bytes_per_record: u32,
    pub mft_valid_data_length: u64,
}

/// Open the volume `\\.\X:` and read the geometry via
/// `FSCTL_GET_NTFS_VOLUME_DATA` (the boot-sector parse stays as the
/// documented fallback in the references; the FSCTL is the primary).
///
/// # Errors
/// User-readable reason when the volume cannot open or is not NTFS.
pub fn turbo_geometry(drive_root: &str) -> Result<(std::fs::File, TurboGeometry), String> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    // `C:\` → `\\.\C:`
    let vol = format!(r"\\.\{}", drive_root.trim_end_matches('\\'));
    let wide_vol = wide(&vol);
    // SAFETY: NUL-terminated volume path; share r/w so a live system
    // volume opens; the handle is owned by the returned File.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide_vol.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|e| format!("Couldn't open {vol}: {e}"))?;
    // SAFETY: the HANDLE came from CreateFileW and is owned exclusively.
    let file = unsafe { std::fs::File::from_raw_handle(handle.0) };

    // NTFS_VOLUME_DATA_BUFFER (winioctl): 8 LARGE_INTEGERs then u32s.
    let mut out = [0u8; 64];
    let mut returned = 0u32;
    // SAFETY: FSCTL_GET_NTFS_VOLUME_DATA = 0x00090064; valid handle;
    // sized out-buffer; returned-bytes out-param.
    let ok = unsafe {
        DeviceIoControl(
            windows::Win32::Foundation::HANDLE(file.as_raw_handle()),
            0x0009_0064,
            None,
            0,
            Some(out.as_mut_ptr().cast()),
            out.len() as u32,
            Some(&mut returned),
            None,
        )
    };
    if ok.is_err() || returned < 52 {
        return Err("This drive does not report NTFS geometry (turbo needs NTFS).".into());
    }
    let rd_u32 = |o: usize| u32::from_le_bytes([out[o], out[o + 1], out[o + 2], out[o + 3]]);
    let read_i64 = |o: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&out[o..o + 8]);
        i64::from_le_bytes(b)
    };
    let bytes_per_sector = rd_u32(40);
    let bytes_per_cluster = rd_u32(44);
    let bytes_per_record = rd_u32(48);
    let mft_valid = read_i64(32);
    if bytes_per_sector == 0 || bytes_per_record == 0 {
        return Err("The NTFS geometry is unusable (zero record size).".into());
    }
    Ok((
        file,
        TurboGeometry {
            bytes_per_sector,
            bytes_per_cluster: bytes_per_cluster.max(1),
            bytes_per_record,
            mft_valid_data_length: mft_valid.max(0) as u64,
        },
    ))
}

/// Read the whole $MFT through record 0's data runs (the references'
/// load discipline: chunk-aligned, every slot read, "FILE"-signed
/// records fixed later by the core parser).
///
/// # Errors
/// User-readable reason on read failures or a corrupt record-0 map.
pub fn turbo_read_mft(volume: &mut std::fs::File, geo: &TurboGeometry) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};

    // Read record 0 (the $MFT's own record).
    let mut rec0 = vec![0u8; geo.bytes_per_record as usize];
    volume
        .seek(SeekFrom::Start(0))
        .map_err(|e| format!("Couldn't seek the volume: {e}"))?;
    volume
        .read_exact(&mut rec0)
        .map_err(|e| format!("Couldn't read the $MFT record: {e}"))?;
    if &rec0[0..4] != b"FILE" {
        return Err("The $MFT record is unreadable (not FILE-signed).".into());
    }

    // Geometry-driven cluster size; record 0's $DATA runs live in the
    // record after fixup — reuse the core parser through a tiny shim.
    let runs =
        diskbytes_core::turbo::record::mft_record0_runs(&mut rec0, geo.bytes_per_sector as usize)
            .map_err(|e| format!("The $MFT run list is corrupt: {e:?}"))?;

    // Stage the MFT: sequential reads per run (sparse runs skipped).
    let mut mft: Vec<u8> = Vec::new();
    for run in runs {
        let Some(lcn) = run.lcn else { continue }; // sparse
        let offset =
            u64::try_from(lcn).map_err(|_| "The $MFT cluster map is corrupt.".to_string())?;
        let byte_len = run
            .length
            .checked_mul(u64::from(geo.bytes_per_cluster))
            .ok_or_else(|| "The $MFT size overflows.".to_string())?;
        let at = offset
            .checked_mul(u64::from(geo.bytes_per_cluster))
            .ok_or_else(|| "The $MFT offset overflows.".to_string())?;
        volume
            .seek(SeekFrom::Start(at))
            .map_err(|e| format!("Couldn't seek the $MFT: {e}"))?;
        let take = byte_len.min(geo.mft_valid_data_length.saturating_sub(mft.len() as u64));
        if take == 0 {
            break;
        }
        let start = mft.len();
        mft.resize(start + take as usize, 0);
        volume
            .read_exact(&mut mft[start..])
            .map_err(|e| format!("Couldn't read the $MFT: {e}"))?;
    }
    Ok(mft)
}

/// Enable SeBackupPrivilege (spec §5 elevated requirement) — best
/// effort: privilege presence is the caller's elevation gate.
pub fn enable_backup_privilege() -> bool {
    use windows::Win32::Foundation::{CloseHandle, LUID};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let wide = wide("SeBackupPrivilege");
    let mut luid = LUID::default();
    // SAFETY: NUL-terminated privilege name; LUID out valid.
    if unsafe { LookupPrivilegeValueW(None, PCWSTR(wide.as_ptr()), &mut luid) }.is_err() {
        return false;
    }
    let mut token = windows::Win32::Foundation::HANDLE::default();
    // SAFETY: current pseudo-process handle; token out valid.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut token) }
        .is_err()
    {
        return false;
    }
    let mut tp: TOKEN_PRIVILEGES = unsafe { std::mem::zeroed() };
    tp.PrivilegeCount = 1;
    tp.Privileges[0].Luid = luid;
    tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
    // SAFETY: valid token; sized privileges struct.
    let ok = unsafe { AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None) };
    // SAFETY: GetLastError immediately after the AdjustTokenPrivileges
    // call on this thread.
    let last_err = unsafe { windows::Win32::Foundation::GetLastError() };
    // AdjustTokenPrivileges returns TRUE even when the privilege was NOT
    // assigned: the not-assigned signal is ERROR_NOT_ALL_ASSIGNED (1300)
    // in last-error. The old check compared against 1313
    // (ERROR_NO_SUCH_PRIVILEGE) — impossible here, since a bad privilege
    // NAME already failed at LookupPrivilegeValueW above — so an
    // elevated-but-filtered process (no SeBackupPrivilege hold) was
    // told the privilege was granted and turbo proceeded to fail
    // opaquely instead of taking the honest user-visible fallback.
    let granted = ok.is_ok() && last_err != ERROR_NOT_ALL_ASSIGNED;
    // SAFETY: handle balance.
    unsafe { CloseHandle(token) }.ok();
    granted
}

// ============================================================================
