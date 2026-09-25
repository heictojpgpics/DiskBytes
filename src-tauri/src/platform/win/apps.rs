//! Applications surface: registry uninstall entries, MSIX packages,
//! UserAssist, icons, process close, uninstaller exec.

use windows::core::PCWSTR;

use super::wide;

// M8: Applications platform surface (spec §11; doc 02 §7)
// ============================================================================
// Raw Windows I/O feeding the pure matching in `core::apps`: registry
// Uninstall keys, MSIX packages, UserAssist last-used, icon→PNG
// extraction, cluster size, process closing and uninstaller launch.

/// One raw registry Uninstall entry (pre-matching).
#[derive(Debug, Clone, Default)]
pub struct RawRegistryApp {
    /// Full key path (hive + subkey) — the stable id.
    pub id: String,
    pub name: String,
    pub publisher: String,
    pub version: String,
    pub install_location: String,
    pub uninstall_string: String,
    pub quiet_uninstall_string: String,
    /// Parsed `DisplayIcon` path ("" when absent).
    pub display_icon: String,
}

/// One raw MSIX package entry (pre-matching).
#[derive(Debug, Clone, Default)]
pub struct RawMsixApp {
    /// Package full name — the stable id.
    pub id: String,
    pub name: String,
    pub publisher: String,
    pub version: String,
    pub install_location: String,
    pub family_name: String,
}

/// The three Uninstall roots (spec §11): 64-bit HKLM, 32-bit HKLM view,
/// and the per-user hive.
const UNINSTALL_ROOTS: [(&str, bool); 3] = [
    (
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        false,
    ),
    (
        r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
        true,
    ),
    (
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall",
        false,
    ),
];

/// Enumerate the registry Uninstall keys: skip entries without a
/// `DisplayName`, `SystemComponent = 1`, or a `ParentKeyName`
/// (spec §11). Missing keys → empty result (not an error).
pub fn registry_uninstall_entries() -> Vec<RawRegistryApp> {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
        KEY_READ,
    };
    let mut out = Vec::new();
    for (sub, is_user) in UNINSTALL_ROOTS {
        let hive = if is_user {
            HKEY_CURRENT_USER
        } else {
            HKEY_LOCAL_MACHINE
        };
        let sub_wide = wide(sub);
        let mut hk = HKEY::default();
        // SAFETY: NUL-terminated subkey path; hk is a valid out-handle
        // slot. A missing key (ERROR_FILE_NOT_FOUND) just yields no
        // entries for this root.
        let open =
            unsafe { RegOpenKeyExW(hive, PCWSTR(sub_wide.as_ptr()), None, KEY_READ, &mut hk) };
        if !open.is_ok() {
            continue;
        }
        let mut index = 0u32;
        loop {
            let mut name_buf = [0u16; 256];
            let mut name_len = name_buf.len() as u32;
            // SAFETY: name_buf/name_len describe the out buffer; the
            // remaining optional params are None.
            let err = unsafe {
                RegEnumKeyExW(
                    hk,
                    index,
                    Some(windows::core::PWSTR(name_buf.as_mut_ptr())),
                    &mut name_len,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if !err.is_ok() {
                break; // ERROR_NO_MORE_ITEMS or failure → done
            }
            let key_name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
            if let Some(app) = read_uninstall_entry(hk, &key_name, hive, sub) {
                out.push(app);
            }
            index += 1;
        }
        // SAFETY: balances the successful RegOpenKeyExW above.
        let _ = unsafe { RegCloseKey(hk) };
    }
    out
}

/// Read one Uninstall subkey into a [`RawRegistryApp`] (skipping rules
/// applied). `None` when the entry should not be listed.
fn read_uninstall_entry(
    parent: windows::Win32::System::Registry::HKEY,
    key_name: &str,
    hive: windows::Win32::System::Registry::HKEY,
    sub: &str,
) -> Option<RawRegistryApp> {
    use windows::Win32::System::Registry::{RegCloseKey, RegOpenKeyExW, KEY_READ};
    let mut hk = parent;
    let key_wide = wide(key_name);
    // SAFETY: NUL-terminated subkey name under an open parent key.
    let open = unsafe { RegOpenKeyExW(parent, PCWSTR(key_wide.as_ptr()), None, KEY_READ, &mut hk) };
    if !open.is_ok() {
        return None;
    }
    let get_str = |value: &str| -> Option<String> {
        let v = reg_query_string(hk, value);
        v.filter(|s| !s.trim().is_empty())
    };
    let name = get_str("DisplayName")?;
    // Skip SystemComponent = 1.
    if reg_query_dword(hk, "SystemComponent") == Some(1) {
        // SAFETY: balances the open above.
        let _ = unsafe { RegCloseKey(hk) };
        return None;
    }
    // Skip entries with a ParentKeyName (they belong to a parent suite).
    if reg_value_exists(hk, "ParentKeyName") {
        let _ = unsafe { RegCloseKey(hk) };
        return None;
    }
    let app = RawRegistryApp {
        id: format!("{}\\{}\\{}", hive_name(hive), sub, key_name),
        name,
        publisher: get_str("Publisher").unwrap_or_default(),
        version: get_str("DisplayVersion").unwrap_or_default(),
        install_location: get_str("InstallLocation").unwrap_or_default(),
        uninstall_string: get_str("UninstallString").unwrap_or_default(),
        quiet_uninstall_string: get_str("QuietUninstallString").unwrap_or_default(),
        display_icon: get_str("DisplayIcon")
            .as_deref()
            .and_then(diskbytes_core::apps::parse_display_icon)
            .unwrap_or_default(),
    };
    let _ = unsafe { RegCloseKey(hk) };
    Some(app)
}

/// Human hive name for ids.
fn hive_name(h: windows::Win32::System::Registry::HKEY) -> &'static str {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    if h == HKEY_LOCAL_MACHINE {
        "HKLM"
    } else if h == HKEY_CURRENT_USER {
        "HKCU"
    } else {
        "HK?"
    }
}

/// Read a REG_SZ/REG_EXPAND_SZ value (growing buffer, lossy UTF-16).
fn reg_query_string(hk: windows::Win32::System::Registry::HKEY, value: &str) -> Option<String> {
    use windows::Win32::System::Registry::{
        RegQueryValueExW, REG_EXPAND_SZ, REG_SZ, REG_VALUE_TYPE,
    };
    let vw = wide(value);
    let mut ty = REG_VALUE_TYPE::default();
    let mut len = 0u32;
    // SAFETY: two-phase query: first call takes the size.
    let err = unsafe {
        RegQueryValueExW(
            hk,
            PCWSTR(vw.as_ptr()),
            None,
            Some(&mut ty),
            None,
            Some(&mut len),
        )
    };
    if !err.is_ok() || (ty != REG_SZ && ty != REG_EXPAND_SZ) || len == 0 {
        return None;
    }
    let words = len as usize / 2 + 1;
    let mut buf = vec![0u16; words];
    let mut len2 = len;
    // SAFETY: buf covers the reported size.
    let err = unsafe {
        RegQueryValueExW(
            hk,
            PCWSTR(vw.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr().cast::<u8>()),
            Some(&mut len2),
        )
    };
    if !err.is_ok() {
        return None;
    }
    let raw = &buf[..len2 as usize / 2];
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    Some(String::from_utf16_lossy(&raw[..end]))
}

/// Read a REG_DWORD value.
fn reg_query_dword(hk: windows::Win32::System::Registry::HKEY, value: &str) -> Option<u32> {
    use windows::Win32::System::Registry::{RegQueryValueExW, REG_DWORD, REG_VALUE_TYPE};
    let vw = wide(value);
    let mut ty = REG_VALUE_TYPE::default();
    let mut data = [0u8; 4];
    let mut len = 4u32;
    // SAFETY: fixed 4-byte buffer for REG_DWORD.
    let err = unsafe {
        RegQueryValueExW(
            hk,
            PCWSTR(vw.as_ptr()),
            None,
            Some(&mut ty),
            Some(data.as_mut_ptr()),
            Some(&mut len),
        )
    };
    (err.is_ok() && ty == REG_DWORD && len == 4).then(|| u32::from_le_bytes(data))
}

/// Does a value exist (any type)?
fn reg_value_exists(hk: windows::Win32::System::Registry::HKEY, value: &str) -> bool {
    use windows::Win32::System::Registry::RegQueryValueExW;
    let vw = wide(value);
    // SAFETY: size-only probe.
    let err =
        unsafe { RegQueryValueExW(hk, PCWSTR(vw.as_ptr()), None, None, None, Some(&mut 0u32)) };
    err.is_ok()
}

/// MSIX / Store packages via WinRT `PackageManager` (spec §11): skip
/// framework, resource, and system-signed packages. Errors surface with
/// a reason (never silent).
pub fn msix_packages() -> Result<Vec<RawMsixApp>, String> {
    use windows::ApplicationModel::PackageSignatureKind;
    use windows::Management::Deployment::PackageManager;
    let _com = ComGuard::init_mta()?;
    let pm = PackageManager::new().map_err(|e| format!("PackageManager: {e}"))?;
    let packages = pm
        .FindPackagesByUserSecurityId(&windows::core::HSTRING::new())
        .map_err(|e| format!("FindPackagesByUserSecurityId: {e}"))?;
    let mut out = Vec::new();
    for p in packages {
        let Ok(id) = p.Id() else { continue };
        let full = id.FullName().unwrap_or_default().to_string_lossy();
        if full.is_empty() {
            continue;
        }
        let Ok(framework) = p.IsFramework() else {
            continue;
        };
        if framework {
            continue;
        }
        let resource = p.IsResourcePackage().unwrap_or(false);
        if resource {
            continue;
        }
        let publisher = id.Publisher().unwrap_or_default().to_string_lossy();
        // "System-signed" (decision, decision-log): the OS optional-
        // component signer `CN=Microsoft Windows, ...` — Windows' own
        // system packages. Store-signed inbox apps (Calculator etc.) use
        // other publisher subjects and stay listed.
        if publisher
            .to_ascii_lowercase()
            .starts_with("cn=microsoft windows")
        {
            continue;
        }
        // Framework/run-time signatures we never offer for uninstall.
        if matches!(p.SignatureKind(), Ok(PackageSignatureKind::Store))
            && publisher
                .to_ascii_lowercase()
                .starts_with("cn=microsoft corporation")
            && id
                .Name()
                .unwrap_or_default()
                .to_string_lossy()
                .starts_with("Microsoft.")
        {
            // Publisher-prefix Microsoft.* Store runtime helpers.
            continue;
        }
        let version = id.Version().map_or_else(
            |_| String::new(),
            |v| format!("{}.{}.{}.{}", v.Major, v.Minor, v.Build, v.Revision),
        );
        let location = p
            .InstalledLocation()
            .and_then(|f| f.Path())
            .unwrap_or_default()
            .to_string_lossy();
        out.push(RawMsixApp {
            id: full,
            name: id.Name().unwrap_or_default().to_string_lossy(),
            publisher,
            version,
            install_location: location,
            family_name: id.FamilyName().unwrap_or_default().to_string_lossy(),
        });
    }
    Ok(out)
}

/// Remove an MSIX package (blocking on the async deployment op).
///
/// # Errors
/// String error carrying the deployment error text when removal fails.
pub fn msix_remove_package(full_name: &str) -> Result<(), String> {
    use windows::Management::Deployment::PackageManager;
    let _com = ComGuard::init_mta()?;
    let pm = PackageManager::new().map_err(|e| format!("PackageManager: {e}"))?;
    let op = pm
        .RemovePackageAsync(&windows::core::HSTRING::from(full_name))
        .map_err(|e| format!("RemovePackageAsync: {e}"))?;
    // windows-future 0.3 exposes no blocking get(): poll Status.
    loop {
        let status = op.Status().map_err(|e| format!("Status: {e}"))?;
        match status {
            windows_future::AsyncStatus::Completed => break,
            windows_future::AsyncStatus::Started => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            other => {
                return Err(format!("package removal failed (status {other:?})"));
            }
        }
    }
    op.GetResults()
        .map_err(|e| format!("package removal failed: {e}"))?;
    Ok(())
}

/// Decoded UserAssist entries: (resolved executable path, unix seconds)
/// from `HKCU\…\Explorer\UserAssist\{CEBFF5CD-…}\Count` (ROT13 names,
/// FILETIME at data offset 60). Best effort: unreadable parts are
/// skipped, never errors.
pub fn userassist_entries() -> Vec<(String, i64)> {
    use diskbytes_core::apps::{rot13, userassist_last_run};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    };
    const UA_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\UserAssist\{CEBFF5CD-ACE2-4F4F-9178-9926F41749EA}\Count";
    let mut out = Vec::new();
    let sub_wide = wide(UA_KEY);
    let mut hk = HKEY::default();
    // SAFETY: NUL-terminated key path; out-handle slot valid.
    let open = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub_wide.as_ptr()),
            None,
            KEY_READ,
            &mut hk,
        )
    };
    if !open.is_ok() {
        return out;
    }
    let mut index = 0u32;
    let mut ty = windows::Win32::System::Registry::REG_VALUE_TYPE::default();
    loop {
        let mut name_buf = [0u16; 512];
        let mut name_len = name_buf.len() as u32;
        let mut data_buf = [0u8; 72];
        let mut data_len = data_buf.len() as u32;
        // SAFETY: name/data buffers with matching lengths.
        let err = unsafe {
            RegEnumValueW(
                hk,
                index,
                Some(windows::core::PWSTR(name_buf.as_mut_ptr())),
                &mut name_len,
                None,
                Some(std::ptr::addr_of_mut!(ty).cast::<u32>()),
                Some(data_buf.as_mut_ptr()),
                Some(&mut data_len),
            )
        };
        if !err.is_ok() {
            break;
        }
        let raw_name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
        let decoded = rot13(&raw_name);
        // Entries look like "{GUID}\Sub\Path\app.exe"; GUID prefixes are
        // known-folder ids resolved via SHGetKnownFolderPath.
        if let Some(resolved) = resolve_known_folder_prefix(&decoded) {
            if let Some(t) = userassist_last_run(&data_buf[..data_len as usize]) {
                out.push((resolved, t));
            }
        }
        index += 1;
    }
    let _ = unsafe { RegCloseKey(hk) };
    out
}

/// Resolve a leading `{GUID}` in a UserAssist path to its known-folder
/// path (`{6D809377-…}\App\app.exe` → `C:\Program Files\App\app.exe`).
/// Non-GUID-prefixed names return the input unchanged; unresolvable
/// GUIDs return the input too (matching will simply fail later).
fn resolve_known_folder_prefix(decoded: &str) -> Option<String> {
    use windows::core::GUID;
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};
    let rest = decoded.strip_prefix('{')?;
    let (guid_str, tail) = rest.split_once('}')?;
    let guid = GUID::try_from(guid_str).ok()?;
    // SAFETY: KNOWNFOLDERID contract; the returned PWSTR is copied and
    // freed inside the block.
    let base = unsafe {
        let p = SHGetKnownFolderPath(&guid, KNOWN_FOLDER_FLAG::default(), None).ok()?;
        let s = p.to_string().ok()?;
        windows::Win32::System::Com::CoTaskMemFree(Some(p.as_ptr().cast::<core::ffi::c_void>()));
        s
    };
    let tail = tail.strip_prefix('\\').or_else(|| tail.strip_prefix('/'))?;
    if tail.is_empty() {
        Some(base)
    } else {
        Some(format!("{base}\\{tail}"))
    }
}

/// Icon PNG as a `data:image/png;base64,…` URL (spec §11: extracted
/// with `SHGetFileInfoW`, PNG via the png crate). `None` on any
/// extraction failure (the UI shows the placeholder).
#[must_use]
pub fn icon_png_data_url(icon_path: &str) -> Option<String> {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, SelectObject, BITMAPINFO,
        BITMAPINFOHEADER, DIB_RGB_COLORS,
    };
    use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

    if icon_path.trim().is_empty() {
        return None;
    }
    let path_wide = wide(icon_path);
    let mut fi = SHFILEINFOW::default();
    // SAFETY: SHFILEINFOW out-struct valid for the call; SHGFI_ICON
    // asks the shell to own/fill hIcon.
    let got = unsafe {
        SHGetFileInfoW(
            PCWSTR(path_wide.as_ptr()),
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut fi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    if got == 0 || fi.hIcon.is_invalid() {
        return None;
    }
    // SAFETY: hIcon owned from SHGetFileInfoW; ICONINFO out-struct
    // valid; all GDI objects freed below (hbmColor/hbmMask by us, hIcon
    // by DestroyIcon, the DC by DeleteDC after deselecting).
    unsafe {
        let mut info = ICONINFO::default();
        let ok = GetIconInfo(fi.hIcon, &mut info).is_ok();
        let mut result = None;
        if ok && !info.hbmColor.is_invalid() {
            let dc = CreateCompatibleDC(None);
            let old = SelectObject(dc, info.hbmColor.into());
            let mut bmi = BITMAPINFO::default();
            let mut hdr = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: 0,
                biHeight: 0,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            };
            bmi.bmiHeader = hdr;
            // Pass 1: dimensions (lpvBits = None).
            if GetDIBits(dc, info.hbmColor, 0, 0, None, &mut bmi, DIB_RGB_COLORS) != 0 {
                let w = bmi.bmiHeader.biWidth.max(0) as usize;
                let h = (-bmi.bmiHeader.biHeight).max(0) as usize;
                if w > 0 && h > 0 && w * h <= 1024 * 1024 {
                    let mut pixels = vec![0u8; w * h * 4];
                    hdr.biWidth = bmi.bmiHeader.biWidth;
                    hdr.biHeight = -bmi.bmiHeader.biHeight; // top-down
                    bmi.bmiHeader = hdr;
                    if GetDIBits(
                        dc,
                        info.hbmColor,
                        0,
                        h as u32,
                        Some(pixels.as_mut_ptr().cast::<std::ffi::c_void>()),
                        &mut bmi,
                        DIB_RGB_COLORS,
                    ) != 0
                    {
                        result = bgra_to_png(&pixels, w, h);
                    }
                }
            }
            let _ = SelectObject(dc, old);
            let _ = DeleteDC(dc);
        }
        let _ = DeleteObject(info.hbmColor.into());
        let _ = DeleteObject(info.hbmMask.into());
        let _ = DestroyIcon(fi.hIcon);
        result
    }
}

/// Encode a BGRA bottom-up-free (top-down) buffer as PNG → data URL.
fn bgra_to_png(bgra: &[u8], w: usize, h: usize) -> Option<String> {
    // BGRA → RGBA.
    let mut rgba = vec![0u8; bgra.len()];
    for (dst, src) in rgba.chunks_exact_mut(4).zip(bgra.chunks_exact(4)) {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = src[3];
    }
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, w as u32, h as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&rgba).ok()?;
    }
    Some(format!(
        "data:image/png;base64,{}",
        diskbytes_core::apps::base64_encode(&png_bytes)
    ))
}

/// Close processes whose images live under `dir` (spec §11: EnumWindows
/// → WM_CLOSE → 5 s wait → TerminateProcess). Returns the image paths
/// that were running (for the UI's "closed N processes" note).
/// Close processes whose images live under `dir` (spec §11: EnumWindows
/// → WM_CLOSE → 5 s wait → TerminateProcess). Returns the image paths
/// that were running (for the UI's "closed N processes" note).
pub fn close_processes_under(dir: &str) -> Vec<String> {
    use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
    };

    #[derive(Default)]
    struct Ctx {
        targets: Vec<(windows::Win32::Foundation::HWND, u32)>, // (hwnd, pid)
    }

    // SAFETY: the LPARAM carries a &mut Ctx valid for the enumeration;
    // the callback only reads the window pid.
    unsafe extern "system" fn enum_cb(
        hwnd: windows::Win32::Foundation::HWND,
        lparam: windows::Win32::Foundation::LPARAM,
    ) -> windows::core::BOOL {
        let ctx = &mut *(lparam.0 as *mut Ctx);
        let mut pid = 0u32;
        // SAFETY: pid out-slot valid.
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != 0 {
            ctx.targets.push((hwnd, pid));
        }
        windows::core::BOOL(1) // keep enumerating; filtering happens after
    }

    let dir_norm = normalize_for_compare(dir);
    let mut ctx = Ctx::default();
    // SAFETY: ctx outlives the synchronous enumeration.
    let _ = unsafe {
        EnumWindows(
            Some(enum_cb),
            windows::Win32::Foundation::LPARAM(std::ptr::addr_of_mut!(ctx) as isize),
        )
    };

    // Filter to processes whose image lives under dir; WM_CLOSE them.
    let mut closed: Vec<String> = Vec::new();
    let mut handles: Vec<(windows::Win32::Foundation::HANDLE, String)> = Vec::new();
    for (hwnd, pid) in ctx.targets {
        // SAFETY: pid from GetWindowThreadProcessId; handle closed below
        // or in the wait loop.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION
                    | PROCESS_TERMINATE
                    | windows::Win32::System::Threading::PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        };
        let Ok(handle) = handle else { continue };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: buffer/len pair valid; PROCESS_QUERY_LIMITED granted.
        let ok = unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
        };
        if ok.is_err() {
            let _ = unsafe { CloseHandle(handle) };
            continue;
        }
        let image = String::from_utf16_lossy(&buf[..len as usize]);
        if !normalize_for_compare(&image).starts_with(&dir_norm) {
            let _ = unsafe { CloseHandle(handle) };
            continue;
        }
        // SAFETY: valid hwnd from the enumeration.
        let _ = unsafe {
            PostMessageW(
                Some(hwnd),
                WM_CLOSE,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            )
        };
        if !closed.contains(&image) {
            closed.push(image.clone());
        }
        handles.push((handle, image));
    }

    // Up to 5 s graceful wait, then TerminateProcess.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    for (handle, image) in &mut handles {
        while std::time::Instant::now() < deadline {
            // SAFETY: handle has PROCESS_SYNCHRONIZE; zero timeout probe.
            let r = unsafe { WaitForSingleObject(*handle, 0) };
            if r != WAIT_TIMEOUT {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let still_running = unsafe { WaitForSingleObject(*handle, 0) } == WAIT_TIMEOUT;
        if still_running {
            // SAFETY: PROCESS_TERMINATE was requested at open.
            let _ = unsafe { windows::Win32::System::Threading::TerminateProcess(*handle, 1) };
        }
        let _ = unsafe { CloseHandle(*handle) };
        let _ = image;
    }
    closed
}

/// Lowercase + separator-normalized path for prefix comparisons.
fn normalize_for_compare(p: &str) -> String {
    let mut s = p.to_ascii_lowercase().replace('/', "\\");
    while s.ends_with('\\') {
        s.pop();
    }
    s
}

/// Launch an uninstall command line and wait for exit (spec §11).
/// `UninstallString` may be a quoted exe + args; CreateProcessW parses
/// the full command line. Timeout 1 hour (MSI chains can be slow).
///
/// # Errors
/// String error when the process cannot start or does not exit within
/// the wait window.
pub fn launch_and_wait_uninstaller(cmd_line: &str) -> Result<i32, String> {
    use windows::Win32::System::Threading::{
        CreateProcessW, GetExitCodeProcess, WaitForSingleObject, PROCESS_INFORMATION, STARTUPINFOW,
    };
    if cmd_line.trim().is_empty() {
        return Err("no UninstallString".to_string());
    }
    let mut cmdline_buf: Vec<u16> = cmd_line.encode_utf16().collect();
    cmdline_buf.push(0);
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: CreateProcessW may write into the mutable command-line
    // buffer (documented); si/pi are valid out-structs; the returned
    // handles are closed below.
    // SAFETY: CreateProcessW may write into the mutable command-line
    // buffer (documented); si/pi are valid in/out-structs; the returned
    // handles are closed below.
    let launched = unsafe {
        CreateProcessW(
            windows::core::PCWSTR::null(),
            Some(windows::core::PWSTR(cmdline_buf.as_mut_ptr())),
            None,
            None,
            false,
            windows::Win32::System::Threading::PROCESS_CREATION_FLAGS::default(),
            None,
            windows::core::PCWSTR::null(),
            &si,
            &mut pi,
        )
    };
    if let Err(e) = launched {
        return Err(format!("could not launch the uninstaller: {e}"));
    }
    // SAFETY: process handle valid; 1 h wait.
    let wait = unsafe { WaitForSingleObject(pi.hProcess, 3_600_000) };
    let mut exit_code: u32 = 0;
    // SAFETY: handle valid; out slot valid.
    let _ = unsafe { GetExitCodeProcess(pi.hProcess, &mut exit_code) };
    // SAFETY: balance both handles.
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(pi.hThread) };
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(pi.hProcess) };
    if wait == windows::Win32::Foundation::WAIT_TIMEOUT {
        return Err("the uninstaller did not exit within 1 hour".to_string());
    }
    i32::try_from(exit_code).map_err(|_| "uninstaller exit code overflow".to_string())
}

/// COM initialization guard (MTA) for WinRT calls on worker threads.
/// Only constructed when `CoInitializeEx` SUCCEEDED (S_OK or S_FALSE —
/// both must be balanced by `CoUninitialize`).
struct ComGuard;

impl ComGuard {
    /// Initialize COM MTA on this thread; S_FALSE (already initialized)
    /// is fine. `Err` when COM refuses (e.g. the thread already
    /// initialized STA — never silently ignored).
    fn init_mta() -> Result<Self, String> {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        // SAFETY: no apartment-sensitive state is being carried across
        // this call; balanced by Drop exactly once per success.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() {
            return Err(format!("CoInitializeEx: {hr}"));
        }
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: balances this guard's CoInitializeEx exactly once.
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
}
