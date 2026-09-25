//! Sidebar commands (spec §6; doc 03 M6): drives, home, disk storage,
//! elevation state, elevated relaunch, Quick Wins (cached per
//! generation), Quick Wins item paths, and the File Types bar data.
//!
//! Quick Wins categories are computed from the ALREADY-BUILT tree (no
//! extra disk pass — spec §6.7) via `core::quickwins::resolve` with the
//! known-folder env roots resolved through the platform seam.

use std::collections::HashMap;
use std::sync::Arc;

use diskbytes_core::platform::Platform;
use diskbytes_core::quickwins;
use diskbytes_core::scan::categories::FileCategory;
use diskbytes_core::scan::node::Tree;
use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, State};

use crate::platform::HostPlatform;
use crate::state::AppState;

/// One drive chip (spec §6.3).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveChip {
    /// Display letter, e.g. `C:`.
    pub letter: String,
    /// Scan target for this drive (`C:\`).
    pub target: String,
}

/// Fixed-drive chips (spec §6.3 — one per fixed drive).
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn get_drive_chips(platform: State<'_, Arc<HostPlatform>>) -> Vec<DriveChip> {
    (*platform)
        .fixed_drive_roots()
        .iter()
        .filter_map(|root| {
            // `\\?\C:\` → letter `C:`.
            let trimmed = root.trim_start_matches(r"\\?\");
            let bytes = trimmed.as_bytes();
            if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
                Some(DriveChip {
                    letter: format!("{}:", bytes[0] as char),
                    target: format!("{}:\\", bytes[0] as char),
                })
            } else {
                None
            }
        })
        .collect()
}

/// The user profile path (the Home button — spec §6.2).
///
/// # Errors
/// String error when the known folder cannot be resolved.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn get_home_path(platform: State<'_, Arc<HostPlatform>>) -> Result<String, String> {
    (*platform)
        .known_folder(diskbytes_core::platform::KnownFolder::Profile)
        .ok_or_else(|| "Couldn't resolve your user profile folder.".into())
}

/// Resolve a display path to a node id in the CURRENT tree, so the Home
/// button can NAVIGATE when the path is inside the last scan (no
/// rescan) and only start a new scan when it isn't. Returns `None`
/// (not an error) when there is no tree, the generation is stale, or
/// the path is outside the scan — the caller decides what to do.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn resolve_path(generation: u64, path: String, state: State<'_, AppState>) -> Option<u32> {
    let guard = state.tree.read();
    let tree = guard.as_ref()?;
    if tree.generation != generation {
        return None;
    }
    tree.resolve_display_path(&path)
}

/// The disk storage snapshot (spec §6.5) for the volume containing the
/// current scan root (system drive when there is no scan).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageInfo {
    pub label: String,
    pub total: u64,
    pub used: u64,
    pub free: u64,
    /// Fraction used (0..1).
    pub used_pct: f64,
}

/// Read the storage snapshot.
///
/// # Errors
/// String error when the volume cannot be queried.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn disk_storage(state: State<'_, AppState>) -> Result<StorageInfo, String> {
    // Prefer the current scan root's volume; fall back to the system
    // drive (spec §6.5).
    let path: Option<String> = {
        let guard = state.tree.read();
        guard
            .as_ref()
            .and_then(|t| t.roots.first().map(|r| r.path.clone()))
    };
    let probe = path.unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            "/".to_string()
        } else {
            std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into())
        }
    });
    let snap = crate::platform::os::disk_storage(&probe)
        .ok_or_else(|| "Couldn't read this volume's free space.".to_string())?;
    let label = String::from_utf16_lossy(&snap.label.0)
        .trim_end_matches('\0')
        .trim()
        .to_string();
    Ok(StorageInfo {
        label,
        total: snap.total,
        used: snap.used,
        free: snap.free,
        used_pct: if snap.total > 0 {
            snap.used as f64 / snap.total as f64
        } else {
            0.0
        },
    })
}

/// True when running elevated (hides the restart-as-admin button).
#[tauri::command]
pub fn is_elevated() -> bool {
    crate::platform::os::is_elevated()
}

/// Restart as administrator and re-run the same scan (spec §6.4/§7):
/// ShellExecuteW "runas" with `--scan <target>`, then exit this
/// instance so only the elevated window remains.
///
/// # Errors
/// String error when elevation is declined or the launch fails.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // AppHandle is the tauri command contract
pub fn restart_as_admin(scan_target: &str, turbo: Option<bool>, app: AppHandle) {
    // The elevated instance takes over; this one exits (restart never
    // returns). Failures surface as a user-readable error first.
    let run = |args: String| crate::platform::os::relaunch_elevated_with(scan_target, &args);
    let result = if turbo.unwrap_or(false) {
        run("--turbo".into())
    } else {
        run(String::new())
    };
    match result {
        // Exit cleanly — do NOT `tauri::process::restart`, which would
        // relaunch a second, still-unelevated copy alongside the new one.
        Ok(()) => app.exit(0),
        Err(reason) => {
            let _ = tauri::Emitter::emit(&app, "admin-restart-failed", reason);
        }
    }
}

/// One Quick Wins row (spec §6.7).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuickWinRow {
    pub id: String,
    pub title: String,
    pub icon: String,
    pub count: u64,
    pub size: u64,
    /// Review-only rows refuse "Add all" (VM disks, Windows.old).
    pub review_only: bool,
    /// Context line (e.g. the ms-settings link hint for Windows.old).
    pub extra: Option<String>,
    /// The biggest match (row click navigates there — spec §6.7).
    pub biggest_match: Option<u32>,
}

/// Quick Wins for the current tree (cached per generation; spec §6.7:
/// computed from the already-built tree, no extra disk pass).
///
/// # Errors
/// String error when no scan exists yet or the generation is stale.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub async fn quick_wins(
    generation: u64,
    state: State<'_, AppState>,
    platform: State<'_, Arc<HostPlatform>>,
    cache: State<'_, QuickWinsCache>,
) -> Result<Vec<QuickWinRow>, String> {
    let tree = {
        let guard = state.tree.read();
        let Some(tree) = guard.as_ref() else {
            return Err("no scan yet".into());
        };
        if tree.generation != generation {
            return Err(format!(
                "stale generation {} (current {})",
                generation, tree.generation
            ));
        }
        Arc::clone(tree)
    };
    if let Some(hit) = cache.done.lock().get(&generation) {
        return Ok(Arc::clone(hit).as_ref().clone());
    }
    let env_roots = env_roots(**platform);
    let rows = tauri::async_runtime::spawn_blocking(move || {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        compute_quick_wins(&tree, &env_roots, now)
    })
    .await
    .map_err(|e| format!("quick-wins thread failed: {e}"))?;
    cache.done.lock().insert(generation, Arc::new(rows.clone()));
    Ok(rows)
}

/// Quick Wins cache (per generation — cleared implicitly by keying).
#[derive(Default)]
pub struct QuickWinsCache {
    done: Mutex<HashMap<u64, Arc<Vec<QuickWinRow>>>>,
}

/// Managed state constructor.
#[must_use]
pub fn quick_wins_cache() -> QuickWinsCache {
    QuickWinsCache::default()
}

impl QuickWinsCache {
    /// Clear (scan-swap / surgery path; called cross-module).
    pub fn clear_pub(&self) {
        self.done.lock().clear();
    }
}

/// The pure computation (host-testable shape: env roots injected).
fn compute_quick_wins(
    tree: &Tree,
    env_roots: &HashMap<String, String>,
    now: i64,
) -> Vec<QuickWinRow> {
    quickwins::resolve(tree, env_roots, now)
        .into_iter()
        .map(|c| QuickWinRow {
            id: c.id.to_string(),
            title: c.title.to_string(),
            icon: c.icon.to_string(),
            count: c.items.len() as u64,
            size: c.size,
            review_only: c.review_only,
            extra: c.extra.map(std::string::ToString::to_string),
            biggest_match: c.items.first().copied(),
        })
        .collect()
}

/// Known-folder env roots for the matcher (spec §6 pattern table).
fn env_roots(platform: HostPlatform) -> HashMap<String, String> {
    let mut m: HashMap<String, String> = HashMap::new();
    let pairs = [
        (
            "%USERPROFILE%",
            diskbytes_core::platform::KnownFolder::Profile,
        ),
        (
            "%LOCALAPPDATA%",
            diskbytes_core::platform::KnownFolder::LocalAppData,
        ),
        (
            "%APPDATA%",
            diskbytes_core::platform::KnownFolder::RoamingAppData,
        ),
        (
            "%PROGRAMDATA%",
            diskbytes_core::platform::KnownFolder::ProgramData,
        ),
    ];
    for (key, folder) in pairs {
        if let Some(path) = platform.known_folder(folder) {
            m.insert(key.to_string(), path);
        }
    }
    if cfg!(target_os = "macos") {
        // Mac aliases for the Mac BuildPrompt §5 pattern table.
        if let Some(home) = platform.known_folder(diskbytes_core::platform::KnownFolder::Profile) {
            let support = format!("{home}/Library/Application Support");
            m.insert("%HOME%".into(), home);
            m.insert("%APP_SUPPORT%".into(), support);
        }
    }
    m
}

/// One staged-able Quick Wins item (for "Add all N to Cleanup").
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuickWinItem {
    pub id: u32,
    pub path: String,
    pub size: u64,
}

/// The item list for a Quick Wins category (Add-all staging + Show in
/// Explorer targets).
///
/// # Errors
/// String error when no scan exists or the generation is stale.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn quick_win_items(
    generation: u64,
    category_id: String,
    state: State<'_, AppState>,
    platform: State<'_, Arc<HostPlatform>>,
) -> Result<Vec<QuickWinItem>, String> {
    let guard = state.tree.read();
    let Some(tree) = guard.as_ref() else {
        return Err("no scan yet".into());
    };
    if tree.generation != generation {
        return Err(format!(
            "stale generation {} (current {})",
            generation, tree.generation
        ));
    }
    let env_roots = env_roots(**platform);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let cats = quickwins::resolve(tree, &env_roots, now);
    let Some(cat) = cats.into_iter().find(|c| c.id == category_id) else {
        return Err(format!("unknown Quick Wins category {category_id}"));
    };
    Ok(cat
        .items
        .into_iter()
        .take(quickwins::CATEGORY_CAP)
        .map(|id| QuickWinItem {
            id,
            path: tree.node_path(id),
            size: tree.node(id).map_or(0, |n| n.on_disk),
        })
        .collect())
}

/// One File Types bar segment (spec §6.8).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeSegment {
    pub label: String,
    pub color: u32,
    pub size: u64,
}

/// The File Types stacked-bar data for the scan root.
///
/// # Errors
/// String error when no scan exists or the generation is stale.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn file_types(generation: u64, state: State<'_, AppState>) -> Result<Vec<TypeSegment>, String> {
    let guard = state.tree.read();
    let Some(tree) = guard.as_ref() else {
        return Err("no scan yet".into());
    };
    if tree.generation != generation {
        return Err(format!(
            "stale generation {} (current {})",
            generation, tree.generation
        ));
    }
    let root = tree.root;
    let Some(n) = tree.node(root) else {
        return Ok(Vec::new());
    };
    let sizes: [u64; 9] = tree
        .dir_extras
        .get(n.dir_index as usize)
        .map_or([0; 9], |e| e.type_sizes);
    let mut out: Vec<TypeSegment> = (0..9u8)
        .filter_map(|bits| {
            let cat = FileCategory::from_bits(bits);
            let size = sizes[bits as usize];
            (size > 0).then(|| TypeSegment {
                label: cat.label().to_string(),
                color: cat.color(),
                size,
            })
        })
        .collect();
    out.sort_unstable_by_key(|s| std::cmp::Reverse(s.size));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_win_row_shape() {
        let row = QuickWinRow {
            id: "downloads".into(),
            title: "Downloads".into(),
            icon: "downloads".into(),
            count: 3,
            size: 120,
            review_only: false,
            extra: None,
            biggest_match: Some(7),
        };
        assert_eq!(row.id, "downloads");
        assert!(!row.review_only);
    }

    #[test]
    fn type_segments_sorted_desc() {
        #[rustfmt::skip]
        let mut segs = [
            TypeSegment { label: "A".into(), color: 1, size: 10 },
            TypeSegment { label: "B".into(), color: 2, size: 30 },
            TypeSegment { label: "C".into(), color: 3, size: 20 },
        ];
        segs.sort_unstable_by_key(|s| std::cmp::Reverse(s.size));
        assert_eq!(segs[0].label, "B");
    }
}
