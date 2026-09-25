//! License platform surface: app data dir, machine GUID, drive
//! serial, CPUID brand, Keychain, hardlink identity.

use super::dir::statfs_of;
use std::ffi::{c_void, CString};

use super::ffi::{
    cf_boolean_true, sysctlbyname, CFDataCreate, CFDataGetBytePtr, CFDataGetLength,
    CFDictionaryAddValue, CFDictionaryCreateMutable, CFStringCreateWithCString, IOObjectRelease,
    IORegistryEntryCreateCFProperty, IOServiceGetMatchingService, IOServiceMatching, SecItemAdd,
    SecItemCopyMatching, SecItemDelete,
};
use super::objc::{cf_release, cf_string_to_string};

// ============================================================================
// License platform surface (the win.rs M10 section mirrored)
// ============================================================================

/// Per-user app-support directory for DiskBytes
/// (`~/Library/Application Support/DiskBytes`, created on demand). `.` when
/// `HOME` is unset (test runners).
#[must_use]
pub fn app_data_dir() -> std::path::PathBuf {
    let base = std::env::var("HOME").map_or_else(
        |_| std::path::PathBuf::from("."),
        |h| {
            std::path::PathBuf::from(h)
                .join("Library")
                .join("Application Support")
        },
    );
    let dir = base.join("DiskBytes");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// MachineGuid analogue: IOPlatformUUID (IOKit).
#[must_use]
pub fn machine_guid() -> Option<String> {
    unsafe {
        let name = CString::new("IOPlatformExpertDevice").ok()?;
        // SAFETY: IOServiceMatching returns a CF dictionary consumed by
        // IOServiceGetMatchingService.
        let matching = IOServiceMatching(name.as_ptr());
        if matching.is_null() {
            return None;
        }
        // SAFETY: matching consumed by the lookup (release-on-consume).
        // First argument is kIOMainPortDefault (0) — the master-port
        // parameter is ignored on macOS 12+ and 0 is the documented
        // default (mach_host_self would leak a send right).
        let service = IOServiceGetMatchingService(0, matching);
        if service == 0 {
            return None;
        }
        // SAFETY: Create-rule CFString key over a C literal (NUL
        // included by the c"…" syntax).
        let key =
            CFStringCreateWithCString(std::ptr::null(), c"IOPlatformUUID".as_ptr(), 0x0800_0100);
        if key.is_null() {
            // SAFETY: release the service on the failure path.
            let _ = IOObjectRelease(service);
            return None;
        }
        // SAFETY: property from the live service object.
        let uuid_cf = IORegistryEntryCreateCFProperty(service, key, std::ptr::null(), 0);
        // SAFETY: release the key after the query.
        cf_release(key);
        let _ = IOObjectRelease(service);
        if uuid_cf.is_null() {
            return None;
        }
        let s = cf_string_to_string(uuid_cf);
        // SAFETY: release the property object.
        cf_release(uuid_cf);
        s
    }
}

/// System-drive serial analogue: the root volume's fsid as u32.
#[must_use]
pub fn system_drive_serial() -> Option<u32> {
    let c = CString::new("/").ok()?;
    statfs_of(&c).map(|st| st.f_fsid[0])
}

/// CPU brand via sysctl.
#[must_use]
pub fn cpuid_brand() -> Option<String> {
    let name = CString::new("machdep.cpu.brand_string").ok()?;
    let mut buf = [0u8; 128];
    let mut len = buf.len();
    // SAFETY: sysctlbyname into a fixed buffer.
    let rc = unsafe {
        sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    let end = buf[..len].iter().position(|&b| b == 0).unwrap_or(len);
    String::from_utf8(buf[..end].to_vec()).ok()
}

// ─────────────────────────────────────────────────────────────────────
// Keychain persistence (the DPAPI analogue).
// ─────────────────────────────────────────────────────────────────────

const KEYCHAIN_SERVICE: &str = "com.confines.diskbytes";
const KEYCHAIN_ACCOUNT: &str = "license-state";

fn cf_key(name: &str) -> *mut c_void {
    // SAFETY: Create-rule CFString over a PROPERLY NUL-terminated
    // CString — the old `name.as_ptr().cast()` handed a bare &str
    // pointer to a C-string API (an out-of-bounds read that only
    // worked because rodata literals happened to sit before zeros;
    // caught while lint-hardening the mac platform).
    let Ok(c) = CString::new(name) else {
        return std::ptr::null_mut();
    };
    unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), 0x0800_0100) }
}

/// Build the SecItem attributes/query dictionary.
fn keychain_dict(
    service: *const c_void,
    account: *const c_void,
    data: *const c_void,
) -> *mut c_void {
    // SAFETY: CFDictionaryCreateMutable + AddValue with Create-rule keys.
    unsafe {
        let class = cf_key("kSecClass");
        let gen_pass = cf_key("kSecClassGenericPassword");
        let service_key = cf_key("kSecAttrService");
        let account_key = cf_key("kSecAttrAccount");
        let d = CFDictionaryCreateMutable(std::ptr::null(), 0, std::ptr::null(), std::ptr::null());
        CFDictionaryAddValue(d, class, gen_pass);
        CFDictionaryAddValue(d, service_key, service);
        CFDictionaryAddValue(d, account_key, account);
        if !data.is_null() {
            let data_key = cf_key("kSecValueData");
            CFDictionaryAddValue(d, data_key, data);
            cf_release(data_key);
        }
        cf_release(class);
        cf_release(gen_pass);
        cf_release(service_key);
        cf_release(account_key);
        d
    }
}

/// Encrypt-and-store (a Keychain "generic password" item; the Keychain
/// itself provides the confidentiality DPAPI gives on Windows).
///
/// # Errors
/// When the Keychain refuses the upsert (status != 0).
pub fn dpapi_protect(data: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let service = cf_key(KEYCHAIN_SERVICE);
        let account = cf_key(KEYCHAIN_ACCOUNT);
        // SAFETY: Create-rule CFData over the slice.
        let cf_data = CFDataCreate(std::ptr::null(), data.as_ptr(), data.len() as isize);
        // Replace any existing item first (idempotent upsert).
        let del_query = keychain_dict(service, account, std::ptr::null());
        SecItemDelete(del_query);
        cf_release(del_query);
        let add_attrs = keychain_dict(service, account, cf_data);
        let status = SecItemAdd(add_attrs, std::ptr::null_mut());
        cf_release(add_attrs);
        cf_release(cf_data);
        cf_release(service);
        cf_release(account);
        if status != 0 {
            return Err(format!("Keychain write failed (status {status})"));
        }
        // The stored form is the raw payload (the Keychain is the
        // protection); return it so callers keep one byte contract.
        Ok(data.to_vec())
    }
}

/// Fetch-and-decrypt from the Keychain.
///
/// # Errors
/// When the Keychain holds no item or returns no data.
pub fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    // `data` is the caller's fallback blob; when the Keychain holds the
    // item it wins (that IS the persisted state).
    let _ = data;
    unsafe {
        let service = cf_key(KEYCHAIN_SERVICE);
        let account = cf_key(KEYCHAIN_ACCOUNT);
        let query = keychain_dict(service, account, std::ptr::null());
        let return_data_key = cf_key("kSecReturnData");
        // SAFETY: kCFBooleanTrue singleton (immutable).
        let yes = cf_boolean_true() as *mut c_void;
        CFDictionaryAddValue(query, return_data_key, yes);
        let mut result: *const c_void = std::ptr::null();
        let status = SecItemCopyMatching(query, &mut result);
        cf_release(query);
        cf_release(return_data_key);
        cf_release(service);
        cf_release(account);
        if status != 0 || result.is_null() {
            return Err("License state not found in the Keychain".into());
        }
        // SAFETY: CFData accessors on the returned item.
        let len = unsafe { CFDataGetLength(result) };
        let ptr = unsafe { CFDataGetBytePtr(result) };
        let out = if len > 0 {
            // SAFETY: ptr valid for len bytes per the CFData contract.
            unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec()
        } else {
            Vec::new()
        };
        // SAFETY: release the returned item.
        cf_release(result);
        Ok(out)
    }
}

/// POSIX stat() hardlink identity (st_dev, st_ino) — the dupes
/// exclusion on macOS comes free from the filesystem.
#[must_use] // parity with win.rs; on macOS the only caller (dupes) is cfg(windows)
#[allow(dead_code)] // API-parity stub: win.rs's consumer is Windows-gated (mac hardlink
                    // dedup is a backlog item — see docs/LEARNINGS-BACKLOG.md)
pub fn hardlink_identity(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path).ok()?;
    Some((md.dev(), md.ino()))
}
