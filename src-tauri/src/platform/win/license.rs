//! License platform surface: app data dir, machine GUID, drive
//! serial, CPUID brand, DPAPI protection, hardlink identity.

use windows::core::PCWSTR;

use super::wide;

// ============================================================================
// M10: License platform surface (doc 06; licensing doc §3/§5.4)
// ============================================================================

/// Per-user roaming app-data directory for DiskBytes
/// (`%APPDATA%\DiskBytes`, created on demand). `.` when `APPDATA` is
/// unset (test runners, portable mode).
#[must_use]
pub fn app_data_dir() -> std::path::PathBuf {
    let base = std::env::var("APPDATA")
        .map_or_else(|_| std::path::PathBuf::from("."), std::path::PathBuf::from);
    let dir = base.join("DiskBytes");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid` (hardware binding
/// input; doc 06 §3.5). `None` when unreadable.
pub fn machine_guid() -> Option<String> {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ,
        REG_VALUE_TYPE,
    };
    let sub = wide(r"SOFTWARE\Microsoft\Cryptography");
    let mut hk = HKEY::default();
    // SAFETY: NUL-terminated subkey; out-handle slot valid.
    let open = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(sub.as_ptr()),
            None,
            KEY_READ,
            &mut hk,
        )
    };
    if !open.is_ok() {
        return None;
    }
    let name = wide("MachineGuid");
    let mut ty = REG_VALUE_TYPE::default();
    let mut len = 0u32;
    // SAFETY: size probe.
    let err = unsafe {
        RegQueryValueExW(
            hk,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            None,
            Some(&mut len),
        )
    };
    if err.is_err() || ty != REG_SZ || len == 0 {
        let _ = unsafe { RegCloseKey(hk) };
        return None;
    }
    let mut buf = vec![0u16; len as usize / 2 + 1];
    let mut len2 = len;
    // SAFETY: buffer covers the reported size.
    let err = unsafe {
        RegQueryValueExW(
            hk,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr().cast::<u8>()),
            Some(&mut len2),
        )
    };
    let _ = unsafe { RegCloseKey(hk) };
    if err.is_err() {
        return None;
    }
    let raw = &buf[..len2 as usize / 2];
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    Some(String::from_utf16_lossy(&raw[..end]))
}

/// The SYSTEM drive's volume serial (hardware binding input).
pub fn system_drive_serial() -> Option<u32> {
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let root = format!("{drive}\\");
    let wide_root = wide(&root);
    let mut serial: u32 = 0;
    // SAFETY: NUL-terminated volume root; out-pointer valid.
    let ok = unsafe {
        GetVolumeInformationW(
            PCWSTR(wide_root.as_ptr()),
            None,
            Some(&mut serial),
            None,
            None,
            None,
        )
    };
    ok.is_ok().then_some(serial)
}

/// CPUID brand string (hardware binding input; `None` on non-x86 or
/// CPUID-less CPUs).
#[cfg(target_arch = "x86_64")]
pub fn cpuid_brand() -> Option<String> {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::{__cpuid, CpuidResult};
        // CPUID leaf 0x80000000 (safe intrinsic on x86_64).
        let max = __cpuid(0x8000_0000).eax;
        if max < 0x8000_0004 {
            return None;
        }
        let mut brand = String::new();
        for leaf in 0x8000_0002..=0x8000_0004 {
            // Extended brand leaves validated by max above.
            let CpuidResult { eax, ebx, ecx, edx } = __cpuid(leaf);
            for word in [eax, ebx, ecx, edx] {
                let chunk = word.to_le_bytes();
                brand.push_str(&String::from_utf8_lossy(&chunk));
            }
        }
        let trimmed = brand.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }
}

/// DPAPI-encrypt bytes (licensing doc §5.4 — local cache at rest).
///
/// # Errors
/// String error when `CryptProtectData` fails.
pub fn dpapi_protect(data: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input blob points at `data` for the call; output blob is
    // filled by DPAPI (LocalAlloc) and copied+freed below.
    let result = unsafe {
        CryptProtectData(
            &input,
            windows::core::PCWSTR::null(),
            None,
            None,
            None,
            0,
            &mut output,
        )
    };
    if let Err(e) = result {
        return Err(format!("CryptProtectData: {e}"));
    }
    let bytes = unsafe {
        let slice = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
        let out = slice.to_vec();
        windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
            output.pbData.cast::<core::ffi::c_void>(),
        )));
        out
    };
    Ok(bytes)
}

/// DPAPI-decrypt bytes.
///
/// # Errors
/// String error when `CryptUnprotectData` fails (wrong user/blob).
pub fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input blob valid; output freed below (LocalFree).
    let result = unsafe { CryptUnprotectData(&input, None, None, None, None, 0, &mut output) };
    if let Err(e) = result {
        return Err(format!("CryptUnprotectData: {e}"));
    }
    let bytes = unsafe {
        let slice = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
        let out = slice.to_vec();
        windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
            output.pbData.cast::<core::ffi::c_void>(),
        )));
        out
    };
    Ok(bytes)
}

/// Hardlink identity `(volume serial, file index)` for a path (spec §10:
/// hardlinks are NOT duplicates). `None` = unavailable (treated unique).
/// Platform seam: the only Win32 file-id call site outside `scanner`
/// internals — `commands::dupes` consumes it without FFI.
pub fn hardlink_identity(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let f = std::fs::File::open(path).ok()?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: valid file handle; sized out-struct.
    if unsafe {
        GetFileInformationByHandle(
            windows::Win32::Foundation::HANDLE(f.as_raw_handle()),
            &mut info,
        )
    }
    .is_err()
    {
        return None;
    }
    Some((u64::from(info.dwVolumeSerialNumber), {
        let lo = u64::from(info.nFileIndexLow);
        let hi = u64::from(info.nFileIndexHigh);
        (hi << 32) | lo
    }))
}
