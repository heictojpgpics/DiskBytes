//! Windows directory enumeration engine: NtQueryDirectoryFile with
//! FileIdFullDirectoryInformation, drive roots, known folders, volume
//! serials — plus the shell integration (open/reveal/clipboard).
//! Verbatim move from the old single-file `win.rs` (worklog wave 2a).

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

use crate::platform::{DirEntryData, DirListing, KnownFolder, ListError, Platform};

const BUFFER_WORDS: usize = 256 * 1024 / std::mem::size_of::<u64>();

/// Windows implementation of the [`Platform`] trait.
#[derive(Debug, Clone, Copy, Default)]
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

pub(crate) fn wide(s: &str) -> Vec<u16> {
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

/// Walk one `NtQueryDirectoryFile` result buffer of
/// `FileIdFullDirectoryInformation` records (`returned` bytes valid),
/// by `NextEntryOffset` until 0. Pure over the buffer — the record
/// contract is unit-tested with synthetic buffers (the mac engine's
/// `parse_bulk_record` pattern).
///
/// Returns the entries parsed so far plus the contract-violation
/// message when one is hit mid-walk (the caller surfaces it as
/// `ListError::Other`; partial entries are preserved — the scanner
/// may still surface names when the response is only partly valid,
/// exactly as the inline loop did).
fn walk_records(buffer: &[u64], returned: usize) -> (Vec<DirEntryData>, Option<String>) {
    let mut entries: Vec<DirEntryData> = Vec::new();
    let mut offset = 0usize;
    loop {
        const HEADER_SIZE: usize = std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, FileName);
        // is_none_or is MSRV 1.82; the app targets 1.80 (spec) — map_or.
        if offset
            .checked_add(HEADER_SIZE)
            .map_or(true, |end| end > returned)
        {
            // A record needs HEADER_SIZE bytes: covers both a mid-chain
            // offset past the end AND a buffer shorter than one header
            // (the old `offset > returned.saturating_sub(HEADER)` let
            // the too-short case fall through to the name check with a
            // misleading message — CI's first Windows run caught it).
            return (
                entries,
                Some("record header exceeds the returned length".into()),
            );
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
            return (entries, Some("record has an odd UTF-16 byte length".into()));
        }
        let name_len = (info.FileNameLength / 2) as usize;
        let name_start = offset
            .checked_add(HEADER_SIZE)
            .and_then(|s| s.checked_add(name_len * std::mem::size_of::<u16>()));
        // is_none_or is MSRV 1.82; the app targets 1.80 (spec).
        if name_start.map_or(true, |end| end > returned) {
            return (
                entries,
                Some("record name exceeds the returned length".into()),
            );
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
        let is_dotdot = name.len() == 2 && name[0] == u16::from(b'.') && name[1] == u16::from(b'.');
        if !is_dot && !is_dotdot {
            let attrs = info.FileAttributes;
            let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
            let cloud = attrs
                & (FILE_ATTRIBUTE_OFFLINE.0
                    | FILE_ATTRIBUTE_RECALL_ON_OPEN.0
                    | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0)
                != 0;
            let reparse_tag = (attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0).then_some(info.EaSize);
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
            return (entries, Some("NextEntryOffset overflow".into()));
        };
        if next >= returned {
            // The final record's offset may point past the end —
            // enumeration continues via another Nt call only when
            // more data exists; treat as end of this buffer.
            break;
        }
        offset = next;
    }
    (entries, None)
}

impl Platform for WindowsPlatform {
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

            let (batch, violation) = walk_records(&buffer, returned);
            entries.extend(batch);
            if let Some(msg) = violation {
                error = Some(ListError::Other(msg));
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
pub(crate) fn root_of(verbatim: &str) -> Option<String> {
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

#[cfg(test)]
mod record_walk_tests {
    //! Synthetic `FileIdFullDirectoryInformation` buffers — the record
    //! contract locked the same way the mac engine locks
    //! `parse_bulk_record` (kernel-shaped bytes, no kernel needed).
    //! Plus one REAL-syscall E2E on the Windows runner (the parity the
    //! mac bulk-parser test has had since wave 1e).
    use super::*;

    const ATTR_DIR: u32 = FILE_ATTRIBUTE_DIRECTORY.0;
    const ATTR_REPARSE: u32 = FILE_ATTRIBUTE_REPARSE_POINT.0;
    const ATTR_CLOUD: u32 = FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0;

    /// FILETIME of 1970-01-01 (unix 0) — `created` reads back 0, meaning
    /// unknown, per the contract. `LastWriteTime` gets +1 s.
    const FT_UNIX_EPOCH: i64 = 11_644_473_600 * 10_000_000;
    const FT_UNIX_EPOCH_PLUS_1S: i64 = FT_UNIX_EPOCH + 10_000_000;

    /// Field offsets taken from the real struct once, at module level —
    /// the record builder and the violation tests share them (ABI-locked
    /// by `layout_offsets_are_locked`).
    const O_CRT: usize = std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, CreationTime);
    const O_WR: usize = std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, LastWriteTime);
    const O_EOF: usize = std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, EndOfFile);
    const O_ALC: usize = std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, AllocationSize);
    const O_ATR: usize = std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, FileAttributes);
    const O_LEN: usize = std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, FileNameLength);
    const O_EA: usize = std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, EaSize);
    const O_FID: usize = std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, FileId);
    const HEADER: usize = std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, FileName);

    /// One synthetic record: header + UTF-16 name. `NextEntryOffset` is
    /// left 0 — `pack` chains records. Field offsets come from
    /// `offset_of!` on the real struct so the builder can never drift
    /// from the ABI.
    fn rec(name: &str, attrs: u32, logical: i64, alloc: i64) -> Vec<u8> {
        let name16: Vec<u16> = name.encode_utf16().collect();
        let mut b = vec![0u8; HEADER + name16.len() * 2];
        b[O_CRT..O_CRT + 8].copy_from_slice(&FT_UNIX_EPOCH.to_le_bytes());
        b[O_WR..O_WR + 8].copy_from_slice(&FT_UNIX_EPOCH_PLUS_1S.to_le_bytes());
        b[O_EOF..O_EOF + 8].copy_from_slice(&logical.to_le_bytes());
        b[O_ALC..O_ALC + 8].copy_from_slice(&alloc.to_le_bytes());
        b[O_ATR..O_ATR + 4].copy_from_slice(&attrs.to_le_bytes());
        b[O_LEN..O_LEN + 4].copy_from_slice(&((name16.len() * 2) as u32).to_le_bytes());
        // A plausible reparse tag (IO_REPARSE_TAG_SYMLINK): only surfaced
        // when ATTR_REPARSE is set.
        b[O_EA..O_EA + 4].copy_from_slice(&0xA000_0003u32.to_le_bytes());
        b[O_FID..O_FID + 8].copy_from_slice(&0x1234_5678_9ABCi64.to_le_bytes());
        for (i, &u) in name16.iter().enumerate() {
            b[HEADER + i * 2..HEADER + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        b
    }

    /// Chain records into one engine-shaped buffer: 8-byte aligned, each
    /// record's `NextEntryOffset` pointing at the next, the final one 0.
    /// Returns the u64-word buffer and the `returned` byte count.
    fn pack(recs: Vec<Vec<u8>>) -> (Vec<u64>, usize) {
        let mut recs = recs;
        for r in &mut recs {
            while r.len() % 8 != 0 {
                r.push(0);
            }
        }
        let mut out: Vec<u8> = Vec::new();
        let last = recs.len() - 1;
        for (i, r) in recs.iter().enumerate() {
            let next: u32 = if i == last { 0 } else { r.len() as u32 };
            let mut r = r.clone();
            r[0..4].copy_from_slice(&next.to_le_bytes());
            out.extend_from_slice(&r);
        }
        let returned = out.len();
        let mut buffer: Vec<u64> = out
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().expect("chunk is 8 bytes")))
            .collect();
        buffer.resize(BUFFER_WORDS, 0);
        (buffer, returned)
    }

    fn names(entries: &[DirEntryData]) -> Vec<String> {
        entries
            .iter()
            .map(|e| String::from_utf16_lossy(&e.name))
            .collect()
    }

    #[test]
    fn layout_offsets_are_locked() {
        // The ABI the manual record builder depends on — VERIFIED ON THE
        // REAL WINDOWS RUNNER: FileId sits at 72, not 68 (LARGE_INTEGER
        // alignment inserts 4 bytes of padding after EaSize at 64), so
        // FileName starts at 80. The first run of this test caught the
        // hand-written 68/76 assumption.
        assert_eq!(
            std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, NextEntryOffset),
            0
        );
        assert_eq!(
            std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, FileAttributes),
            56
        );
        assert_eq!(
            std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, EaSize),
            64
        );
        assert_eq!(
            std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, FileId),
            72
        );
        assert_eq!(
            std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, FileName),
            80
        );
    }

    #[test]
    fn single_record_parses_every_field() {
        let (buffer, returned) = pack(vec![rec("alpha.txt", 0, 100, 4096)]);
        let (entries, err) = walk_records(&buffer, returned);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.name, "alpha.txt".encode_utf16().collect::<Vec<u16>>());
        assert!(!e.is_dir);
        assert_eq!(e.logical, 100);
        assert_eq!(e.on_disk, 4096);
        assert_eq!(e.modified, 1, "LastWriteTime = epoch+1s -> unix 1");
        assert_eq!(e.created, 0, "CreationTime = epoch -> unix 0 (unknown)");
        assert!(e.reparse_tag.is_none(), "no reparse attribute");
        assert!(!e.cloud);
        assert_eq!(e.file_id, 0x1234_5678_9ABC);
    }

    #[test]
    fn chained_records_parse_in_order_with_all_kinds() {
        let (buffer, returned) = pack(vec![
            rec("folder", ATTR_DIR, 0, 0),
            rec("data.bin", 0, 8192, 8192),
            rec("link.lnk", ATTR_REPARSE, 40, 40),
            rec("cloud.dat", ATTR_CLOUD, 1_000, 1_000),
        ]);
        let (entries, err) = walk_records(&buffer, returned);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(
            names(&entries),
            ["folder", "data.bin", "link.lnk", "cloud.dat"]
        );
        assert!(entries[0].is_dir);
        assert!(!entries[1].is_dir);
        assert_eq!(entries[1].logical, 8192);
        // The reparse tag rides in EaSize (spec §4) and marks
        // never-descend.
        assert_eq!(entries[2].reparse_tag, Some(0xA000_0003));
        // The cloud attribute is the recall-on-data-access placeholder.
        assert!(entries[3].cloud);
    }

    #[test]
    fn dot_and_dotdot_are_skipped() {
        let (buffer, returned) = pack(vec![
            rec(".", ATTR_DIR, 0, 0),
            rec("..", ATTR_DIR, 0, 0),
            rec("real.txt", 0, 5, 4096),
        ]);
        let (entries, err) = walk_records(&buffer, returned);
        assert!(err.is_none());
        assert_eq!(names(&entries), ["real.txt"]);
    }

    #[test]
    fn truncated_header_is_a_violation() {
        let buffer = vec![0u64; BUFFER_WORDS];
        let (_e, err) = walk_records(&buffer, 10); // < 76-byte header
        assert_eq!(
            err.as_deref(),
            Some("record header exceeds the returned length")
        );
    }

    #[test]
    fn name_past_returned_is_a_violation() {
        let mut r = rec("waytoolongname.txt", 0, 1, 1);
        // Claim a name far longer than the buffer holds.
        r[O_LEN..O_LEN + 4].copy_from_slice(&100_000u32.to_le_bytes());
        let mut buffer: Vec<u64> = r
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        buffer.resize(BUFFER_WORDS, 0);
        let (_e, err) = walk_records(&buffer, r.len());
        assert_eq!(
            err.as_deref(),
            Some("record name exceeds the returned length")
        );
    }

    #[test]
    fn odd_utf16_length_is_a_violation() {
        let mut r = rec("odd.bin", 0, 1, 1);
        r[O_LEN..O_LEN + 4].copy_from_slice(&3u32.to_le_bytes());
        let mut buffer: Vec<u64> = r
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        buffer.resize(BUFFER_WORDS, 0);
        let (_e, err) = walk_records(&buffer, r.len());
        assert_eq!(err.as_deref(), Some("record has an odd UTF-16 byte length"));
    }

    #[test]
    fn partial_entries_survive_a_mid_walk_violation() {
        // Record 1 parses; record 2 claims a name past the buffer. The
        // walk must hand back record 1 plus the violation — exactly the
        // inline loop's keep-what-parsed semantics.
        let mut recs = vec![rec("good.txt", 0, 7, 4096), rec("bad.txt", 0, 7, 4096)];
        for r in &mut recs {
            while r.len() % 8 != 0 {
                r.push(0);
            }
        }
        let first_len = recs[0].len() as u32;
        recs[0][0..4].copy_from_slice(&first_len.to_le_bytes());
        recs[1][O_LEN..O_LEN + 4].copy_from_slice(&90_000u32.to_le_bytes());
        let mut flat: Vec<u8> = Vec::new();
        for r in &recs {
            flat.extend_from_slice(r);
        }
        let returned = flat.len();
        let mut buffer: Vec<u64> = flat
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        buffer.resize(BUFFER_WORDS, 0);
        let (entries, err) = walk_records(&buffer, returned);
        assert_eq!(
            err.as_deref(),
            Some("record name exceeds the returned length")
        );
        assert_eq!(names(&entries), ["good.txt"], "parsed prefix is kept");
    }

    #[test]
    fn final_offset_past_returned_ends_the_buffer_cleanly() {
        // The last record's NextEntryOffset may point past `returned` —
        // documented as end-of-buffer, not an error.
        let mut recs = vec![rec("one.txt", 0, 1, 1), rec("two.txt", 0, 2, 2)];
        for r in &mut recs {
            while r.len() % 8 != 0 {
                r.push(0);
            }
        }
        let first_len = recs[0].len() as u32;
        recs[0][0..4].copy_from_slice(&first_len.to_le_bytes());
        recs[1][0..4].copy_from_slice(&1_000u32.to_le_bytes()); // past the end
        let mut flat: Vec<u8> = Vec::new();
        for r in &recs {
            flat.extend_from_slice(r);
        }
        let returned = flat.len();
        let mut buffer: Vec<u64> = flat
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        buffer.resize(BUFFER_WORDS, 0);
        let (entries, err) = walk_records(&buffer, returned);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(names(&entries), ["one.txt", "two.txt"]);
    }

    #[test]
    fn unicode_names_round_trip_exactly() {
        let (buffer, returned) = pack(vec![
            rec("café.txt", 0, 1, 1),
            rec("日本語.md", 0, 1, 1),
            rec("emoji-📁.txt", 0, 1, 1),
        ]);
        let (entries, err) = walk_records(&buffer, returned);
        assert!(err.is_none(), "{err:?}");
        let got = names(&entries);
        assert_eq!(
            got,
            [
                "café.txt".to_string(),
                "日本語.md".to_string(),
                "emoji-📁.txt".to_string()
            ]
        );
        // Surrogate pairs preserved exactly (no lossy folding).
        assert_eq!(
            entries[2].name,
            "emoji-📁.txt".encode_utf16().collect::<Vec<u16>>()
        );
    }

    #[test]
    fn filetime_zero_and_epoch_map_to_unknown() {
        assert_eq!(filetime_to_unix(0), 0);
        assert_eq!(filetime_to_unix(FT_UNIX_EPOCH as u64), 0);
        assert_eq!(filetime_to_unix(FT_UNIX_EPOCH_PLUS_1S as u64), 1);
    }

    /// THE REAL SYSCALL, end to end (the parity the mac engine's staged
    /// test has had since wave 1e): stage a directory with known files
    /// — including non-ASCII names — run `WindowsPlatform::list_dir`
    /// (CreateFileW + NtQueryDirectoryFile + walk_records) and assert.
    /// Win32 CREATE-time name normalization may rewrite odd shapes, so
    /// assertions compare against what `read_dir` says actually landed.
    #[test]
    fn real_ntquerydirectoryfile_enumerates_a_staged_directory() {
        let dir = std::env::temp_dir().join(format!(
            "db-ntwalk-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).expect("stage dirs");
        std::fs::write(dir.join("alpha.txt"), b"hello world").expect("stage alpha");
        std::fs::write(dir.join("beta.bin"), [0u8; 8192]).expect("stage beta");
        std::fs::write(dir.join("café.txt"), b"accents").expect("stage cafe");
        std::fs::write(sub.join("日本語.md"), b"cjk").expect("stage cjk");
        let verbatim = format!(r"\\?\{}", dir.display());
        let listing = WindowsPlatform.list_dir(&verbatim);
        assert!(listing.error.is_none(), "engine error: {:?}", listing.error);
        let got = names(&listing.entries);
        assert!(got.contains(&"alpha.txt".to_string()), "names: {got:?}");
        assert!(got.contains(&"beta.bin".to_string()), "names: {got:?}");
        assert!(got.contains(&"sub".to_string()), "names: {got:?}");
        assert!(got.contains(&"café.txt".to_string()), "mojibake: {got:?}");
        let beta = listing
            .entries
            .iter()
            .find(|e| e.name == "beta.bin".encode_utf16().collect::<Vec<u16>>())
            .expect("beta entry");
        assert!(!beta.is_dir);
        assert_eq!(beta.logical, 8192);
        assert!(beta.on_disk >= 8192, "allocation: {}", beta.on_disk);
        assert!(beta.modified > 0, "mtime: {}", beta.modified);
        let sub_e = listing
            .entries
            .iter()
            .find(|e| e.name == "sub".encode_utf16().collect::<Vec<u16>>())
            .expect("sub entry");
        assert!(sub_e.is_dir);
        // list_dir is ONE level: sub's CJK file asserts via a second call.
        let sub_listing = WindowsPlatform.list_dir(&format!(r"\\?\{}", sub.display()));
        assert!(sub_listing.error.is_none(), "{:?}", sub_listing.error);
        let sub_got = names(&sub_listing.entries);
        assert!(
            sub_got.contains(&"日本語.md".to_string()),
            "mojibake (CJK): {sub_got:?}"
        );
    }
}
