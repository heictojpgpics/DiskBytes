//! Recycle Bin moves (spec §9, BuildPrompt §1 safety promise):
//! EVERYTHING goes to the Recycle Bin through `IFileOperation` — this
//! crate contains NO direct-delete API (the doc 09 §2 grep is the gate).
//!
//! Flow:
//! 1. `plan_commit` (pure, host-testable): sort shortest-first; items
//!    nested inside another staged item are absorbed by it.
//! 2. Pre-flight (per item, spec §9): DRIVE_FIXED volume, the volume's
//!    bin is not `NukeOnDelete == 1`, the item fits the bin's
//!    `MaxCapacity`, and it is not protected. Failing items get a clear
//!    reason instead of being attempted. Missing items count as already
//!    gone.
//! 3. `IFileOperation` with `FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE` and
//!    NOT `FOF_NOCONFIRMATION` / `FOF_NOERRORUI` (Windows may show UAC
//!    for Program Files items). Per-item `HRESULT`s come from an
//!    `IFileOperationProgressSink` (`PostDeleteItem`).
//!
//! windows-rs usage flows through `platform::win::recycle_seam`
//! (doc 02 §2 footnote).

// COM calls below carry SAFETY comments (the doc 02 §2 footnote
// sanctions this module as a windows-rs consumer through the
// `platform::win::recycle_seam` re-exports).
#![allow(unsafe_code)]
// The #[implement] macro emits code that trips ref_as_ptr /
// inline_always / undocumented_unsafe_blocks; the COM boundary is
// reviewed as a unit (same allowance as win.rs).
#![allow(clippy::undocumented_unsafe_blocks)]
#![allow(clippy::ref_as_ptr)]
#![allow(clippy::inline_always)]

use serde::Serialize;
use std::cmp::Ordering;

#[cfg(windows)]
use std::sync::Arc;

#[cfg(windows)]
use parking_lot::Mutex;

use crate::platform::os::{bin_policy_for, path_missing, path_on_fixed_drive};

#[cfg(windows)]
use crate::platform::os::recycle_seam;

#[cfg(windows)]
use crate::platform::os::ComApartment;

/// One staged item as the command layer resolved it (tree flags joined).
#[derive(Debug, Clone)]
pub struct StagedPath {
    /// Real node id (0 = synthetic/no node, e.g. leftovers paths).
    #[allow(dead_code)]
    pub id: u32,
    /// Display path.
    pub path: String,
    /// On-disk size in bytes.
    pub size: u64,
    /// Protected nodes are refused (spec §9 + R7.4).
    pub protected: bool,
}

/// One recycled item.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashedItem {
    pub path: String,
    /// The item was already gone (counted as recycled, no-op).
    pub already_gone: bool,
    /// Nested inside another recycled item (counted with the parent).
    pub nested: bool,
}

/// One refused item with a user-readable reason.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedItem {
    pub path: String,
    pub reason: String,
}

/// The commit outcome (spec §9: `{trashed, failed: [{path, reason}]}`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecycleOutcome {
    pub trashed: Vec<TrashedItem>,
    pub failed: Vec<FailedItem>,
}

/// The planned commit after the pure pass: effective roots sorted
/// shortest-first + the nested/missing classification.
#[derive(Debug, Clone)]
pub struct CommitPlan {
    /// Effective items (shortest path first).
    pub items: Vec<StagedPath>,
    /// Items removed by absorption (nested inside another staged
    /// item): their path (for honest UI accounting — they ARE recycled
    /// with their parent) plus the index of the absorbing root in
    /// `items`.
    pub absorbed: Vec<AbsorbedItem>,
}

/// One staged item absorbed by an ancestor (see [`CommitPlan`]).
#[derive(Debug, Clone)]
pub struct AbsorbedItem {
    /// The absorbed (nested) item's path.
    pub path: String,
    /// Index into `CommitPlan::items` of the absorbing root (metadata
    /// for future UI grouping; read by tests — clippy's dead-code scan
    /// intentionally ignores test reads).
    #[allow(dead_code)]
    pub absorbed_by: usize,
}

/// Pure planning pass (spec §9: "Sort paths shortest-first. Items nested
/// inside an already-recycled folder count as recycled"). A path is
/// nested when it equals or starts with `parent + '\'`.
#[must_use]
pub fn plan_commit(items: Vec<StagedPath>) -> CommitPlan {
    let mut sorted = items;
    sorted.sort_by(|a, b| match a.path.len().cmp(&b.path.len()) {
        Ordering::Equal => a.path.cmp(&b.path),
        other => other,
    });
    // Absorb nested paths into their ancestor (both staged).
    let mut keep: Vec<StagedPath> = Vec::with_capacity(sorted.len());
    let mut absorbed: Vec<AbsorbedItem> = Vec::new();
    for item in sorted {
        let mut absorbed_by: Option<usize> = None;
        for (i, k) in keep.iter().enumerate() {
            let nested = item.path.len() > k.path.len()
                && (item.path.starts_with(&k.path)
                    && item.path.as_bytes().get(k.path.len()) == Some(&b'\\'));
            if nested {
                absorbed_by = Some(i);
                break;
            }
        }
        match absorbed_by {
            Some(i) => absorbed.push(AbsorbedItem {
                path: item.path,
                absorbed_by: i,
            }),
            None => keep.push(item),
        }
    }
    CommitPlan {
        items: keep,
        absorbed,
    }
}

/// Pre-flight one item (spec §9). Returns Err(reason) to REFUSE instead
/// of attempting the move.
fn preflight(item: &StagedPath) -> Result<(), String> {
    if item.protected {
        return Err("This item is protected by Windows and can't be staged for cleanup.".into());
    }
    if path_missing(&item.path) {
        // Handled by the caller (already_gone), not a failure.
        return Ok(());
    }
    if !path_on_fixed_drive(&item.path) {
        return Err("This item is not on a fixed drive, so it can't go to the Recycle Bin.".into());
    }
    let policy = bin_policy_for(&item.path);
    if policy.nuke_on_delete {
        return Err(
            "The Recycle Bin is disabled on this drive — deleting would be permanent, so it was refused.".into(),
        );
    }
    if let Some(cap_mb) = policy.max_capacity_mb {
        if cap_mb > 0 && item.size > cap_mb * 1_048_576 {
            return Err(format!(
                "This item ({}) is larger than this drive's Recycle Bin capacity ({}), so it can't be recycled.",
                humans(item.size),
                humans(cap_mb * 1_048_576)
            ));
        }
    }
    Ok(())
}

/// Minimal human-readable bytes for pre-flight reasons.
fn humans(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < 4 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Move every staged item to the Recycle Bin / Trash (spec §9 contract;
/// the platform pass is `com_recycle_pass` — IFileOperation on Windows,
/// NSWorkspace recycleURLs on macOS). Runs on a blocking thread (the
/// command layer calls this inside `spawn_blocking`).
///
/// # Errors
/// String error when COM cannot initialize or the operation object
/// cannot be created — per-item problems land in `failed` instead.
pub fn move_to_recycle_bin(items: Vec<StagedPath>) -> Result<RecycleOutcome, String> {
    let plan = plan_commit(items);
    let mut trashed: Vec<TrashedItem> = Vec::new();
    let mut failed: Vec<FailedItem> = Vec::new();

    // Pass 1: pre-flight + missing detection.
    let mut candidates: Vec<StagedPath> = Vec::new();
    for item in &plan.items {
        if path_missing(&item.path) {
            trashed.push(TrashedItem {
                path: item.path.clone(),
                already_gone: true,
                nested: false,
            });
            continue;
        }
        match preflight(item) {
            Ok(()) => candidates.push(item.clone()),
            Err(reason) => failed.push(FailedItem {
                path: item.path.clone(),
                reason,
            }),
        }
    }
    // Absorbed (nested) items count as recycled with their parent: the
    // plan keeps only the absorbing root, so every nested path re-joins
    // the trashed list marked `nested` for honest UI accounting (the
    // frontend removes queue items by trashed path — without these
    // entries absorbed items lingered in the queue after a commit).
    for abs in &plan.absorbed {
        trashed.push(TrashedItem {
            path: abs.path.clone(),
            already_gone: false,
            nested: true,
        });
    }

    if candidates.is_empty() {
        return Ok(RecycleOutcome { trashed, failed });
    }

    let com_trashed = com_recycle_pass(&candidates, &mut failed)?;
    trashed.extend(com_trashed);
    Ok(RecycleOutcome { trashed, failed })
}

/// The progress sink (Windows): records per-item `PostDeleteItem` results
/// keyed by the item's parsing path (spec §9: "Collect per-item HRESULTs").
#[cfg(windows)]
mod windows_pass {
    #![allow(
        clippy::undocumented_unsafe_blocks,
        clippy::ref_as_ptr,
        clippy::inline_always
    )]
    use super::{recycle_seam, Arc, ComApartment, FailedItem, Mutex, StagedPath, TrashedItem};

    #[recycle_seam::implement(recycle_seam::IFileOperationProgressSink)]
    struct DeleteSink {
        results: Arc<Mutex<Vec<(String, i32)>>>,
    }

    impl recycle_seam::IFileOperationProgressSink_Impl for DeleteSink_Impl {
        fn PostDeleteItem(
            &self,
            _dwflags: u32,
            psiitem: windows::core::Ref<'_, recycle_seam::IShellItem>,
            hrdelete: windows::core::HRESULT,
            _psinewlycreated: windows::core::Ref<'_, recycle_seam::IShellItem>,
        ) -> windows::core::Result<()> {
            if let Ok(item) = psiitem.ok() {
                // SAFETY: COM call with a valid IShellItem; PWSTR owned by
                // GetDisplayName is freed below.
                let name = unsafe {
                    item.GetDisplayName(recycle_seam::SIGDN_DESKTOPABSOLUTEPARSING)
                        .ok()
                        .and_then(|p| {
                            let s = p.to_string().ok();
                            // CoTaskMemFree the PWSTR (the documented owner).
                            windows::Win32::System::Com::CoTaskMemFree(Some(p.as_ptr().cast()));
                            s
                        })
                };
                if let Some(path) = name {
                    self.results.lock().push((path, hrdelete.0));
                }
            }
            Ok(())
        }

        fn PreDeleteItem(
            &self,
            _dwflags: u32,
            _psiitem: windows::core::Ref<'_, recycle_seam::IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }

        fn StartOperations(&self) -> windows::core::Result<()> {
            Ok(())
        }

        fn FinishOperations(&self, _hrresult: windows::core::HRESULT) -> windows::core::Result<()> {
            Ok(())
        }

        fn PostRenameItem(
            &self,
            _dwflags: u32,
            _psiitem: windows::core::Ref<'_, recycle_seam::IShellItem>,
            _psznewname: &windows::core::PCWSTR,
            _hrrename: windows::core::HRESULT,
            _psinewlycreated: windows::core::Ref<'_, recycle_seam::IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }

        fn PreRenameItem(
            &self,
            _dwflags: u32,
            _psiitem: windows::core::Ref<'_, recycle_seam::IShellItem>,
            _psznewname: &windows::core::PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }

        fn PostMoveItem(
            &self,
            _dwflags: u32,
            _psiitem: windows::core::Ref<'_, recycle_seam::IShellItem>,
            _psidestinationfolder: windows::core::Ref<'_, recycle_seam::IShellItem>,
            _psznewname: &windows::core::PCWSTR,
            _hrmove: windows::core::HRESULT,
            _psinewlycreated: windows::core::Ref<'_, recycle_seam::IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }

        fn PreMoveItem(
            &self,
            _dwflags: u32,
            _psiitem: windows::core::Ref<'_, recycle_seam::IShellItem>,
            _psidestinationfolder: windows::core::Ref<'_, recycle_seam::IShellItem>,
            _psznewname: &windows::core::PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }

        fn PostCopyItem(
            &self,
            _dwflags: u32,
            _psiitem: windows::core::Ref<'_, recycle_seam::IShellItem>,
            _psidestinationfolder: windows::core::Ref<'_, recycle_seam::IShellItem>,
            _psznewname: &windows::core::PCWSTR,
            _hrcopy: windows::core::HRESULT,
            _psinewlycreated: windows::core::Ref<'_, recycle_seam::IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }

        fn PreCopyItem(
            &self,
            _dwflags: u32,
            _psiitem: windows::core::Ref<'_, recycle_seam::IShellItem>,
            _psidestinationfolder: windows::core::Ref<'_, recycle_seam::IShellItem>,
            _psznewname: &windows::core::PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }

        fn UpdateProgress(&self, _iworktotal: u32, _iworksofar: u32) -> windows::core::Result<()> {
            Ok(())
        }

        fn ResetTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }

        fn PauseTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }

        fn ResumeTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }

        fn PostNewItem(
            &self,
            _dwflags: u32,
            _psidestinationfolder: windows::core::Ref<'_, recycle_seam::IShellItem>,
            _psznewname: &windows::core::PCWSTR,
            _psztemplatename: &windows::core::PCWSTR,
            _dwfileattributes: u32,
            _hrnew: windows::core::HRESULT,
            _psinewitem: windows::core::Ref<'_, recycle_seam::IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }

        fn PreNewItem(
            &self,
            _dwflags: u32,
            _psidestinationfolder: windows::core::Ref<'_, recycle_seam::IShellItem>,
            _psznewname: &windows::core::PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
    }

    /// The COM pass: one `IFileOperation` over the pre-flighted candidates
    /// (per-item `PostDeleteItem` HRESULTs through the sink).
    pub(crate) fn com_recycle_pass(
        candidates: &[StagedPath],
        failed: &mut Vec<FailedItem>,
    ) -> Result<Vec<TrashedItem>, String> {
        let mut trashed: Vec<TrashedItem> = Vec::new();

        // Pass 2: IFileOperation with the progress sink.
        let com = ComApartment::init()?;
        let results: Arc<Mutex<Vec<(String, i32)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = DeleteSink {
            results: Arc::clone(&results),
        };
        // SAFETY: COM apartment initialized above on THIS thread.
        let op: recycle_seam::IFileOperation = unsafe {
            windows::Win32::System::Com::CoCreateInstance(
                &recycle_seam::FileOperation,
                None,
                recycle_seam::CLSCTX_ALL,
            )
        }
        .map_err(|e| format!("Windows file operation unavailable: {e}"))?;

        // SAFETY: flags per spec §9: ALLOWUNDO + RECYCLEONDELETE only
        // (Windows shows its own confirmations/UAC).
        unsafe {
            op.SetOperationFlags(recycle_seam::FOF_ALLOWUNDO | recycle_seam::FOFX_RECYCLEONDELETE)
        }
        .map_err(|e| format!("Couldn't configure the Recycle Bin operation: {e}"))?;
        let sink_iface: recycle_seam::IFileOperationProgressSink = sink.into();
        let cookie = unsafe { op.Advise(&sink_iface) }
            .map_err(|e| format!("Couldn't attach the operation progress: {e}"))?;

        let mut queued_paths: Vec<String> = Vec::new();
        for item in candidates {
            // SAFETY: parsing-name item creation from a valid path.
            let shell_item: recycle_seam::IShellItem = if let Ok(item_obj) = unsafe {
                recycle_seam::SHCreateItemFromParsingName(
                    &windows::core::HSTRING::from(item.path.as_str()),
                    None,
                )
            } {
                item_obj
            } else {
                // Vanished between pre-flight and queueing.
                trashed.push(TrashedItem {
                    path: item.path.clone(),
                    already_gone: true,
                    nested: false,
                });
                continue;
            };
            // SAFETY: DeleteItem with the per-item sink (per-item HRESULTs).
            if let Err(e) = unsafe { op.DeleteItem(&shell_item, &sink_iface) } {
                failed.push(FailedItem {
                    path: item.path.clone(),
                    reason: format!("Windows refused this item: {e}"),
                });
                continue;
            }
            queued_paths.push(item.path.clone());
        }

        // SAFETY: performs the queued operations (Windows UI allowed).
        if let Err(e) = unsafe { op.PerformOperations() } {
            // The sink may still hold partial results; surface the overall
            // failure honestly.
            failed.push(FailedItem {
                path: queued_paths.first().cloned().unwrap_or_default(),
                reason: format!("The Recycle Bin operation failed: {e}"),
            });
        }
        // SAFETY: unadvise with the cookie from Advise.
        unsafe { op.Unadvise(cookie) }.ok();
        drop(op);
        drop(sink_iface);

        // Pass 3: reconcile sink results with the queued set.
        let sink_results =
            Arc::try_unwrap(results).map_or_else(|_| Vec::new(), parking_lot::Mutex::into_inner);
        for path in queued_paths {
            let hr = sink_results
                .iter()
                .find(|(p, _)| paths_equal(p, &path))
                .map(|(_, hr)| *hr);
            match hr {
                // No callback + PerformOperations ok = recycled (the sink
                // only reports some item kinds).
                Some(0) | None => trashed.push(TrashedItem {
                    path,
                    already_gone: false,
                    nested: false,
                }),
                Some(code) => failed.push(FailedItem {
                    path,
                    reason: format!(
                        "Windows couldn't recycle this item (error {}): {}",
                        code,
                        hr_message(code)
                    ),
                }),
            }
        }

        let _ = com;
        Ok(trashed)
    }

    /// Case-insensitive path compare (Windows semantics).
    pub(super) fn paths_equal(a: &str, b: &str) -> bool {
        a.eq_ignore_ascii_case(b)
    }

    /// User-readable meaning for common delete HRESULTs.
    pub(super) fn hr_message(hr: i32) -> &'static str {
        // Reinterpret the i32 as its u32 HRESULT bits (two's complement):
        // FAILED HRESULTs have the high bit set → NEGATIVE as i32, and
        // `u32::try_from` would reject them (→ 0 → generic fallback) — the
        // whole table was dead code before this fix (CI caught it).
        let hr = u32::from_ne_bytes(hr.to_ne_bytes());
        match hr {
            // E_ACCESSDENIED = HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED):
            // 0x80070005.
            0x8007_0005 => "access denied — try restarting as administrator",
            0x8000_4005 => "unspecified error — the item was not recycled",
            0x8007_0070 => "there is not enough space on the disk",
            0x8007_0002 => "the system cannot find the file",
            0x8007_0003 => "the system cannot find the path",
            0x8007_00AA => "the resource is in use",
            _ => "the item was not recycled",
        }
    }
}

#[cfg(target_os = "macos")]
mod mac_pass {
    // Explicit imports (no `use super::*` glob): the wildcard hid what
    // the mac pass actually consumes from the parent module.
    use super::{FailedItem, StagedPath, TrashedItem};
    use crate::platform::os::recycle_to_trash;

    /// The macOS Trash pass: NSWorkspace.recycleURLs — the same
    /// pre-flight contract, a Finder Trash move (never a hard delete).
    pub(crate) fn com_recycle_pass(
        candidates: &[StagedPath],
        failed: &mut Vec<FailedItem>,
    ) -> Result<Vec<TrashedItem>, String> {
        let paths: Vec<String> = candidates.iter().map(|c| c.path.clone()).collect();
        let results = recycle_to_trash(&paths)?;
        let mut trashed = Vec::new();
        for (path, outcome) in results {
            match outcome {
                Ok(()) => trashed.push(TrashedItem {
                    path,
                    already_gone: false,
                    nested: false,
                }),
                Err(reason) => failed.push(FailedItem { path, reason }),
            }
        }
        Ok(trashed)
    }
}

#[cfg(windows)]
use windows_pass::com_recycle_pass;

#[cfg(target_os = "macos")]
use mac_pass::com_recycle_pass;

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, size: u64, protected: bool) -> StagedPath {
        StagedPath {
            id: 0,
            path: path.to_string(),
            size,
            protected,
        }
    }

    #[test]
    fn plan_sorts_shortest_first() {
        let plan = plan_commit(vec![
            item(r"C:\deep\nested\thing.bin", 10, false),
            item(r"C:\a", 10, false),
            item(r"C:\medium\folder", 10, false),
        ]);
        assert_eq!(plan.items[0].path, r"C:\a");
        assert_eq!(plan.items[1].path, r"C:\medium\folder");
        assert_eq!(plan.items.len(), 3);
        assert!(plan.absorbed.is_empty());
    }

    #[test]
    fn plan_absorbs_nested() {
        let plan = plan_commit(vec![
            item(r"C:\folder", 100, false),
            item(r"C:\folder\inner.txt", 5, false),
            item(r"C:\folder\sub", 20, false),
            item(r"C:\folder\sub\file.bin", 8, false),
        ]);
        // Only the top folder survives.
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].path, r"C:\folder");
        assert_eq!(plan.absorbed.len(), 3);
        // The absorbed paths are preserved for UI accounting, each
        // pointing at the absorbing root's index.
        let paths: Vec<&str> = plan.absorbed.iter().map(|a| a.path.as_str()).collect();
        assert!(paths.contains(&r"C:\folder\inner.txt"));
        assert!(paths.contains(&r"C:\folder\sub"));
        assert!(paths.contains(&r"C:\folder\sub\file.bin"));
        assert!(plan.absorbed.iter().all(|a| a.absorbed_by == 0));
    }

    #[test]
    fn plan_prefix_without_separator_is_not_nested() {
        // C:\folder-2 does not live inside C:\folder.
        let plan = plan_commit(vec![
            item(r"C:\folder", 100, false),
            item(r"C:\folder-2", 20, false),
        ]);
        assert_eq!(plan.items.len(), 2);
        assert!(plan.absorbed.is_empty());
    }

    #[test]
    fn humans_readable() {
        assert_eq!(humans(500), "500 B");
        assert_eq!(humans(2 * 1024 * 1024), "2.0 MB");
    }

    #[cfg(windows)]
    #[test]
    fn hr_messages_map() {
        use super::windows_pass::hr_message;
        // E_ACCESSDENIED = 0x80070005 as i32 (negative — the case that
        // exposed the try_from dead-table bug).
        assert!(hr_message(-2_147_024_891).contains("administrator"));
        // E_FAIL = 0x80004005 as i32.
        assert!(hr_message(-2_147_467_259).contains("not recycled"));
        // Disk full = 0x80070070 as i32.
        assert!(hr_message(-2_147_024_784).contains("not enough space"));
        assert!(hr_message(1).contains("not recycled"));
    }

    #[cfg(windows)]
    #[test]
    fn paths_equal_case_insensitive() {
        use super::windows_pass::paths_equal;
        assert!(paths_equal(r"C:\A\B", r"c:\a\b"));
        assert!(!paths_equal(r"C:\A\B", r"C:\A\C"));
    }
}
