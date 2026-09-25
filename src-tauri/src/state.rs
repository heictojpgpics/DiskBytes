//! App state (spec §4): the finished tree is `Arc<Tree>` behind an
//! `RwLock`; IPC commands take a read lock. Scan replacement drops the
//! old `Arc` on a background thread so the UI never stalls freeing a
//! million nodes. Every tree request carries the scan **generation**.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use diskbytes_core::scan::node::Tree;
use diskbytes_core::scan::scanner::Progress;
use parking_lot::{Mutex, RwLock};

/// The live progress sink the scanner updates once per directory batch
/// (spec §4); the 150 ms ticker and `get_status` read it.
pub type ProgressSink = Arc<Mutex<Progress>>;

/// Control over the one running scan (starting a new scan cancels the
/// old — spec §4).
pub struct ScanHandle {
    /// The generation this scan writes (kept for `get_status` /
    /// future cancel-status reporting).
    #[allow(dead_code)]
    pub generation: u64,
    /// Cancel flag shared with the scanner workers.
    pub cancel: Arc<AtomicBool>,
    /// The scan thread (joins itself into the state swap; never joined
    /// from the command layer — cancellation is cooperative).
    #[allow(dead_code)]
    pub join: Option<std::thread::JoinHandle<()>>,
}

/// The sticky outcome of the last COMPLETED scan (not cancellations):
/// the UI reconciles against it because a `scan-done` event can fire
/// before the `start_scan` round-trip resolves (tiny trees finish in
/// milliseconds — the event lands while the store still holds the old
/// generation and gets dropped as stale). `get_status` replays it.
#[derive(Debug, Clone, Default)]
pub struct DoneRecord {
    /// The generation that finished.
    pub generation: u64,
    /// Final stats (logical, onDisk, files, folders).
    pub stats: Option<(u64, u64, u64, u64)>,
    /// Terminal error, if the scan failed.
    pub error: Option<String>,
}

/// Shared application state managed by Tauri.
pub struct AppState {
    /// The finished tree (`None` before the first scan completes).
    pub tree: RwLock<Option<Arc<Tree>>>,
    /// Monotonic scan generation (IPC staleness contract).
    pub generation: AtomicU64,
    /// The running scan, if any.
    pub scan: Mutex<Option<ScanHandle>>,
    /// Generation-owned scanning flag: `0` = idle, otherwise the
    /// generation of the running scan. A bare `AtomicBool` could not
    /// distinguish "the scan that just exited" from "the newer scan
    /// that superseded it": a superseded worker storing `false` killed
    /// the NEW scan's progress ticker and made `get_status` lie for the
    /// whole scan. Workers clear ONLY their own generation
    /// (compare_exchange), so a superseded exit can never clobber a
    /// successor's running state.
    pub scanning: AtomicU64,
    /// The live progress snapshot (valid while `scanning != 0`).
    pub progress: ProgressSink,
    /// The last completed scan's outcome (see `DoneRecord` — the
    /// lost-event reconcile path; written by the scan thread before
    /// the `scan-done` emit, read by `get_status`).
    pub last_done: Mutex<DoneRecord>,
}

impl AppState {
    /// Fresh state at generation 1 (the first scan takes it).
    #[must_use]
    pub fn new() -> Self {
        Self {
            tree: RwLock::new(None),
            generation: AtomicU64::new(1),
            scan: Mutex::new(None),
            scanning: AtomicU64::new(0),
            progress: Arc::new(Mutex::new(Progress::default())),
            last_done: Mutex::new(DoneRecord::default()),
        }
    }

    /// The current generation for request tagging.
    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Mark `generation` as the running scan (idempotent for the same
    /// generation; never overwrites a different running generation —
    /// callers only mark a generation they freshly allocated).
    pub fn mark_scanning(&self, generation: u64) {
        self.scanning.store(generation, Ordering::SeqCst);
    }

    /// True while a scan is running.
    pub fn is_scanning(&self) -> bool {
        self.scanning.load(Ordering::SeqCst) != 0
    }

    /// Clear the running flag ONLY when it still names `generation`
    /// (a superseded worker's exit must not clobber a successor).
    pub fn end_scanning(&self, generation: u64) {
        self.scanning
            .compare_exchange(generation, 0, Ordering::SeqCst, Ordering::SeqCst)
            .ok();
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
