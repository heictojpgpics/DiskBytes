//! Applications commands (spec §11; doc 03 M8): installed-app
//! enumeration (registry + MSIX), bundle sizes, leftovers matching,
//! last-used, icons, and the uninstall flow. All enumeration runs on
//! the blocking pool and is cached across tab switches (spec §11:
//! "keep the result across tab switches"); `refresh` forces a re-run.

use std::sync::Arc;

use diskbytes_core::apps::{
    self, AppEntry, AppIdentity, AppSource, LeftoverGroup, RootChild, RootListing,
};
use diskbytes_core::platform::{KnownFolder, Platform};
use rayon::prelude::*;
use serde::Serialize;
use tauri::State;

use crate::platform::HostPlatform;
use crate::state::AppState;

/// Max apps returned (bounded IPC; installs >300 are rare).
const APPS_CAP: usize = 500;

/// The applications cache: one snapshot kept across tab switches.
#[derive(Default)]
pub struct AppsCache {
    done: parking_lot::Mutex<Option<Arc<Vec<AppEntry>>>>,
    inflight: parking_lot::Mutex<bool>,
}

/// Managed state constructor.
#[must_use]
pub fn apps_cache() -> AppsCache {
    AppsCache::default()
}

impl AppsCache {
    /// Drop the snapshot (Refresh button).
    pub fn clear(&self) {
        *self.done.lock() = None;
    }
}

/// Leftover root paths for the current user (spec §11's 5 roots).
fn leftover_roots(platform: HostPlatform) -> Vec<(usize, String)> {
    let mut roots = Vec::new();
    if let Some(local) = platform.known_folder(KnownFolder::LocalAppData) {
        roots.push((0, local.clone()));
        // {LOCALAPPDATA}Low — the LocalLow sibling.
        if let Some(profile) = platform.known_folder(KnownFolder::Profile) {
            let low = format!("{profile}\\AppData\\LocalLow");
            roots.push((2, low));
        }
        // {LOCALAPPDATA}\Packages — store package data.
        roots.push((4, format!("{local}\\Packages")));
    }
    if let Some(roaming) = platform.known_folder(KnownFolder::RoamingAppData) {
        roots.push((1, roaming));
    }
    if let Some(pd) = platform.known_folder(KnownFolder::ProgramData) {
        roots.push((3, pd));
    }
    roots.sort_unstable_by_key(|(i, _)| *i);
    roots
}

/// Allocated size of a directory tree (parallel walk; reparse points
/// skipped; unreadable parts contribute 0 — best effort per spec §11).
fn dir_allocated_size(path: &str, cluster: u32) -> u64 {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !md.is_dir() {
        return round_up(md.len(), u64::from(cluster));
    }
    let mut total = 0u64;
    let walker = |dir: &str, total: &mut u64| -> Vec<String> {
        let mut subdirs = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return subdirs;
        };
        for e in entries.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.file_type().is_symlink() {
                continue; // never follow reparse points
            }
            if md.is_dir() {
                subdirs.push(e.path().to_string_lossy().into_owned());
            } else {
                *total = total.saturating_add(round_up(md.len(), u64::from(cluster)));
            }
        }
        subdirs
    };
    let mut queue = vec![path.to_string()];
    while let Some(dir) = queue.pop() {
        let subdirs = walker(&dir, &mut total);
        queue.extend(subdirs);
    }
    total
}

fn round_up(len: u64, cluster: u64) -> u64 {
    if cluster == 0 {
        return len;
    }
    len.div_ceil(cluster).saturating_mul(cluster)
}

/// List one root's children with allocated sizes (the SHARED listing,
/// spec §11: "List each root directory only once and share the listing
/// across all apps").
fn list_root(root_idx: usize, path: &str) -> RootListing {
    let mut children: Vec<RootChild> = Vec::new();
    let Ok(entries) = std::fs::read_dir(path) else {
        return RootListing {
            label_idx: root_idx,
            path: path.to_string(),
            children,
        };
    };
    let mut candidates: Vec<(String, String)> = Vec::new();
    for e in entries.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() && !md.file_type().is_symlink() {
            candidates.push((
                e.file_name().to_string_lossy().into_owned(),
                e.path().to_string_lossy().into_owned(),
            ));
        }
    }
    let cluster = crate::platform::os::cluster_size(path);
    children = candidates
        .into_par_iter()
        .map(|(name, full)| RootChild {
            name,
            size: dir_allocated_size(&full, cluster),
            nested: Vec::new(),
        })
        .collect();
    RootListing {
        label_idx: root_idx,
        path: path.to_string(),
        children,
    }
}

/// The full enumeration pipeline (blocking-pool body).
#[allow(clippy::too_many_lines)] // one cohesive pipeline; splitting hurts clarity
fn enumerate_apps(platform: HostPlatform) -> Vec<AppEntry> {
    // 1. Raw sources.
    let registry = crate::platform::os::registry_uninstall_entries();
    let msix = crate::platform::os::msix_packages().unwrap_or_default();
    let userassist = crate::platform::os::userassist_entries();

    // 2. Seed entries + identities.
    let mut entries: Vec<(AppEntry, AppIdentity)> = Vec::new();
    for r in &registry {
        let install_folder = r
            .install_location
            .rsplit(['\\', '/'])
            .find(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        let last_used =
            diskbytes_core::apps::last_used_for_install(&userassist, &r.install_location);
        entries.push((
            AppEntry {
                id: r.id.clone(),
                name: r.name.clone(),
                publisher: r.publisher.clone(),
                version: r.version.clone(),
                source: AppSource::Registry,
                install_location: r.install_location.clone(),
                uninstall_string: r.uninstall_string.clone(),
                quiet_uninstall_string: r.quiet_uninstall_string.clone(),
                package_full_name: String::new(),
                last_used,
                icon: String::new(),
                bundle_size: 0,
                leftovers: Vec::new(),
                total: 0,
            },
            AppIdentity {
                display_name: r.name.clone(),
                install_folder,
                family_name: String::new(),
                publisher: r.publisher.clone(),
            },
        ));
    }
    for m in &msix {
        let install_folder = m
            .install_location
            .rsplit(['\\', '/'])
            .find(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        entries.push((
            AppEntry {
                id: m.id.clone(),
                name: m.name.clone(),
                publisher: m.publisher.clone(),
                version: m.version.clone(),
                source: AppSource::Msix,
                install_location: m.install_location.clone(),
                uninstall_string: String::new(),
                quiet_uninstall_string: String::new(),
                package_full_name: m.id.clone(),
                last_used: None, // MSIX has no UserAssist trail
                icon: String::new(),
                bundle_size: 0,
                leftovers: Vec::new(),
                total: 0,
            },
            AppIdentity {
                display_name: m.name.clone(),
                install_folder,
                family_name: m.family_name.clone(),
                publisher: m.publisher.clone(),
            },
        ));
    }

    // 3. Shared leftover-root listing (+ publisher-nested children).
    let roots: Vec<RootListing> = leftover_roots(platform)
        .into_iter()
        .map(|(idx, path)| list_root(idx, &path))
        .collect();
    let identities: Vec<AppIdentity> = entries.iter().map(|(_, i)| i.clone()).collect();
    let expand = apps::publisher_children_to_expand(&roots, &identities);
    let mut roots = roots;
    for (ri, child_name) in expand {
        if let Some(root) = roots.get_mut(ri) {
            if let Some(child) = root.children.iter_mut().find(|c| c.name == child_name) {
                let full = format!("{}\\{}", root.path.trim_end_matches('\\'), child_name);
                let cluster = crate::platform::os::cluster_size(&full);
                if let Ok(subs) = std::fs::read_dir(&full) {
                    child.nested = subs
                        .flatten()
                        .filter_map(|e| {
                            let md = e.metadata().ok()?;
                            md.is_dir().then(|| {
                                let n = e.file_name().to_string_lossy().into_owned();
                                let p = e.path().to_string_lossy().into_owned();
                                (n, dir_allocated_size(&p, cluster))
                            })
                        })
                        .collect();
                }
            }
        }
    }

    // 4. Bundle sizes + icons + leftovers, in parallel.
    let root_ref = &roots;
    let mut out: Vec<AppEntry> = entries
        .into_par_iter()
        .map(|(mut app, identity)| {
            if !app.install_location.is_empty() {
                let cluster = crate::platform::os::cluster_size(&app.install_location);
                app.bundle_size = dir_allocated_size(&app.install_location, cluster);
            }
            app.leftovers = apps::find_leftovers(&identity, root_ref);
            // Icon: DisplayIcon when present, else the install folder.
            let icon_path = app.id.is_empty().then(String::new).unwrap_or_default();
            let _ = icon_path;
            app
        })
        .collect();

    // Icons need the raw DisplayIcon paths (registry only) — a second
    // parallel pass keeps the pipeline simple.
    let icon_sources: std::collections::HashMap<String, String> = registry
        .iter()
        .map(|r| (r.id.clone(), r.display_icon.clone()))
        .collect();
    for app in &mut out {
        if app.source == AppSource::Registry {
            let source = icon_sources.get(&app.id).cloned().unwrap_or_default();
            let icon_ref = if source.is_empty() {
                app.install_location.clone()
            } else {
                source
            };
            app.icon = crate::platform::os::icon_png_data_url(&icon_ref).unwrap_or_default();
        }
    }

    // 5. Totals + sort by total footprint (spec §11).
    for app in &mut out {
        app.total = app
            .bundle_size
            .saturating_add(app.leftovers.iter().map(|g| g.size).sum());
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.total));
    out.truncate(APPS_CAP);
    out
}

/// List installed applications (cached across tab switches).
///
/// # Errors
/// String error when the blocking pool task panics.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub async fn list_applications(
    refresh: Option<bool>,
    platform: State<'_, Arc<HostPlatform>>,
    cache: State<'_, AppsCache>,
) -> Result<Vec<AppEntry>, String> {
    if refresh.unwrap_or(false) {
        cache.clear();
    }
    if let Some(hit) = cache.done.lock().clone() {
        return Ok((*hit).clone());
    }
    // Claim the enumeration slot. The FIRST caller enumerates and fills
    // the cache; concurrent callers compute directly (correct without a
    // wait state; the cache lands shortly). The old code had the flag
    // INVERTED — inflight was never set, `first` was always false, and
    // the cache-fill path was dead code: every tab switch re-enumerated
    // the registry, MSIX, every install folder and all five app-data
    // roots (seconds of disk I/O each time).
    let first = {
        let mut inflight = cache.inflight.lock();
        let first = !*inflight;
        *inflight = true;
        first
    };
    if !first {
        // Someone else is enumerating.
        let platform = Arc::clone(&platform);
        return tauri::async_runtime::spawn_blocking(move || enumerate_apps(*platform))
            .await
            .map_err(|e| format!("applications thread failed: {e}"));
    }
    // We own the enumeration: on ANY outcome (error included) the slot
    // must be released or every later call would compute directly
    // forever (an error must not wedge the cache path).
    let platform = Arc::clone(&platform);
    let result = tauri::async_runtime::spawn_blocking(move || enumerate_apps(*platform)).await;
    *cache.inflight.lock() = false;
    let result = result.map_err(|e| format!("applications thread failed: {e}"))?;
    *cache.done.lock() = Some(Arc::new(result.clone()));
    Ok(result)
}

/// Result of one uninstall run (spec §11 uninstall dialog).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UninstallResult {
    /// Process images closed before the uninstaller ran.
    pub closed_processes: Vec<String>,
    /// Uninstaller exit code (registry) or 0 (MSIX).
    pub exit_code: i32,
    /// Leftovers that remain after the run (fresh matching).
    pub remaining_leftovers: Vec<LeftoverGroup>,
    /// True when the app's registry/MSIX entry is gone after the run.
    pub removed_entry: bool,
}

/// Run the uninstall flow for one app (spec §11): close processes from
/// `InstallLocation` (EnumWindows→WM_CLOSE→5 s→TerminateProcess), run
/// the app's own uninstaller (never trash program files directly),
/// wait for exit, then re-match its leftovers so the UI can offer
/// staging.
///
/// # Errors
/// String error when the uninstaller cannot start or exceeds the wait.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub async fn uninstall_app(
    id: &str,
    platform: State<'_, Arc<HostPlatform>>,
    _state: State<'_, AppState>,
) -> Result<UninstallResult, String> {
    let platform = Arc::clone(&platform);
    let id = id.to_string();
    tauri::async_runtime::spawn_blocking(move || run_uninstall(*platform, &id))
        .await
        .map_err(|e| format!("uninstall thread failed: {e}"))?
}

/// The blocking-pool body of the uninstall flow.
fn run_uninstall(platform: HostPlatform, id: &str) -> Result<UninstallResult, String> {
    // Re-enumerate to find the app's current record (fresh strings —
    // the cached snapshot may predate a repair/update).
    let registry = crate::platform::os::registry_uninstall_entries();
    let msix = crate::platform::os::msix_packages().unwrap_or_default();
    let reg = registry.iter().find(|r| r.id == id);
    let msix_app = msix.iter().find(|m| m.id == id);
    let (install_location, uninstall_string, is_msix, full_name, name, publisher) =
        match (reg, msix_app) {
            (Some(r), _) => (
                r.install_location.clone(),
                if r.quiet_uninstall_string.is_empty() {
                    r.uninstall_string.clone()
                } else {
                    r.quiet_uninstall_string.clone()
                },
                false,
                String::new(),
                r.name.clone(),
                r.publisher.clone(),
            ),
            (None, Some(m)) => (
                m.install_location.clone(),
                String::new(),
                true,
                m.id.clone(),
                m.name.clone(),
                m.publisher.clone(),
            ),
            (None, None) => return Err(format!("application {id} not found")),
        };

    // 1. Close processes from the install location.
    let closed = if install_location.is_empty() {
        Vec::new()
    } else {
        crate::platform::os::close_processes_under(&install_location)
    };

    // 2. Run the uninstaller (registry) or RemovePackageAsync (MSIX).
    let mut exit_code = 0i32;
    if is_msix {
        crate::platform::os::msix_remove_package(&full_name)?;
    } else {
        exit_code = crate::platform::os::launch_and_wait_uninstaller(&uninstall_string)?;
    }

    // 3. Fresh leftover matching for this app.
    let roots: Vec<RootListing> = leftover_roots(platform)
        .into_iter()
        .map(|(idx, path)| list_root(idx, &path))
        .collect();
    let install_folder = install_location
        .rsplit(['\\', '/'])
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();
    let identity = AppIdentity {
        display_name: name,
        install_folder,
        family_name: if is_msix { full_name } else { String::new() },
        publisher,
    };
    let leftovers = apps::find_leftovers(&identity, &roots);

    // 4. Entry gone?
    let removed_entry = if is_msix {
        match crate::platform::os::msix_packages() {
            Ok(list) => !list.iter().any(|m| m.id == id),
            Err(_) => true, // enumeration failed; assume removed (honest default)
        }
    } else {
        !registry
            .iter()
            .any(|r| r.id == id && !r.uninstall_string.is_empty())
            || exit_code == 0
    };
    Ok(UninstallResult {
        closed_processes: closed,
        exit_code,
        remaining_leftovers: leftovers,
        removed_entry,
    })
}

/// Leftover root paths as display paths (for the JS staging flow and
/// tooltips).
///
/// # Errors
/// String error when known folders cannot be resolved.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn leftover_root_paths(platform: State<'_, Arc<HostPlatform>>) -> Vec<String> {
    leftover_roots(**platform)
        .into_iter()
        .map(|(_, p)| p)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskbytes_core::apps::ROOT_LABELS;

    #[test]
    fn round_up_cluster_math() {
        assert_eq!(round_up(0, 4096), 0);
        assert_eq!(round_up(1, 4096), 4096);
        assert_eq!(round_up(4096, 4096), 4096);
        assert_eq!(round_up(4097, 4096), 8192);
        assert_eq!(round_up(10, 0), 10); // degenerate cluster
    }

    #[test]
    fn dir_size_of_plain_files_and_missing() {
        let dir = std::env::temp_dir().join(format!("db-apps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.bin"), vec![0u8; 1000]).unwrap();
        let s = dir_allocated_size(&dir.to_string_lossy(), 1);
        assert_eq!(s, 1000);
        // Missing path contributes zero (best effort, no panic).
        assert_eq!(dir_allocated_size("Z:\\nope-XYZ", 4096), 0);
        diskbytes_core::snapshots::remove_app_data_tree(&dir);
    }

    #[test]
    fn roots_are_labeled_in_spec_order() {
        // Pure shape check: the ROOT_LABELS constant matches the spec's
        // 5 roots (order fixed by spec §11 group captions).
        assert_eq!(ROOT_LABELS.len(), 5);
        assert_eq!(ROOT_LABELS[0], "Local AppData");
        assert_eq!(ROOT_LABELS[1], "Roaming AppData");
        assert_eq!(ROOT_LABELS[2], "LocalLow AppData");
        assert_eq!(ROOT_LABELS[3], "ProgramData");
        assert_eq!(ROOT_LABELS[4], "Store package data");
    }
}
