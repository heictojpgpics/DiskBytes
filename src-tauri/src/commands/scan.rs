//! Scan commands (spec §4; doc 03 M3.7): `start_scan(target)` and
//! `get_status`, plus the `scan-progress` (150 ms) and `scan-done`
//! events and the §15 dev hooks.
//!
//! Starting a new scan cancels the old one cooperatively (the old
//! thread exits without swapping its tree). The finished tree is
//! `Arc<Tree>` behind the state `RwLock`; the OLD tree drops on a
//! background thread so the UI never stalls freeing a million nodes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use diskbytes_core::scan::node::Tree;
use diskbytes_core::scan::scanner::{Progress, ScanOutcome, ScanTarget};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::platform::HostPlatform;
use crate::state::{AppState, ScanHandle};

/// How often the ticker emits `scan-progress` (spec §4: 150 ms).
const TICK_MS: Duration = Duration::from_millis(150);

/// The status response for the Explore idle/scanning/done states.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponse {
    /// Current scan generation.
    pub generation: u64,
    /// Whether a scan is running.
    pub scanning: bool,
    /// Whether a finished tree exists.
    pub has_tree: bool,
    /// Live progress (valid while scanning; last values after).
    pub progress: Progress,
    /// Sticky last-completed-scan record (reconcile path — see
    /// `state::DoneRecord`).
    pub last_done: Option<LastDone>,
}

/// The `get_status` replay of the last completed scan (same shape as
/// the `scan-done` payload).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastDone {
    /// The generation that finished.
    pub generation: u64,
    /// Root stats when the scan succeeded: `(logical, on_disk, files,
    /// folders)`.
    pub stats: Option<(u64, u64, u64, u64)>,
    /// User-readable reason when the scan did not produce a tree.
    pub error: Option<String>,
}

/// The `scan-done` payload.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanDone {
    /// The generation that finished.
    pub generation: u64,
    /// Root stats when the scan succeeded: `(logical, on_disk, files,
    /// folders)`.
    pub stats: Option<(u64, u64, u64, u64)>,
    /// User-readable reason when the scan did not produce a tree.
    pub error: Option<String>,
}

/// Parse a scan target string: `"ThisPC"`, a drive root, or a folder.
#[must_use]
pub fn parse_target(s: &str) -> ScanTarget {
    if s.eq_ignore_ascii_case("thispc") || s.eq_ignore_ascii_case("this pc") {
        return ScanTarget::ThisPc;
    }
    // The separator follows the target's own family ("/" targets stay
    // POSIX; "C:\…" targets stay Windows).
    let trimmed = s.trim_end_matches(['\\', '/']);
    if cfg!(target_os = "macos") {
        ScanTarget::Folder(format!("{trimmed}/"))
    } else {
        ScanTarget::Folder(format!("{trimmed}\\"))
    }
}

/// Start a scan of `target` (`"ThisPC"` or a path). Cancels any running
/// scan. Emits `scan-progress` every 150 ms and `scan-done` at the end.
#[tauri::command]
/// The scan lifecycle in one place (cancellation, swap, events,
/// telemetry) — splitting it across helpers would hide the ordering
/// invariants (generation guard → swap → cache clear → done event).
#[allow(clippy::too_many_lines)]
pub async fn start_scan(
    target: String,
    app: AppHandle,
    state: State<'_, AppState>,
    platform: State<'_, Arc<HostPlatform>>,
) -> Result<u64, String> {
    // Cancel any running scan (cooperative; its thread exits without
    // swapping its tree — spec §4 "starting a new scan cancels the old").
    {
        let scan = state.scan.lock();
        if let Some(old) = scan.as_ref() {
            old.cancel.store(true, Ordering::SeqCst);
        }
    }

    let generation = state.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let scan_target = parse_target(&target);
    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let progress = Arc::clone(&state.progress);
    // Reset the progress snapshot for the new generation.
    *progress.lock() = Progress::default();

    state.mark_scanning(generation);

    let app_handle = app.clone();
    let platform = Arc::clone(&*platform);

    // The scan + swap thread. State is borrowed from the handle INSIDE
    // the closure so everything the thread owns is 'static.
    let cancel_for_thread = Arc::clone(&cancel);
    let engine_label = "standard";
    let join = std::thread::spawn(move || {
        let state_inner = app_handle.state::<AppState>();
        let started = std::time::Instant::now();
        let outcome = diskbytes_core::scan::scanner::scan(
            platform,
            &scan_target,
            generation,
            &cancel_for_thread,
            &progress,
        );
        // Clear ONLY our own generation: when a newer scan superseded
        // us, the flag names ITS generation and must stay set (the old
        // unconditional `store(false)` here killed the successor's
        // ticker and made `get_status` report idle mid-scan).
        state_inner.end_scanning(generation);
        // Engine telemetry (doc 07 §4) — performance watchdog, counts only.
        let an = app_handle.try_state::<crate::analytics::Analytics>();
        let denied = state_inner.progress.lock().denied;
        if let Some(an) = &an {
            if let ScanOutcome::Done(ref tree) = outcome {
                an.capture(
                    "scan_perf",
                    &[
                        ("engine", serde_json::json!(engine_label)),
                        ("files", serde_json::json!(tree.root_stats().2)),
                        ("dirs", serde_json::json!(tree.root_stats().3)),
                        ("bytes", serde_json::json!(tree.root_stats().1)),
                        (
                            "duration_ms",
                            serde_json::json!(started.elapsed().as_millis() as u64),
                        ),
                        (
                            "workers",
                            serde_json::json!(std::thread::available_parallelism()
                                .map_or(0u32, |n| u32::try_from(n.get()).unwrap_or(0))),
                        ),
                        ("denied", serde_json::json!(denied)),
                    ],
                );
            }
        }

        match outcome {
            ScanOutcome::Done(tree) => {
                swap_tree(&state_inner, Arc::new(tree), generation);
                // Layout caches are generation-keyed; drop stale entries on
                // swap (doc 03 M4.1). Same for the regroup / Top Sizes /
                // Age Map caches (spec §7.7/§7.8: cached per generation).
                clear_all_caches(&app_handle);
                let root_stats = {
                    let guard = state_inner.tree.read();
                    guard.as_ref().map(|t| t.root_stats())
                };
                // Sticky outcome BEFORE the emit: a tiny tree can finish
                // before the `start_scan` round-trip resolves, so the
                // `scan-done` event is dropped as stale by the store —
                // `get_status` replays this record for the reconcile.
                *state_inner.last_done.lock() = crate::state::DoneRecord {
                    generation,
                    stats: root_stats,
                    error: None,
                };
                let _ = app_handle.emit(
                    "scan-done",
                    ScanDone {
                        generation,
                        stats: root_stats,
                        error: None,
                    },
                );
            }
            ScanOutcome::Cancelled => {
                // Superseded or user-cancelled: no tree swap, no done
                // event (the NEW scan's events own the UI now).
            }
            ScanOutcome::RootFailed(reason) => {
                *state_inner.last_done.lock() = crate::state::DoneRecord {
                    generation,
                    stats: None,
                    error: Some(reason.clone()),
                };
                let _ = app_handle.emit(
                    "scan-done",
                    ScanDone {
                        generation,
                        stats: None,
                        error: Some(reason),
                    },
                );
            }
        }
    });

    // Register the handle (replacing the cancelled one).
    {
        let mut scan = state.scan.lock();
        *scan = Some(ScanHandle {
            generation,
            cancel: Arc::clone(&cancel),
            join: Some(join),
        });
    }

    // The 150 ms progress ticker (spec §4). Generation-owned: it exits
    // when a NEWER scan's flag replaces ours (its own ticker owns the
    // emissions from there) — the old flagless loop double-emitted or
    // died early depending on which worker's exit path cleared the
    // shared bool first.
    let ticker_app = app.clone();
    std::thread::Builder::new()
        .name("db-ticker".into())
        .spawn(move || {
            let ticker_state = ticker_app.state::<AppState>();
            loop {
                if ticker_state.scanning.load(Ordering::SeqCst) != generation {
                    return;
                }
                let snapshot = ticker_state.progress.lock().clone();
                let gen = ticker_state.current_generation();
                let _ = ticker_app.emit(
                    "scan-progress",
                    ProgressEvent {
                        generation: gen,
                        progress: snapshot,
                    },
                );
                std::thread::sleep(TICK_MS);
            }
        })
        .ok();

    Ok(generation)
}

/// `scan-progress` payload (generation-tagged so stale ticks drop).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    /// The generation this tick belongs to.
    pub generation: u64,
    /// Progress snapshot.
    pub progress: Progress,
}

/// Live status for the Explore states (idle / scanning / done) plus
/// the sticky last-done record (the lost-event reconcile: a scan can
/// complete before the `start_scan` round-trip lands, so the UI polls
/// this right after resolving).
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn get_status(state: State<'_, AppState>) -> StatusResponse {
    let last_done = state.last_done.lock().clone();
    StatusResponse {
        generation: state.current_generation(),
        scanning: state.is_scanning(),
        has_tree: state.tree.read().is_some(),
        progress: state.progress.lock().clone(),
        last_done: Some(LastDone {
            generation: last_done.generation,
            stats: last_done.stats,
            error: last_done.error,
        }),
    }
}

/// Cancel the running scan (user action — spec §4 "starting a new scan
/// cancels the old"; this exposes the SAME cooperative mechanism to the
/// Stop button). The worker thread exits at its next checkpoint
/// without swapping its tree, so the previous tree (if any) stays
/// visible. Returns whether a scan was actually running.
///
/// The `scanning` flag flips immediately so the 150 ms progress ticker
/// stops at once; the frontend reverts to its saved tree-generation
/// optimistically and reconciles through `get_status` (a scan that
/// completed in the cancel window still emits its `scan-done`).
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn cancel_scan(state: State<'_, AppState>) -> bool {
    let running = {
        let scan = state.scan.lock();
        match scan.as_ref() {
            Some(handle) => {
                handle.cancel.store(true, Ordering::SeqCst);
                // Clear the running flag ONLY when it still names this
                // handle's generation (a newer scan may already own it).
                state.end_scanning(handle.generation);
                true
            }
            None => false,
        }
    };
    running
}

/// Swap the finished tree in, dropping the old `Arc<Tree>` on a
/// background thread (spec §4 — never stall the UI on a million-node
/// drop).
fn swap_tree(state: &AppState, new: Arc<Tree>, generation: u64) {
    let old = {
        let mut guard = state.tree.write();
        // Only swap when we are still the current generation.
        if generation != state.current_generation() {
            return;
        }
        guard.replace(new)
    };
    if let Some(old) = old {
        std::thread::spawn(move || drop(old));
    }
}

/// Dev hooks (spec §15): `DISKBYTES_SCAN` / `--scan` auto-start targets,
/// `DISKBYTES_MODE` visualization, `--turbo`, `DISKBYTES_VERIFY`.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DevHooks {
    /// Auto-scan target from `DISKBYTES_SCAN` / `--scan <path>`.
    pub scan: Option<String>,
    /// Visualization from `DISKBYTES_MODE`.
    pub mode: Option<String>,
    /// `--turbo` requested.
    pub turbo: bool,
    /// `DISKBYTES_VERIFY=1` two-engine comparison.
    pub verify: bool,
    /// `DISKBYTES_TOUR=1` auto-cycles tabs/modes/overlays so CI
    /// screenshot passes can capture every state of the real app
    /// without interactive automation.
    pub tour: bool,
}

/// Read the §15 dev hooks once (flags/env; empty when not set).
#[must_use]
pub fn read_dev_hooks() -> DevHooks {
    let args: Vec<String> = std::env::args().collect();
    let mut hooks = DevHooks::default();
    if let Ok(v) = std::env::var("DISKBYTES_SCAN") {
        if !v.is_empty() {
            hooks.scan = Some(v);
        }
    }
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--scan" {
            if let Some(path) = it.next() {
                hooks.scan = Some(path.clone());
            }
        } else if a == "--turbo" {
            hooks.turbo = true;
        }
    }
    if let Ok(v) = std::env::var("DISKBYTES_MODE") {
        if !v.is_empty() {
            hooks.mode = Some(v);
        }
    }
    if let Ok(v) = std::env::var("DISKBYTES_VERIFY") {
        hooks.verify = v == "1";
    }
    if let Ok(v) = std::env::var("DISKBYTES_TOUR") {
        hooks.tour = v == "1";
    }
    hooks
}

/// `get_dev_hooks` command (spec §15).
#[tauri::command]
pub fn get_dev_hooks() -> DevHooks {
    read_dev_hooks()
}

/// The turbo scan outcome summary (scan-done + turbo report).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurboReport {
    pub generation: u64,
    /// Records parsed.
    pub records: u64,
    /// Warnings (torn/bad/orphans/unreferenced/cycles).
    pub torn: u64,
    pub bad: u64,
    pub unreferenced: u64,
    /// Turbo wall time in ms.
    pub ms: u64,
}

/// Start a TURBO scan (spec §5; doc 03 M7) of `target` (a drive root).
/// Requires elevation; the R2 fallback contract: any failure returns a
/// user-readable reason and the caller falls back to the STANDARD
/// engine, user-visible, never silent.
///
/// # Errors
/// - `"ELEVATION_REQUIRED"` when not elevated (JS shows the shield).
/// - A reason string for every other failure (fallback trigger).
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
#[allow(clippy::too_many_lines)] // scan lifecycle: elevation gate + thread spawn
pub async fn start_scan_turbo(
    target: String,
    app: AppHandle,
    state: State<'_, AppState>,
    platform: State<'_, std::sync::Arc<HostPlatform>>,
) -> Result<u64, String> {
    if !crate::platform::os::is_elevated() {
        return Err("ELEVATION_REQUIRED".into());
    }
    let drive_root = if cfg!(target_os = "macos") {
        format!("{}/", target.trim_end_matches(['\\', '/']))
    } else {
        format!("{}\\", target.trim_end_matches('\\'))
    };
    let label = drive_root.clone();
    let generation = state.generation.fetch_add(1, Ordering::SeqCst) + 1;

    // Cancel any standard scan in flight (same contract as start_scan).
    {
        let scan = state.scan.lock();
        if let Some(old) = scan.as_ref() {
            old.cancel.store(true, Ordering::SeqCst);
        }
    }
    // Turbo scans register a ScanHandle too: without one, cancel_scan
    // flipped a flag on a STALE handle while the MFT read ran
    // uncancelled — the Stop button was a no-op during turbo scans.
    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    state.mark_scanning(generation);
    *state.progress.lock() = Progress::default();

    let app_handle = app.clone();
    let platform = std::sync::Arc::clone(&*platform);
    let cancel_for_thread = Arc::clone(&cancel);
    let join = std::thread::Builder::new()
        .name("db-scan-turbo".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            let state_inner = app_handle.state::<AppState>();
            let mut reason: Option<String> = None;

            let mut tree_opt: Option<diskbytes_core::scan::node::Tree> = None;
            let mut report: Option<TurboReport> = None;

            if !crate::platform::os::enable_backup_privilege() {
                reason = Some("The backup privilege is unavailable on this process.".into());
            }
            if reason.is_none() {
                // The MFT read itself honours the cancel flag (a user Stop
                // during the raw read should not keep the disk busy).
                if cancel_for_thread.load(Ordering::SeqCst) {
                    reason = Some("cancelled".into());
                }
            }
            if reason.is_none() {
                match crate::platform::os::turbo_geometry(&drive_root) {
                    Ok((mut volume, geo)) => {
                        match crate::platform::os::turbo_read_mft(&mut volume, &geo) {
                            Ok(mft) => {
                                let core_geo = diskbytes_core::turbo::Geometry {
                                    bytes_per_sector: geo.bytes_per_sector,
                                    bytes_per_cluster: geo.bytes_per_cluster,
                                    bytes_per_record: geo.bytes_per_record,
                                    mft_valid_data_length: geo.mft_valid_data_length,
                                };
                                let (entries, warnings) =
                                    diskbytes_core::turbo::parse_all(&mft, &core_geo);
                                let records = entries.len() as u64;
                                let mut build =
                                    diskbytes_core::turbo::tree::build_tree(entries, &label);
                                diskbytes_core::scan::rollup::finalize(&mut build.tree);
                                build.tree.generation = generation;
                                tree_opt = Some(build.tree);
                                report = Some(TurboReport {
                                    generation,
                                    records,
                                    torn: warnings.torn,
                                    bad: warnings.bad,
                                    unreferenced: warnings.unreferenced,
                                    ms: started.elapsed().as_millis() as u64,
                                });
                            }
                            Err(e) => reason = Some(e),
                        }
                    }
                    Err(e) => reason = Some(e),
                }
            }

            match (tree_opt, reason) {
                (Some(tree), None) => {
                    state_inner.end_scanning(generation);
                    let root_stats = tree.root_stats();
                    swap_tree(&state_inner, std::sync::Arc::new(tree), generation);
                    clear_all_caches(&app_handle);
                    *state_inner.last_done.lock() = crate::state::DoneRecord {
                        generation,
                        stats: Some(root_stats),
                        error: None,
                    };
                    let _ = app_handle.emit(
                        "scan-done",
                        ScanDone {
                            generation,
                            stats: Some(root_stats),
                            error: None,
                        },
                    );
                    if let Some(r) = report {
                        let _ = app_handle.emit("turbo-report", r);
                    }
                }
                (_, Some(error)) => {
                    // R2: the ONE allowed fallback — user-visible with reason.
                    let _ = app_handle.emit("turbo-fallback", &error);
                    // Fall back to the standard engine on this thread,
                    // stating the reason in the event above. The scanning
                    // flag STAYS on our generation through the whole
                    // fallback (the old code cleared it first — the entire
                    // fallback ran with `scanning == false`: dead ticker,
                    // `get_status` lying) and the REGISTERED cancel flag
                    // flows in (the old code minted a fresh never-cancelled
                    // flag, so Stop could not cancel a fallback either).
                    let outcome = diskbytes_core::scan::scanner::scan(
                        platform,
                        &parse_target(&drive_root),
                        generation,
                        &cancel_for_thread,
                        &state_inner.progress,
                    );
                    state_inner.end_scanning(generation);
                    match outcome {
                        ScanOutcome::Done(tree) => {
                            let root_stats = tree.root_stats();
                            swap_tree(&state_inner, std::sync::Arc::new(tree), generation);
                            clear_all_caches(&app_handle);
                            *state_inner.last_done.lock() = crate::state::DoneRecord {
                                generation,
                                stats: Some(root_stats),
                                error: None,
                            };
                            let _ = app_handle.emit(
                                "scan-done",
                                ScanDone {
                                    generation,
                                    stats: Some(root_stats),
                                    error: None,
                                },
                            );
                        }
                        ScanOutcome::RootFailed(r) => {
                            *state_inner.last_done.lock() = crate::state::DoneRecord {
                                generation,
                                stats: None,
                                error: Some(r.clone()),
                            };
                            let _ = app_handle.emit(
                                "scan-done",
                                ScanDone {
                                    generation,
                                    stats: None,
                                    error: Some(r),
                                },
                            );
                        }
                        ScanOutcome::Cancelled => {}
                    }
                }
                (None, None) => {
                    state_inner.end_scanning(generation);
                    let reason = "The turbo engine produced no tree.".to_string();
                    *state_inner.last_done.lock() = crate::state::DoneRecord {
                        generation,
                        stats: None,
                        error: Some(reason.clone()),
                    };
                    let _ = app_handle.emit(
                        "scan-done",
                        ScanDone {
                            generation,
                            stats: None,
                            error: Some(reason),
                        },
                    );
                }
            }
        })
        .ok();
    // Register the turbo handle so cancel_scan reaches the MFT read and
    // the fallback (see the ScanHandle note above).
    {
        let mut scan = state.scan.lock();
        *scan = Some(ScanHandle {
            generation,
            cancel,
            join,
        });
    }
    Ok(generation)
}

/// Clear every generation-keyed cache (shared by the scan swap paths).
fn clear_all_caches(app: &AppHandle) {
    crate::commands::layout::clear_cache(&app.state::<crate::commands::layout::Cache>());
    crate::commands::layout::clear_regroup_cache(
        &app.state::<crate::commands::layout::RegroupCache>(),
    );
    crate::commands::explore::TopCache::clear(&app.state::<crate::commands::explore::TopCache>());
    crate::commands::explore::AgeCache::clear(&app.state::<crate::commands::explore::AgeCache>());
    crate::commands::sidebar::QuickWinsCache::clear_pub(
        &app.state::<crate::commands::sidebar::QuickWinsCache>(),
    );
}
