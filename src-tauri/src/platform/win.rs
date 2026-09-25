//! `platform/win.rs` — the ONLY file allowed to call windows-rs directly
//! (doc 02 §2). Implements [`Platform`] for Windows:
//!
//! - `list_dir`: the spec §4 engine — `NtQueryDirectoryFile` with
//!   `FileIdFullDirectoryInformation` (class 38), one reusable 256 KiB
//!   8-byte-aligned buffer, `\\?\` paths,
//!   `FILE_LIST_DIRECTORY | SYNCHRONIZE`, share read/write/delete,
//!   `FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT`,
//!   `RestartScan = true` on the first call, loop to
//!   `STATUS_NO_MORE_FILES`, records walked by `NextEntryOffset`.
//!   Buffer/loop/FFI discipline ported from dua-cli's shipped reader
//!   (`dua-lib/src/windows.rs`, reference clone) — unaligned reads,
//!   `offset_of!` header size, checked record bounds.
//! - drive roots, known folders and volume serials for This PC /
//!   apps-roots / WinSxS identity.
//!
//! This module is FFI-dense by design (doc 02 §2 makes it the single
//! windows-rs seam). Every unsafe block below carries a `// SAFETY:`
//! comment stating its invariant; the lint that demands doc-form
//! `# Safety` sections is allowed module-wide because inline comments
//! travel with the code they guard.

#![allow(unsafe_code)]
#![allow(clippy::undocumented_unsafe_blocks)]
#![allow(clippy::cast_possible_wrap)] // FILETIME ticks: valid values are < 2^63/1e7
#![allow(clippy::cast_possible_truncation)] // buffer_len is 256 KiB by construction (BUFFER_WORDS)
#![allow(clippy::cast_sign_loss)]
// FILETIME/FileId reinterpreted as unsigned ticks/ids (NT layout)
// The NtQuerySystemInformation record walk casts a u8 base pointer to
// the 8-aligned record struct; the buffer is Vec<u64>-backed (aligned
// by construction) and the offsets are record-multiples.
#![allow(clippy::cast_ptr_alignment)]

use super::{DirEntryData, DirListing, KnownFolder, ListError, Platform};

use windows::core::PCWSTR;
use windows::Wdk::Storage::FileSystem::{
    FileIdFullDirectoryInformation, NtQueryDirectoryFile, FILE_ID_FULL_DIR_INFORMATION,
};
use windows::Win32::Foundation::{
    CloseHandle, RtlNtStatusToDosError, HANDLE, STATUS_NO_MORE_FILES,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetDriveTypeW, GetLogicalDriveStringsW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    SYNCHRONIZE,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;

/// One reusable 256 KiB buffer per engine, as 8-byte-aligned u64 words
/// (spec §4; dua-cli's `Vec<u64>` discipline).
const BUFFER_WORDS: usize = 256 * 1024 / std::mem::size_of::<u64>();

/// Windows implementation of the [`Platform`] trait.
#[derive(Debug, Default)]
pub struct WindowsPlatform;

impl WindowsPlatform {
    /// Open a directory handle per spec §4: verbatim path,
    /// `FILE_LIST_DIRECTORY | SYNCHRONIZE`, share r/w/d,
    /// `FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT`.
    ///
    /// # Panics
    /// Never — failures map to [`ListError`] variants.
    fn open_dir(verbatim: &[u16]) -> Result<HANDLE, ListError> {
        // SAFETY: `verbatim` is a NUL-terminated UTF-16 buffer owned by
        // the caller and valid for the call; all other arguments are
        // plain values.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(verbatim.as_ptr()),
                FILE_LIST_DIRECTORY.0 | SYNCHRONIZE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        }
        .map_err(|e| classify_open_error(&e))?;
        Ok(handle)
    }
}

fn wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// Map `CreateFileW` failures onto the spec's error classes.
fn classify_open_error(e: &windows::core::Error) -> ListError {
    use windows::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
    };
    let code = e.code();
    if code == ERROR_ACCESS_DENIED.to_hresult() {
        ListError::AccessDenied
    } else if code == ERROR_FILE_NOT_FOUND.to_hresult() || code == ERROR_PATH_NOT_FOUND.to_hresult()
    {
        ListError::Vanished
    } else {
        ListError::Other(e.to_string())
    }
}

/// Map NTSTATUS values from `NtQueryDirectoryFile` (via
/// `RtlNtStatusToDosError`) onto the spec's error classes.
fn classify_ntstatus(status: windows::Win32::Foundation::NTSTATUS) -> Option<ListError> {
    use windows::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
    };
    if status == STATUS_NO_MORE_FILES {
        return None; // normal end of enumeration
    }
    // SAFETY: pure status translation, no handles or pointers.
    let dos = unsafe { RtlNtStatusToDosError(status) };
    if dos == ERROR_ACCESS_DENIED.0 {
        Some(ListError::AccessDenied)
    } else if dos == ERROR_FILE_NOT_FOUND.0 || dos == ERROR_PATH_NOT_FOUND.0 {
        Some(ListError::Vanished)
    } else {
        Some(ListError::Other(format!(
            "directory enumeration failed (status {:#x}, winerror {dos})",
            status.0
        )))
    }
}

/// FILETIME (100 ns ticks since 1601-01-01, as u64) → Unix seconds
/// (spec §4: `t / 10_000_000 − 11_644_473_600`; 0 = unknown).
fn filetime_to_unix(ft: u64) -> i64 {
    if ft == 0 {
        return 0;
    }
    (ft / 10_000_000) as i64 - 11_644_473_600
}

impl Platform for WindowsPlatform {
    #[allow(clippy::too_many_lines)] // the spec §4 engine loop; record walking lives in parse_records
    fn list_dir(&self, verbatim_dir: &str) -> DirListing {
        let mut entries: Vec<DirEntryData> = Vec::with_capacity(128);
        let wide_path = wide(verbatim_dir);
        let handle = match Self::open_dir(&wide_path) {
            Ok(h) => h,
            Err(e) => {
                return DirListing {
                    entries,
                    error: Some(e),
                }
            }
        };

        // Reusable aligned buffer.
        let mut buffer: Vec<u64> = vec![0; BUFFER_WORDS];
        let buffer_len = buffer.len() * std::mem::size_of::<u64>();
        let mut restart = true;
        let mut error: Option<ListError> = None;

        loop {
            let mut iosb = IO_STATUS_BLOCK::default();
            // SAFETY: handle is owned and valid; `buffer` is 8-byte
            // aligned and writable for `buffer_len` bytes; `iosb` is a
            // valid writable IO_STATUS_BLOCK; no event/APC (synchronous
            // handle); no filename filter (enumerate all); class 38.
            let status = unsafe {
                NtQueryDirectoryFile(
                    handle,
                    None,
                    None,
                    None,
                    &mut iosb,
                    buffer.as_mut_ptr().cast::<core::ffi::c_void>(),
                    buffer_len as u32,
                    FileIdFullDirectoryInformation,
                    false,
                    None,
                    restart,
                )
            };
            restart = false;

            if status == STATUS_NO_MORE_FILES {
                break;
            }
            if status.is_err() {
                match classify_ntstatus(status) {
                    None => break,
                    Some(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }

            // Bytes written live in iosb.Information.
            let returned = iosb.Information;
            if returned == 0 || returned > buffer_len {
                error = Some(ListError::Other(format!(
                    "enumeration returned {returned} bytes for a {buffer_len}-byte buffer"
                )));
                break;
            }

            // Walk records by NextEntryOffset until 0.
            let mut offset = 0usize;
            loop {
                const HEADER_SIZE: usize =
                    std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, FileName);
                if offset > returned.saturating_sub(HEADER_SIZE) {
                    error = Some(ListError::Other(
                        "record header exceeds the returned length".into(),
                    ));
                    break;
                }
                // SAFETY: the header at `offset` lies within the validated
                // `returned` bytes of the aligned buffer; fields are read
                // unaligned because record offsets need not be.
                let info_ptr = unsafe {
                    buffer
                        .as_ptr()
                        .byte_add(offset)
                        .cast::<FILE_ID_FULL_DIR_INFORMATION>()
                };
                // SAFETY: validated above.
                let info = unsafe { info_ptr.read_unaligned() };

                if info.FileNameLength % 2 != 0 {
                    error = Some(ListError::Other(
                        "record has an odd UTF-16 byte length".into(),
                    ));
                    break;
                }
                let name_len = (info.FileNameLength / 2) as usize;
                let name_start = offset
                    .checked_add(HEADER_SIZE)
                    .and_then(|s| s.checked_add(name_len * std::mem::size_of::<u16>()));
                // is_none_or is MSRV 1.82; the app targets 1.80 (spec).
                if name_start.map_or(true, |end| end > returned) {
                    error = Some(ListError::Other(
                        "record name exceeds the returned length".into(),
                    ));
                    break;
                }

                // SAFETY: the name range was validated against `returned`;
                // copy code units one by one (may be unaligned).
                let name_ptr = unsafe { (&raw const (*info_ptr).FileName).cast::<u16>() };
                let mut name: Vec<u16> = Vec::with_capacity(name_len);
                for i in 0..name_len {
                    // SAFETY: index within the validated name range.
                    name.push(unsafe { name_ptr.add(i).read_unaligned() });
                }

                // Skip `.` and `..` (spec §4).
                let is_dot = name.len() == 1 && name[0] == u16::from(b'.');
                let is_dotdot =
                    name.len() == 2 && name[0] == u16::from(b'.') && name[1] == u16::from(b'.');
                if !is_dot && !is_dotdot {
                    let attrs = info.FileAttributes;
                    let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
                    let cloud = attrs
                        & (FILE_ATTRIBUTE_OFFLINE.0
                            | FILE_ATTRIBUTE_RECALL_ON_OPEN.0
                            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0)
                        != 0;
                    let reparse_tag =
                        (attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0).then_some(info.EaSize);
                    entries.push(DirEntryData {
                        name,
                        is_dir,
                        logical: info.EndOfFile.unsigned_abs(),
                        on_disk: info.AllocationSize.unsigned_abs(),
                        // FILETIME values are plain i64 100-ns ticks
                        // (the record layout, not the Win32 struct).
                        modified: filetime_to_unix(info.LastWriteTime as u64),
                        created: filetime_to_unix(info.CreationTime as u64),
                        reparse_tag,
                        cloud,
                        file_id: info.FileId as u64,
                    });
                }

                if info.NextEntryOffset == 0 {
                    break;
                }
                let Some(next) = offset.checked_add(info.NextEntryOffset as usize) else {
                    error = Some(ListError::Other("NextEntryOffset overflow".into()));
                    break;
                };
                if next >= returned {
                    // The final record's offset may point past the end —
                    // enumeration continues via another Nt call only when
                    // more data exists; treat as end of this buffer.
                    break;
                }
                offset = next;
            }

            if error.is_some() {
                break;
            }
        }

        // SAFETY: owned handle, closed exactly once.
        unsafe { CloseHandle(handle) }.ok();
        DirListing { entries, error }
    }

    fn fixed_drive_roots(&self) -> Vec<String> {
        use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;
        // 26 drives max × 4 UTF-16 units incl. NUL.
        let mut buf: Vec<u16> = vec![0; 26 * 4 + 1];
        // SAFETY: `buf` is a writable buffer of the documented max size.
        let len = unsafe { GetLogicalDriveStringsW(Some(&mut buf)) } as usize;
        if len == 0 || len as u32 > buf.len() as u32 + 1 {
            return Vec::new();
        }
        buf.truncate(len);
        // NUL-separated, double-NUL-terminated list.
        let mut roots = Vec::new();
        for drive in buf.split(|&c| c == 0).filter(|s| !s.is_empty()) {
            let root = String::from_utf16_lossy(drive);
            // SAFETY: NUL-terminated UTF-16 for the call's duration.
            let ty = unsafe { GetDriveTypeW(PCWSTR(drive.as_ptr())) };
            if ty == DRIVE_FIXED {
                roots.push(root);
            }
        }
        roots
    }

    fn known_folder(&self, folder: KnownFolder) -> Option<String> {
        use windows::core::GUID;
        use windows::Win32::UI::Shell::KNOWN_FOLDER_FLAG;
        use windows::Win32::UI::Shell::{
            FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_ProgramData, FOLDERID_ProgramFiles,
            FOLDERID_ProgramFilesX86, FOLDERID_RoamingAppData, FOLDERID_UserProgramFiles,
            SHGetKnownFolderPath,
        };
        let (guid, composed): (GUID, bool) = match folder {
            KnownFolder::LocalAppData => (FOLDERID_LocalAppData, false),
            KnownFolder::RoamingAppData => (FOLDERID_RoamingAppData, false),
            KnownFolder::ProgramData => (FOLDERID_ProgramData, false),
            KnownFolder::ProgramFiles => (FOLDERID_ProgramFiles, false),
            KnownFolder::ProgramFilesX86 => (FOLDERID_ProgramFilesX86, false),
            KnownFolder::ProgramFilesWindowsApps => (FOLDERID_ProgramFiles, true),
            KnownFolder::UserPrograms => (FOLDERID_UserProgramFiles, false),
            KnownFolder::Profile => (FOLDERID_Profile, false),
        };
        // SAFETY: KNOWNFOLDERID value; the returned PWSTR is read then
        // freed inside the same block (the documented ownership
        // contract).
        let mut s = unsafe {
            let path = SHGetKnownFolderPath(&guid, KNOWN_FOLDER_FLAG::default(), None).ok()?;
            let s = path.to_string().ok()?;
            windows::Win32::System::Com::CoTaskMemFree(Some(
                path.as_ptr().cast::<core::ffi::c_void>(),
            ));
            s
        };
        if composed {
            if !s.ends_with('\\') {
                s.push('\\');
            }
            s.push_str("WindowsApps");
        }
        Some(s)
    }

    fn volume_serial(&self, verbatim_path: &str) -> Option<u64> {
        use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
        let root = root_of(verbatim_path)?;
        let wide_root = wide(&root);
        let mut serial: u32 = 0;
        // SAFETY: NUL-terminated root path; all out-pointers are valid.
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
        (ok.is_ok()).then(|| u64::from(serial))
    }
}

/// The volume root (`\\?\C:\`) of a verbatim path.
fn root_of(verbatim: &str) -> Option<String> {
    let s = verbatim.strip_prefix(r"\\?\")?;
    if s.len() < 2
        || !s.as_bytes()[1].is_ascii_alphabetic()
        || s.as_bytes()[2..].first() != Some(&b':')
    {
        return None;
    }
    Some(format!(r"\\?\{}:", &s[..1]))
}

/// Shell integration (spec §7/§8; the ONLY windows-rs callers — doc 02
/// §2): open with the default handler, reveal in Explorer, clipboard
/// copy, and process-side helpers for later milestones.
impl WindowsPlatform {
    /// Open `path` with its default handler (folders: Explorer; files:
    /// the registered app — the preview overlay's "Open with default
    /// app" button, spec §8).
    ///
    /// # Errors
    /// User-readable reason when the shell launch fails (checked via the
    /// classic `HINSTANCE > 32` contract).
    pub fn open_path(path: &str) -> Result<(), String> {
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide_path = wide(path);
        let verb = wide("open");
        // SAFETY: NUL-terminated strings; null hwnd/params are the
        // documented "no owner window" form.
        let h = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(wide_path.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        // HINSTANCE > 32 = success (<= 32 is a SE_ERR code).
        if h.0 as usize > 32 {
            Ok(())
        } else {
            Err(format!(
                "Windows could not open this item (shell error {}).",
                h.0 as i32
            ))
        }
    }

    /// Reveal `path` in Explorer with the item selected (spec §6 "Show in
    /// Explorer"). Uses `SHOpenFolderAndSelectItems` — NOT a spawned
    /// process. The item's parent folder opens.
    ///
    /// # Errors
    /// User-readable reason when the PIDL cannot be created or the call
    /// fails.
    pub fn reveal_in_explorer(path: &str) -> Result<(), String> {
        use windows::Win32::UI::Shell::{ILCreateFromPathW, ILFree, SHOpenFolderAndSelectItems};
        let wide_path = wide(path);
        // SAFETY: NUL-terminated path; the PIDL is owned here and freed
        // on every exit path (the documented ownership contract).
        unsafe {
            let pidl = ILCreateFromPathW(PCWSTR(wide_path.as_ptr()));
            if pidl.is_null() {
                return Err("Windows could not locate this item.".into());
            }
            let result = SHOpenFolderAndSelectItems(pidl, None, 0);
            ILFree(Some(pidl.cast_const()));
            result.map_err(|e| format!("Explorer could not show this item: {e}"))
        }
    }

    /// Copy `text` to the clipboard as CF_UNICODETEXT (spec §6/§8 "Copy
    /// Path"). Takes the clipboard once, retries are the caller's
    /// concern (a single open attempt is the honest behavior).
    ///
    /// # Errors
    /// User-readable reason when the clipboard cannot be opened or set.
    pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
        use windows::Win32::Foundation::{GlobalFree, HANDLE};
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
        };
        use windows::Win32::System::Memory::{
            GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
        };
        use windows::Win32::System::Ole::CF_UNICODETEXT;
        let wide_text = wide(text);
        let byte_len = wide_text.len() * std::mem::size_of::<u16>();
        // SAFETY: clipboard + global-memory ownership follows the
        // documented contracts: OpenClipboard → Empty → SetClipboardData
        // transfers the HGLOBAL to the system; CloseClipboard always
        // runs; GlobalLock/Unlock pair around the write.
        unsafe {
            if OpenClipboard(None).is_err() {
                return Err("The clipboard is busy right now.".into());
            }
            let result = (|| {
                EmptyClipboard().map_err(|e| format!("Clipboard reset failed: {e}"))?;
                let handle = GlobalAlloc(GMEM_MOVEABLE, byte_len)
                    .map_err(|e| format!("Clipboard memory failed: {e}"))?;
                let dst = GlobalLock(handle);
                if dst.is_null() {
                    let _ = GlobalFree(Some(handle));
                    return Err("Clipboard memory lock failed.".into());
                }
                std::ptr::copy_nonoverlapping(
                    wide_text.as_ptr(),
                    dst.cast::<u16>(),
                    wide_text.len(),
                );
                let unlock_ok = GlobalUnlock(handle).is_ok();
                if !unlock_ok {
                    // A failure here means our copy went wrong; the handle
                    // is still ours to free (SetClipboardData not yet called).
                    let _ = GlobalFree(Some(handle));
                    return Err("Clipboard write failed.".into());
                }
                // Ownership transfers to the system on success; on failure
                // the handle is still ours to free.
                if SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(handle.0))).is_err() {
                    // The system refused the data; the HGLOBAL is still ours.
                    let _ = GlobalFree(Some(handle));
                    return Err("Clipboard set failed.".into());
                }
                Ok(())
            })();
            let _ = CloseClipboard();
            result
        }
    }
}

// ---------------------------------------------------------------------------
// Recycle Bin seam (spec §9; doc 02 §2 footnote): the COM/registry
// primitives live HERE so recycle.rs only consumes re-exported helpers
// (a future platform-macos crate mirrors this single seam).
// ---------------------------------------------------------------------------
/// Re-exports for the recycle module (doc 02 §2: recycle.rs calls
/// windows-rs only through these).
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
fn drive_root_of(path: &str) -> Option<String> {
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
    use windows::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
    let wide_path = wide(display_path);
    // SAFETY: NUL-terminated path.
    let attrs = unsafe { GetFileAttributesW(PCWSTR(wide_path.as_ptr())) };
    attrs == INVALID_FILE_ATTRIBUTES
}

/// COM apartment initialization guard (recycle thread). `CoUninitialize`
/// runs on drop EXACTLY once per successful `CoInitializeEx` (including
/// the S_FALSE "already initialized" case — balancing is required).
pub struct ComApartment {
    /// The HRESULT returned by `CoInitializeEx` (S_FALSE = already init).
    hr: windows::core::HRESULT,
}

impl ComApartment {
    /// Initialize COM on the current thread (apartment-threaded,
    /// OLE1DDE disabled — the shell's documented requirement).
    ///
    /// # Errors
    /// User-readable message when COM cannot initialize.
    pub fn init() -> Result<Self, String> {
        use windows::Win32::System::Com::{
            CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
        };
        // SAFETY: no reserved params; the thread has no prior unbalanced
        // init (the guard owns the pairing).
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        if hr.is_ok() || hr == windows::core::HRESULT(1)
        /* S_FALSE */
        {
            Ok(Self { hr })
        } else {
            Err(format!("Windows COM unavailable (0x{:08X})", hr.0 as u32))
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: balances the CoInitializeEx from init() on THIS thread.
        unsafe { windows::Win32::System::Com::CoUninitialize() };
        let _ = self.hr;
    }
}

/// Disk storage snapshot (spec §6.5) for the volume containing `path`.
#[derive(Debug, Clone)]
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
    use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetVolumeInformationW};
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
    const ERROR_NOT_ALL_ASSIGNED: windows::Win32::Foundation::WIN32_ERROR =
        windows::Win32::Foundation::WIN32_ERROR(1300);
    let granted = ok.is_ok() && last_err != ERROR_NOT_ALL_ASSIGNED;
    // SAFETY: handle balance.
    unsafe { CloseHandle(token) }.ok();
    granted
}

// ============================================================================
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

/// Cluster size (bytes) for the volume containing `path` —
/// `GetDiskFreeSpaceW`; 4096 fallback so allocated math stays sane.
#[must_use]
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

// ============================================================================
// M9: Monitor raw sampling (spec §12; doc 02 §7)
// ============================================================================
// One stateless snapshot of the whole machine. The sampler thread in
// `commands/monitor.rs` holds two consecutive `RawMonitor`s and lets
// `core::monitor` compute the deltas (CPU %, per-process CPU, rates).

/// One raw machine snapshot (no deltas — deltas are computed by the
/// caller against the previous snapshot).
#[derive(Debug, Clone, Default)]
pub struct RawMonitor {
    /// `GetSystemTimes` ticks (kernel INCLUDES idle).
    pub ticks: diskbytes_core::monitor::CpuTicks,
    pub threads: u32,
    pub processes: u32,
    pub mem_total: u64,
    pub mem_available: u64,
    pub kernel_paged: u64,
    pub kernel_nonpaged: u64,
    pub system_cache: u64,
    pub commit_total: u64,
    pub commit_limit: u64,
    /// Memory Compression private usage (`None` → "—" in the UI).
    pub compressed_ws: Option<u64>,
    /// Filtered octet sums (up, non-loopback, Ethernet/802.11,
    /// HardwareInterface).
    pub net_in: u64,
    pub net_out: u64,
    /// Fixed + removable volumes.
    pub volumes: Vec<diskbytes_core::monitor::VolumeSample>,
    /// (pid, name, kernel_100ns, user_100ns, working set) — PID 0
    /// (System Idle) excluded.
    pub procs: Vec<(u32, String, u64, u64, u64)>,
}

/// Take one raw machine sample. Best effort: individual API failures
/// leave their fields at zero (the spec's values must be SANE, never
/// wrong-looking fabrications — zeroed fields read as "—").
pub fn monitor_raw() -> RawMonitor {
    // Gather every subsystem (best effort; failures read as zero/"—").
    let ticks = system_times();
    let perf = performance_information();
    let mem = global_memory();
    let net = network_octets();
    let procs = process_snapshot();
    let compressed_ws = memory_compression_ws(&procs);
    let volumes = volume_samples();
    let (
        threads,
        processes,
        kernel_paged,
        kernel_nonpaged,
        system_cache,
        commit_total,
        commit_limit,
    ) = perf.map_or((0, 0, 0, 0, 0, 0, 0), |pi| {
        (
            pi.ThreadCount,
            pi.ProcessCount,
            pi.KernelPaged as u64,
            pi.KernelNonpaged as u64,
            pi.SystemCache as u64,
            pi.CommitTotal as u64,
            pi.CommitLimit as u64,
        )
    });
    let (mem_total, mem_available) = mem.unwrap_or((0, 0));
    let (net_in, net_out) = net.unwrap_or((0, 0));
    RawMonitor {
        ticks,
        threads,
        processes,
        mem_total,
        mem_available,
        kernel_paged,
        kernel_nonpaged,
        system_cache,
        commit_total,
        commit_limit,
        compressed_ws,
        net_in,
        net_out,
        volumes,
        procs,
    }
}

/// `GetSystemTimes` as tick counters.
fn system_times() -> diskbytes_core::monitor::CpuTicks {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetSystemTimes;
    let (mut idle, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: three FILETIME out-structs, valid for the call.
    let ok = unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_ok() };
    if !ok {
        return diskbytes_core::monitor::CpuTicks::default();
    }
    let q = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    diskbytes_core::monitor::CpuTicks {
        idle: q(idle),
        kernel: q(kernel),
        user: q(user),
    }
}

/// `GetPerformanceInfo` snapshot.
fn performance_information(
) -> Option<windows::Win32::System::ProcessStatus::PERFORMANCE_INFORMATION> {
    use windows::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
    let mut pi = PERFORMANCE_INFORMATION {
        cb: std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32,
        ..Default::default()
    };
    // SAFETY: struct sized to its own cb for the versioned contract.
    let ok = unsafe {
        GetPerformanceInfo(
            &mut pi,
            std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32,
        )
        .is_ok()
    };
    ok.then_some(pi)
}

/// `GlobalMemoryStatusEx` → (total, available).
fn global_memory() -> Option<(u64, u64)> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut ms = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: dwLength set per the documented contract; out-struct valid.
    let ok = unsafe { GlobalMemoryStatusEx(&mut ms) }.is_ok();
    ok.then_some((ms.ullTotalPhys, ms.ullAvailPhys))
}

/// Filtered octet sums (spec §12: interfaces that are UP, not loopback,
/// type Ethernet (6) or IEEE 802.11 (71), with the HardwareInterface
/// flag — excludes VPN/WSL/Hyper-V virtual switches).
fn network_octets() -> Option<(u64, u64)> {
    use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
    const IF_TYPE_ETHERNET: u32 = 6; // RFC 1213 ifType
    const IF_TYPE_IEEE80211: u32 = 71;
    const IF_TYPE_LOOPBACK: u32 = 24;
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    // SAFETY: GetIfTable2 allocates the table; freed via FreeMibTable
    // below (the documented ownership contract).
    let err = unsafe { GetIfTable2(&mut table) };
    if !err.is_ok() || table.is_null() {
        return None;
    }
    let (mut inn, mut out) = (0u64, 0u64);
    // SAFETY: NumEntries bounds the Table slice; rows are valid for the
    // table's lifetime.
    unsafe {
        let n = (*table).NumEntries as usize;
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), n);
        for row in rows {
            let up = row.OperStatus == windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
            let hw = row.InterfaceAndOperStatusFlags._bitfield & 0x01 != 0; // netioapi.h bit 0
            let ty_ok = (row.Type == IF_TYPE_ETHERNET || row.Type == IF_TYPE_IEEE80211)
                && row.Type != IF_TYPE_LOOPBACK;
            if up && hw && ty_ok {
                inn = inn.saturating_add(row.InOctets);
                out = out.saturating_add(row.OutOctets);
            }
        }
        FreeMibTable(table.cast::<core::ffi::c_void>());
    }
    Some((inn, out))
}

/// All processes via ONE `NtQuerySystemInformation(SystemProcessInformation)`
/// call with a growing buffer (reads protected processes too — no
/// handles opened). PID 0 (System Idle) excluded per spec §12.
fn process_snapshot() -> Vec<(u32, String, u64, u64, u64)> {
    use windows::Wdk::System::SystemInformation::{
        NtQuerySystemInformation, SystemProcessInformation,
    };
    use windows::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION;
    // 256 KiB as 8-byte-aligned words (records are 8-aligned).
    let mut words: Vec<u64> = vec![0; 32 * 1024];
    loop {
        let mut needed = 0u32;
        // SAFETY: words/len pair; SystemProcessInformation is the class
        // 5 layout the struct mirrors; the walk below checks every
        // offset. The u64 backing guarantees record alignment.
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessInformation,
                words.as_mut_ptr().cast::<core::ffi::c_void>(),
                (words.len() * 8) as u32,
                &mut needed,
            )
        };
        if status == windows::Win32::Foundation::STATUS_INFO_LENGTH_MISMATCH {
            let next = (needed as usize / 8 + 1).max(words.len() * 2);
            words.resize(next, 0);
            continue;
        }
        if status != windows::Win32::Foundation::NTSTATUS(0) {
            return Vec::new(); // sampling failed: empty (reads as "—")
        }
        break;
    }
    let buf_len = words.len() * 8;
    let mut out = Vec::new();
    let base = words.as_ptr().cast::<u8>();
    let mut off = 0usize;
    loop {
        if off + std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>() > buf_len {
            break; // torn tail — stop
        }
        // SAFETY: bounds checked above; the record layout matches
        // SYSTEM_PROCESS_INFORMATION (the walk is offset-driven). The
        // buffer is 8-byte aligned (Vec<u64> backing) so the cast to
        // the struct pointer is alignment-sound.
        let rec = unsafe { base.add(off).cast::<SYSTEM_PROCESS_INFORMATION>() };
        let pid = unsafe { (*rec).UniqueProcessId }.0 as usize as u32;
        if pid != 0 {
            // Kernel/User times are hidden in Reserved1: the Vista+
            // layout puts CreateTime@0x20, UserTime@0x28, KernelTime@0x30
            // → Reserved1[32..40] and [40..48] (verified by the layout
            // test below).
            let r1 = &unsafe { (*rec).Reserved1 };
            let q = |i: usize| {
                let mut b = [0u8; 8];
                b.copy_from_slice(&r1[i..i + 8]);
                u64::from_le_bytes(b)
            };
            let user = q(32);
            let kernel = q(40);
            let name = unsafe {
                let us = (*rec).ImageName;
                let len = (us.Length / 2) as usize;
                if us.Buffer.is_null() || len == 0 {
                    String::new()
                } else if (us.Buffer.0 as usize) >= base as usize
                    && (us.Buffer.0 as usize) + len * 2 <= base as usize + buf_len
                {
                    let slice = std::slice::from_raw_parts(us.Buffer.0, len);
                    String::from_utf16_lossy(slice)
                } else {
                    String::new()
                }
            };
            let ws = unsafe { (*rec).WorkingSetSize } as u64;
            out.push((pid, name, kernel, user, ws));
        }
        let next = unsafe { (*rec).NextEntryOffset } as usize;
        if next == 0 {
            break;
        }
        off += next;
    }
    out
}

/// Memory Compression private usage: locate the process in the snapshot
/// (image name "Memory Compression"/"MemCompression"), then
/// `PROCESS_QUERY_LIMITED_INFORMATION` + `K32GetProcessMemoryInfo` with
/// the EX layout → `PrivateUsage`. `None` when unavailable (UI: "—").
fn memory_compression_ws(procs: &[(u32, String, u64, u64, u64)]) -> Option<u64> {
    use windows::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let pid = procs
        .iter()
        .find(|p| {
            let n = p.1.to_ascii_lowercase();
            n == "memory compression" || n == "memcompression"
        })
        .map(|p| p.0)?;
    // SAFETY: pid came from the snapshot; handle closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) };
    let Ok(handle) = handle else {
        return None;
    };
    let cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb,
        ..Default::default()
    };
    // SAFETY: counters sized to its cb (EX layout → PrivateUsage valid);
    // handle has the required access.
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            handle,
            std::ptr::addr_of_mut!(counters)
                .cast::<windows::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
    };
    // SAFETY: balance the OpenProcess.
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) };
    ok.as_bool().then_some(counters.PrivateUsage as u64)
}

/// Fixed + removable mounted volumes with labels + free space.
fn volume_samples() -> Vec<diskbytes_core::monitor::VolumeSample> {
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetLogicalDriveStringsW, GetVolumeInformationW,
    };
    use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;
    const DRIVE_REMOVABLE: u32 = 2; // winbase.h
    let mut buf = [0u16; 512];
    // SAFETY: buffer + capacity in u16s per the documented contract.
    let len = unsafe { GetLogicalDriveStringsW(Some(&mut buf)) } as usize;
    if len == 0 || len >= buf.len() {
        return Vec::new();
    }
    let roots: Vec<String> = buf[..len]
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect();
    let mut out = Vec::new();
    for root in roots {
        let wide_root = wide(&root);
        // SAFETY: NUL-terminated root for both calls.
        let ty = unsafe { GetDriveTypeW(PCWSTR(wide_root.as_ptr())) };
        if ty != DRIVE_FIXED && ty != DRIVE_REMOVABLE {
            continue;
        }
        let mut free: u64 = 0;
        let mut total: u64 = 0;
        let mut total_free: u64 = 0;
        // SAFETY: out-pointers valid.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                PCWSTR(wide_root.as_ptr()),
                Some(&mut free),
                Some(&mut total),
                Some(&mut total_free),
            )
        };
        if ok.is_err() {
            continue;
        }
        let mut label = [0u16; 64];

        // SAFETY: label buffer slice; optional out-pointers are None.
        let _ = unsafe {
            GetVolumeInformationW(
                PCWSTR(wide_root.as_ptr()),
                Some(&mut label),
                None,
                None,
                None,
                None,
            )
        };
        let label_len = label.iter().position(|&c| c == 0).unwrap_or(label.len());
        out.push(diskbytes_core::monitor::VolumeSample {
            root: root.clone(),
            label: String::from_utf16_lossy(&label[..label_len]),
            total,
            free,
        });
    }
    out
}

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

#[cfg(test)]
mod monitor_layout_tests {
    use windows::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION;

    /// Lock the record layout the process walk depends on: ImageName
    /// sits at offset 0x38 (Vista+ layout) so the Reserved1 windows
    /// [32..40]=UserTime, [40..48]=KernelTime derivation is sound.
    #[test]
    fn system_process_information_layout() {
        let pi = SYSTEM_PROCESS_INFORMATION::default();
        let base = std::ptr::addr_of!(pi);
        let name = std::ptr::addr_of!(pi.ImageName);
        let reserved = std::ptr::addr_of!(pi.Reserved1);
        let pid = std::ptr::addr_of!(pi.UniqueProcessId);
        let ws = std::ptr::addr_of!(pi.WorkingSetSize);
        assert_eq!(
            name as usize - base as usize,
            0x38,
            "ImageName must be at 0x38"
        );
        assert_eq!(
            reserved as usize - base as usize,
            0x08,
            "Reserved1 must start at 0x08"
        );
        assert_eq!(
            pid as usize - base as usize,
            0x50,
            "UniqueProcessId must be at 0x50"
        );
        assert!(
            (ws as usize - base as usize) > 0x50,
            "WorkingSetSize follows the header"
        );
        assert_eq!(
            std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>() % 8,
            0,
            "records are 8-byte aligned"
        );
    }
}
