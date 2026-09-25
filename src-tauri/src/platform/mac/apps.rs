//! Applications surface: Info.plist registry scan, MSIX stubs,
//! process close, uninstaller exec.

use std::ffi::{c_void, CString};

use objc2::msg_send;
use objc2::runtime::AnyObject;

use super::dir::statfs_of;
use super::ffi::{
    CFArrayGetCount, CFArrayGetValueAtIndex, CFDataCreate, CFDictionaryGetValueIfPresent,
    CFPropertyListCreateWithData, CFStringCreateWithCString,
};
use super::objc::Id;
use super::objc::{cf_release, cf_string_to_string, cf_url_path, workspace_shared};

/// One /Applications app entry (the registry-analogue; Info.plist
/// values). Field names mirror win.rs exactly — the command layer is
/// platform-generic.
#[derive(Debug, Clone, Default)]
pub struct RawRegistryApp {
    /// Bundle identifier — the stable id (win.rs `id`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Publisher subject (bundle id when the plist lacks one).
    pub publisher: String,
    /// `CFBundleShortVersionString`.
    pub version: String,
    /// The `.app` bundle path.
    pub install_location: String,
    /// Uninstall command ("" — mac apps uninstall via Trash).
    pub uninstall_string: String,
    /// Quiet uninstall ("" on mac).
    pub quiet_uninstall_string: String,
    /// Display icon path ("" — the fallback glyph renders).
    pub display_icon: String,
}

/// MSIX has no macOS analogue — the struct mirrors win.rs so the
/// command layer compiles; the list is always empty.
#[derive(Debug, Clone, Default)]
pub struct RawMsixApp {
    /// Package identity (always "" on macOS — no Store packages).
    pub id: String,
    /// Display name ("" on macOS).
    pub name: String,
    /// Publisher ("" on macOS).
    pub publisher: String,
    /// Version ("" on macOS).
    pub version: String,
    /// Install root ("" on macOS).
    pub install_location: String,
    /// Package family name ("" on macOS).
    pub family_name: String,
}

/// Installed apps on macOS: every `.app` bundle under /Applications and
/// ~/Applications (MSIX has no analogue → callers get an empty list).
#[must_use]
pub fn registry_uninstall_entries() -> Vec<RawRegistryApp> {
    let mut out = Vec::new();
    let roots = [
        "/Applications".to_string(),
        format!("{}/Applications", std::env::var("HOME").unwrap_or_default()),
    ];
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.rsplit('.').next() != Some("app") {
                continue;
            }
            let plist = path.join("Contents/Info.plist");
            let (display, version, bundle_id, publisher) = read_info_plist(&plist);
            let display_name = name.trim_end_matches(".app").to_string();
            let stable_id = bundle_id.clone().unwrap_or_else(|| display_name.clone());
            out.push(RawRegistryApp {
                id: stable_id,
                name: display.unwrap_or(display_name),
                publisher,
                version: version.unwrap_or_else(|| "—".into()),
                install_location: path.to_string_lossy().into_owned(),
                uninstall_string: String::new(),
                quiet_uninstall_string: String::new(),
                display_icon: String::new(),
            });
        }
    }
    out
}

/// Parse the handful of keys we need from an Info.plist
/// (CFPropertyList handles XML + binary forms).
fn read_info_plist(
    path: &std::path::Path,
) -> (Option<String>, Option<String>, Option<String>, String) {
    let Ok(data) = std::fs::read(path) else {
        return (None, None, None, String::new());
    };
    // SAFETY: CFData over our byte slice (no copy, valid for the call).
    let cf_data = unsafe { CFDataCreate(std::ptr::null(), data.as_ptr(), data.len() as isize) };
    if cf_data.is_null() {
        return (None, None, None, String::new());
    }
    // SAFETY: kCFPropertyListImmutable = 0; plist from CFData.
    let plist = unsafe {
        CFPropertyListCreateWithData(
            std::ptr::null(),
            cf_data,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    // SAFETY: release the CFData after the plist is materialized.
    unsafe { cf_release(cf_data) };
    if plist.is_null() {
        return (None, None, None, String::new());
    }
    let get_str = |key: &str| -> Option<String> {
        // SAFETY: Create-rule key over a PROPERLY NUL-terminated CString
        // (a bare &str pointer would be an out-of-bounds read for the
        // C-string API), read-only dictionary lookup, release.
        let c = CString::new(key).ok()?;
        unsafe {
            let k = CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), 0x0800_0100);
            let mut v: *const c_void = std::ptr::null();
            let present = CFDictionaryGetValueIfPresent(plist, k, &mut v);
            cf_release(k);
            if present == 0 || v.is_null() {
                return None;
            }
            cf_string_to_string(v)
        }
    };
    let display = get_str("CFBundleDisplayName").or_else(|| get_str("CFBundleName"));
    let version = get_str("CFBundleShortVersionString");
    let bundle = get_str("CFBundleIdentifier");
    let publisher = get_str("CFBundleIdentifier").unwrap_or_default();
    // SAFETY: release the parsed plist.
    unsafe { cf_release(plist) };
    (display, version, bundle, publisher)
}

/// MSIX has no macOS analogue. (The Result is the win.rs signature —
/// the applications command treats both platforms identically.)
///
/// # Errors
/// Never on macOS — the list is always empty.
#[allow(clippy::unnecessary_wraps)]
pub fn msix_packages() -> Result<Vec<RawMsixApp>, String> {
    Ok(Vec::new())
}

/// No-op on macOS.
///
/// # Errors
/// Always — Store packages are Windows-only.
pub fn msix_remove_package(_full_name: &str) -> Result<(), String> {
    Err("Store packages are Windows-only.".into())
}

/// Last-used on macOS: Spotlight metadata in a future revision; the
/// honest default is unknown.
#[must_use]
pub fn userassist_entries() -> Vec<(String, i64)> {
    Vec::new()
}

/// App icons: NSWorkspace iconForFile → PNG is a follow-up; the
/// fallback glyph renders when unavailable (honest absence, not a fake).
#[must_use]
pub fn icon_png_data_url(_icon_path: &str) -> Option<String> {
    None
}

/// Cluster size (statfs f_bsize).
#[must_use]
pub fn cluster_size(path: &str) -> u32 {
    let c = CString::new(path).unwrap_or_default();
    statfs_of(&c).map_or(0, |st| st.f_bsize)
}

/// Close running apps whose bundle lives under `dir`
/// (NSRunningApplication termination — the Mac BuildPrompt §10 flow).
pub fn close_processes_under(dir: &str) -> Vec<String> {
    let mut closed = Vec::new();
    unsafe {
        let ws = workspace_shared();
        // SAFETY: runningApplications (NSWorkspaceIncludeOthers) returns
        // an autoreleased NSArray of NSRunningApplication.
        let apps: *const c_void = unsafe { msg_send![ws, runningApplications] };
        let count = unsafe { CFArrayGetCount(apps) };
        for i in 0..count {
            // SAFETY: array index in range.
            let app = unsafe { CFArrayGetValueAtIndex(apps, i) } as *mut AnyObject;
            if app.is_null() {
                continue;
            }
            // SAFETY: bundleURL returns an autoreleased NSURL.
            let bundle_url: Id = unsafe { msg_send![app, bundleURL] };
            if !bundle_url.is_null() {
                if let Some(p) = unsafe { cf_url_path(bundle_url) } {
                    if p.starts_with(dir) {
                        // SAFETY: localizedName returns an autoreleased
                        // NSString.
                        let name: Id = unsafe { msg_send![app, localizedName] };
                        let name_s = unsafe { cf_string_to_string(name as *const c_void) }
                            .unwrap_or_else(|| p.clone());
                        // terminate() asks nicely; forceTerminate after
                        // the caller's wait window when needed.
                        let _term_ok: bool = unsafe { msg_send![app, terminate] };
                        closed.push(name_s);
                    }
                }
            }
        }
    }
    closed
}

/// Run an app's own uninstaller (rare on macOS — most apps are
/// drag-to-trash). Waits for exit like the Windows path.
///
/// # Errors
/// When the uninstaller path is empty or fails to launch.
pub fn launch_and_wait_uninstaller(cmd_line: &str) -> Result<i32, String> {
    let mut parts = cmd_line.split_whitespace();
    let exe = parts.next().unwrap_or_default();
    if exe.is_empty() {
        return Err("No uninstaller on macOS".into());
    }
    let status = std::process::Command::new(exe)
        .args(parts)
        .status()
        .map_err(|e| format!("Couldn't run {exe}: {e}"))?;
    Ok(status.code().unwrap_or(-1))
}
