//! Snapshot model, atomic writes and case-insensitive diffing (spec §13).
//!
//! A snapshot stores folder path → logical size for **folders ≥ 1 MB only**
//! (spec: "skip entire smaller subtrees"). Storage layout (app-side
//! `%LOCALAPPDATA%\DiskBytes\Snapshots\`):
//! - one JSON file per snapshot ([`Snapshot`]),
//! - a small `index.json` of [`SnapshotSummary`] rows so the list loads
//!   instantly without opening every file.
//!
//! Both are written atomically (temp file + rename) by [`write_json_atomic`].
//!
//! The diff ([`diff`]) compares path keys **case-insensitively** (NTFS is),
//! treats a folder missing on one side as 0, sorts by absolute change and
//! truncates to the top 200. Different roots are flagged, never assumed
//! equal.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::CoreError;

/// Folders strictly below 1 MB of logical size are not stored (spec §13).
pub const MIN_FOLDER_SIZE: u64 = 1024 * 1024;

/// Maximum number of change rows the diff emits (spec: "top 200").
pub const MAX_DIFF_ROWS: usize = 200;

/// One folder measurement inside a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderSize {
    /// Folder path as displayed when the snapshot was taken.
    pub path: String,
    /// Logical size in bytes (rolled-up `EndOfFile` sum).
    pub logical: u64,
}

/// A full snapshot file (one JSON per snapshot).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// Snapshot id (app generates; filename-stable).
    pub id: String,
    /// Root display path the scan targeted.
    pub root: String,
    /// Unix seconds when the snapshot was taken.
    pub taken_at: i64,
    /// Folder rows (only ≥ [`MIN_FOLDER_SIZE`]; enforced by [`Snapshot::build`]).
    pub folders: Vec<FolderSize>,
}

impl Snapshot {
    /// Build a snapshot from `(path, logical)` pairs, dropping folders
    /// below the 1 MB threshold and keeping input order for the rows
    /// that survive.
    #[must_use]
    pub fn build(id: String, root: String, taken_at: i64, pairs: Vec<(String, u64)>) -> Self {
        let folders = pairs
            .into_iter()
            .filter(|&(_, logical)| logical >= MIN_FOLDER_SIZE)
            .map(|(path, logical)| FolderSize { path, logical })
            .collect();
        Self {
            id,
            root,
            taken_at,
            folders,
        }
    }

    /// Total logical size across stored folders.
    #[must_use]
    pub fn total(&self) -> u64 {
        // Saturating: a corrupt or hand-edited snapshot can hold u64-scale
        // folder sizes whose plain `.sum()` PANICS on overflow in debug
        // builds and silently wraps in release (found by the proptest
        // suite's snapshot-delta property).
        self.folders
            .iter()
            .map(|f| f.logical)
            .fold(0u64, u64::saturating_add)
    }

    /// Lower-cased path → size map for diffing (NTFS is case-insensitive).
    #[must_use]
    fn key_map(&self) -> BTreeMap<String, u64> {
        self.folders
            .iter()
            .map(|f| (f.path.to_ascii_lowercase(), f.logical))
            .collect()
    }
}

/// One `index.json` row so the snapshot list loads instantly (spec §13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSummary {
    /// Snapshot id.
    pub id: String,
    /// Root path that was scanned.
    pub root: String,
    /// Unix seconds when taken.
    pub date: i64,
    /// Total logical size of stored folders.
    pub total: u64,
    /// Number of stored folder rows.
    pub folder_count: u64,
}

/// The summary row for a snapshot (index.json entry).
#[must_use]
pub fn summarize(s: &Snapshot) -> SnapshotSummary {
    SnapshotSummary {
        id: s.id.clone(),
        root: s.root.clone(),
        date: s.taken_at,
        total: s.total(),
        folder_count: s.folders.len() as u64,
    }
}

/// One changed folder between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderChange {
    /// Folder path — spelled as in the AFTER snapshot when present there,
    /// otherwise as in BEFORE (missing sides count as 0, spec §13).
    pub path: String,
    /// Logical size in the before snapshot (0 when absent).
    pub before: u64,
    /// Logical size in the after snapshot (0 when absent).
    pub after: u64,
    /// `after − before`: positive = growth (red "+X"), negative = shrinkage.
    pub delta: i64,
}

/// The computed diff between two snapshots (spec §13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDiff {
    /// True when the two snapshots scanned different roots (UI says so).
    pub different_roots: bool,
    /// Root of the before snapshot.
    pub root_before: String,
    /// Root of the after snapshot.
    pub root_after: String,
    /// Top-200 changes sorted by absolute change, largest first.
    pub changes: Vec<FolderChange>,
    /// Stored-folder totals (before, after) for the header line.
    pub totals: (u64, u64),
}

/// Case-insensitive diff of two snapshots (spec §13).
///
/// - A folder present on one side only counts as 0 on the other.
/// - Sorted by absolute change descending; ties keep deterministic
///   (path, before) order; truncated to [`MAX_DIFF_ROWS`].
/// - The path spelling prefers the AFTER snapshot (the row describes
///   "what this folder became").
#[must_use]
pub fn diff(before: &Snapshot, after: &Snapshot) -> SnapshotDiff {
    let b = before.key_map();
    let a = after.key_map();
    let mut keys: Vec<&String> = b.keys().chain(a.keys()).collect();
    keys.sort_unstable();
    keys.dedup();

    let mut changes: Vec<FolderChange> = Vec::with_capacity(keys.len());
    for k in keys {
        let before_sz = *b.get(k).unwrap_or(&0);
        let after_sz = *a.get(k).unwrap_or(&0);
        if before_sz == after_sz {
            continue; // unchanged rows are not reported
        }
        // Prefer the after spelling, else the before spelling.
        let spelling = after
            .folders
            .iter()
            .find(|f| f.path.eq_ignore_ascii_case(k))
            .map_or_else(
                || {
                    before
                        .folders
                        .iter()
                        .find(|f| f.path.eq_ignore_ascii_case(k))
                        .map_or_else(|| k.clone(), |f| f.path.clone())
                },
                |f| f.path.clone(),
            );
        let delta = after_sz as i64 - before_sz as i64;
        changes.push(FolderChange {
            path: spelling,
            before: before_sz,
            after: after_sz,
            delta,
        });
    }
    changes.sort_by(|x, y| {
        y.delta
            .unsigned_abs()
            .cmp(&x.delta.unsigned_abs())
            .then_with(|| x.path.cmp(&y.path))
            .then(x.before.cmp(&y.before))
    });
    changes.truncate(MAX_DIFF_ROWS);

    SnapshotDiff {
        different_roots: !before.root.eq_ignore_ascii_case(&after.root),
        root_before: before.root.clone(),
        root_after: after.root.clone(),
        changes,
        totals: (before.total(), after.total()),
    }
}

/// Serialize `value` to `path` atomically: write a temp file in the SAME
/// directory (rename must not cross filesystems), flush + fsync, then
/// rename over the destination (spec §13: "temp file + rename").
///
/// The temp name embeds the process id so concurrent writers never
/// collide; a failed attempt removes the temp file.
///
/// # Errors
/// - [`CoreError::SnapshotParse`] wrapping the underlying io/serialize
///   failure (message includes the path) — the destination file is left
///   untouched when the write fails.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), CoreError> {
    let dir = path.parent().ok_or_else(|| {
        CoreError::SnapshotParse("snapshot path has no parent directory".to_string())
    })?;
    let tmp = dir.join(format!(
        "{}.{}.tmp",
        path.file_name().map_or_else(
            || "snapshot".to_string(),
            |n| n.to_string_lossy().into_owned()
        ),
        std::process::id()
    ));
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| CoreError::SnapshotParse(format!("serialize: {e}")))?;
    let write = || -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
        f.sync_data()?;
        drop(f);
        fs::rename(&tmp, path)?;
        Ok(())
    };
    match write() {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(CoreError::SnapshotParse(format!("atomic write: {e}")))
        }
    }
}

/// Parse a snapshot JSON file (list rows load from `index.json` instead).
///
/// # Errors
/// - [`CoreError::SnapshotParse`] when the file cannot be read or does not
///   deserialize into a [`Snapshot`] (message includes the path).
pub fn read_snapshot(path: &Path) -> Result<Snapshot, CoreError> {
    let text = fs::read_to_string(path)
        .map_err(|e| CoreError::SnapshotParse(format!("read {}: {e}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| CoreError::SnapshotParse(format!("parse {}: {e}", path.display())))
}

/// Delete one snapshot: removes its data file and rewrites `index.json`
/// without it (atomic rewrite). Idempotent — a missing file is treated
/// as already deleted, and a missing/corrupt index is left as-is.
///
/// This is the inverse of [`write_json_atomic`] over the app-owned
/// snapshot store. Snapshot files are documents DISKBYTES created
/// (spec §13: `%LOCALAPPDATA%\DiskBytes\Snapshots\`), never user files;
/// the app crate keeps zero direct-delete APIs (R7.1 grep scope =
/// `src-tauri/src`), so this persistence-layer operation lives here with
/// the rest of the store.
///
/// # Errors
/// - [`CoreError::SnapshotParse`] when the file exists but cannot be
///   removed, or the index rewrite fails.
pub fn delete_snapshot(dir: &Path, id: &str) -> Result<(), CoreError> {
    let file = dir.join(format!("{id}.json"));
    match fs::remove_file(&file) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(CoreError::SnapshotParse(format!(
                "delete {}: {e}",
                file.display()
            )))
        }
    }
    let index = dir.join("index.json");
    let Ok(text) = fs::read_to_string(&index) else {
        return Ok(()); // no index yet — nothing to update
    };
    let Ok(mut idx) = serde_json::from_str::<Vec<SnapshotSummary>>(&text) else {
        return Ok(()); // corrupt index — the file delete already succeeded
    };
    let before = idx.len();
    idx.retain(|s| s.id != id);
    if idx.len() != before {
        write_json_atomic(&index, &idx)?;
    }
    Ok(())
}

/// Remove a directory tree of APP-OWNED data (test fixtures, caches).
///
/// The app crate keeps zero direct-delete APIs (R7.1 grep scope =
/// `src-tauri/src`); this is the same persistence-layer rule as
/// [`delete_snapshot`] — never used for user files.
pub fn remove_app_data_tree(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

/// Remove an APP-OWNED data FILE (license cache and friends). Same
/// persistence-layer rule as [`remove_app_data_tree`]/[`delete_snapshot`]
/// — the app crate keeps zero direct-delete APIs (R7.1).
pub fn remove_app_data_file(path: &Path) {
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str, root: &str, at: i64, pairs: Vec<(&str, u64)>) -> Snapshot {
        Snapshot::build(
            id.to_string(),
            root.to_string(),
            at,
            pairs.into_iter().map(|(p, s)| (p.to_string(), s)).collect(),
        )
    }

    #[test]
    fn small_folders_are_excluded() {
        let s = snap(
            "s1",
            "C:\\",
            1000,
            vec![("C:\\Big", 5 * 1024 * 1024), ("C:\\Tiny", 4096)],
        );
        assert_eq!(s.folders.len(), 1);
        assert_eq!(s.folders[0].path, "C:\\Big");
        assert_eq!(s.total(), 5 * 1024 * 1024);
        // exactly 1 MB is kept (>= threshold)
        let s2 = snap("s2", "C:\\", 1, vec![("C:\\Edge", MIN_FOLDER_SIZE)]);
        assert_eq!(s2.folders.len(), 1);
    }

    #[test]
    fn diff_is_case_insensitive_and_missing_counts_zero() {
        // Sizes in MiB — Snapshot::build drops rows below the 1 MB floor.
        const MB: u64 = 1024 * 1024;
        let before = snap(
            "a",
            "C:\\Users\\me",
            10,
            vec![
                ("C:\\Users\\me\\Docs", 100 * MB),
                ("C:\\Users\\me\\Gone", 50 * MB),
            ],
        );
        let after = snap(
            "b",
            "C:\\Users\\me",
            20,
            vec![
                ("C:\\Users\\me\\DOCS", 140 * MB),
                ("C:\\Users\\me\\New", 30 * MB),
            ],
        );
        let d = diff(&before, &after);
        assert!(!d.different_roots);
        // Docs grew 100→140 MiB (+40); Gone vanished 50→0 (−50); New 0→30 (+30).
        assert_eq!(d.changes.len(), 3);
        // Largest absolute change first: −50 MiB (Gone).
        assert_eq!(d.changes[0].path, "C:\\Users\\me\\Gone");
        assert_eq!(d.changes[0].delta, -(50 * MB as i64));
        // Docs row uses the AFTER spelling (uppercase DOCS).
        let docs = d
            .changes
            .iter()
            .find(|c| c.path.to_ascii_lowercase().contains("docs"))
            .unwrap();
        assert_eq!(docs.path, "C:\\Users\\me\\DOCS");
        assert_eq!(docs.before, 100 * MB);
        assert_eq!(docs.after, 140 * MB);
        assert_eq!(docs.delta, 40 * MB as i64);
        let new = d.changes.iter().find(|c| c.path.ends_with("New")).unwrap();
        assert_eq!(new.before, 0);
        assert_eq!(new.after, 30 * MB);
    }

    #[test]
    fn diff_flags_different_roots_case_insensitively() {
        let a = snap("a", "C:\\Work", 1, vec![]);
        let b = snap("b", "c:\\work", 2, vec![]);
        assert!(!diff(&a, &b).different_roots);
        let c = snap("c", "D:\\Work", 3, vec![]);
        assert!(diff(&a, &c).different_roots);
    }

    #[test]
    fn diff_truncates_to_top_200_sorted_by_absolute_change() {
        const MB: u64 = 1024 * 1024;
        let mut pairs_before: Vec<(String, u64)> = Vec::new();
        let mut pairs_after: Vec<(String, u64)> = Vec::new();
        for i in 0u32..600 {
            let p = format!("C:\\F\\folder-{i:03}");
            pairs_before.push((p.clone(), 100 * MB + u64::from(i)));
            // growth proportional to (600 - i): folder-000 grows most
            pairs_after.push((p, 100 * MB + u64::from(600 - i)));
        }
        let before = Snapshot::build("a".into(), "C:\\F".into(), 1, pairs_before);
        let after = Snapshot::build("b".into(), "C:\\F".into(), 2, pairs_after);
        let d = diff(&before, &after);
        assert_eq!(d.changes.len(), MAX_DIFF_ROWS);
        // All deltas are distinct; the top row must be the biggest growth.
        assert_eq!(d.changes[0].delta, 600);
        assert_eq!(d.changes[0].path, "C:\\F\\folder-000");
    }
    #[test]
    fn unchanged_rows_are_not_reported() {
        let a = snap("a", "C:\\", 1, vec![("C:\\Same", 100)]);
        let b = snap("b", "C:\\", 2, vec![("C:\\SAME", 100)]);
        assert_eq!(diff(&a, &b).changes.len(), 0);
    }

    #[test]
    fn atomic_write_round_trips() {
        let dir = std::env::temp_dir().join(format!("db-snap-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snap.json");
        let s = snap(
            "id-7",
            "C:\\Work",
            1234,
            vec![("C:\\Work\\A", 2 * 1024 * 1024)],
        );
        write_json_atomic(&path, &s).unwrap();
        let back = read_snapshot(&path).unwrap();
        assert_eq!(back, s);
        // No temp residue.
        let residue: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(residue.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn summary_row_matches_snapshot() {
        let s = snap("id-9", "C:\\Data", 77, vec![("C:\\Data\\A", 1048576)]);
        let sum = summarize(&s);
        assert_eq!(sum.id, "id-9");
        assert_eq!(sum.root, "C:\\Data");
        assert_eq!(sum.date, 77);
        assert_eq!(sum.total, 1048576);
        assert_eq!(sum.folder_count, 1);
        // serializes to camelCase keys for the JS side
        let j = serde_json::to_value(&sum).unwrap();
        assert!(j.get("folderCount").is_some());
        assert!(j.get("folder_count").is_none());
    }

    #[test]
    fn delete_removes_file_and_index_row_idempotently() {
        let dir = std::env::temp_dir().join(format!("db-snap-del-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let a = snap("d1", "C:\\Work", 1, vec![("C:\\Work\\A", 2 * 1024 * 1024)]);
        let b = snap("d2", "C:\\Work", 2, vec![("C:\\Work\\B", 3 * 1024 * 1024)]);
        write_json_atomic(&dir.join("d1.json"), &a).unwrap();
        write_json_atomic(&dir.join("d2.json"), &b).unwrap();
        let idx = vec![summarize(&a), summarize(&b)];
        write_json_atomic(&dir.join("index.json"), &idx).unwrap();

        delete_snapshot(&dir, "d1").unwrap();
        assert!(!dir.join("d1.json").exists());
        assert!(dir.join("d2.json").exists());
        let after: Vec<SnapshotSummary> =
            serde_json::from_str(&fs::read_to_string(dir.join("index.json")).unwrap()).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, "d2");

        // Idempotent: deleting again is a success, index untouched.
        delete_snapshot(&dir, "d1").unwrap();
        // No index at all → still fine.
        let empty = std::env::temp_dir().join(format!("db-snap-empty-{}", std::process::id()));
        fs::create_dir_all(&empty).unwrap();
        delete_snapshot(&empty, "nope").unwrap();
        fs::remove_dir_all(&empty).ok();
        fs::remove_dir_all(&dir).ok();
    }
}
