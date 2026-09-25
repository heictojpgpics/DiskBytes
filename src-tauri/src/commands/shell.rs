//! Shell-facing commands (spec §7/§8): Open (folders / "Open with
//! default app"), Show in Explorer, Copy Path, and the 64 KB text
//! preview read. All Windows shell calls stay behind
//! `platform::win` (doc 02 §2: the only windows-rs seam); these
//! commands are thin wrappers with user-readable errors (R2: no silent
//! fallbacks).
//!
//! `preview_text` enforces R7.3 at the command boundary: cloud
//! placeholders are NEVER read (no preview, no hash, no download).

use std::io::Read as _;

use serde::Serialize;
use tauri::State;

use crate::platform::HostPlatform;
use crate::state::AppState;

/// Max text preview size (spec §8: "first 64 KB").
pub const PREVIEW_CAP: usize = 64 * 1024;

/// Open a node with its default handler. Folders open in Explorer
/// (context menu "Open", spec §7); files open with the registered app
/// (preview overlay button, spec §8). Cloud placeholders are refused
/// (R7.3 — no action that would hydrate them).
///
/// # Errors
/// String error when the generation is stale, the node is missing, the
/// item is a cloud placeholder, or the shell call fails.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn open_node(generation: u64, id: u32, state: State<'_, AppState>) -> Result<(), String> {
    let path = node_display_path(&state, generation, id)?;
    HostPlatform::open_path(&path)
}

/// Open an EXTERNAL https link in the system browser (doc 06 §4: the
/// buy page is hosted checkout — no in-app payment iframe). Only our
/// own https surfaces call this.
///
/// # Errors
/// String error when the URL is not https or the shell call fails.
#[tauri::command]
pub fn open_url(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("only https links can be opened".into());
    }
    HostPlatform::open_path(url)
}

/// Show a node in Explorer with it selected (spec §6: via
/// `SHOpenFolderAndSelectItems`, never a spawned process).
///
/// # Errors
/// String error when the generation is stale, the node is missing, or
/// the shell call fails.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn reveal_in_explorer(
    generation: u64,
    id: u32,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let path = node_display_path(&state, generation, id)?;
    HostPlatform::reveal_in_explorer(&path)
}

/// Copy a node's display path to the clipboard (spec §6/§8).
///
/// # Errors
/// String error when the generation is stale, the node is missing, or
/// the clipboard is unavailable.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn copy_path(generation: u64, id: u32, state: State<'_, AppState>) -> Result<(), String> {
    let path = node_display_path(&state, generation, id)?;
    HostPlatform::copy_to_clipboard(&path)
}

/// The text preview payload (spec §8).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextPreview {
    /// Lossy-decoded UTF-8 of the first 64 KB.
    pub text: String,
    /// True when the file is larger than the cap.
    pub truncated: bool,
    /// Bytes actually read.
    pub read: usize,
}

/// Read the first 64 KB of a file as text (spec §8: "first 64 KB, read
/// by a Rust command"). Cloud placeholders are NEVER read (R7.3 — the
/// preview overlay offers no action that would download them; this
/// command refuses them with a clear reason).
///
/// # Errors
/// String error when the generation is stale, the node is missing, the
/// item is a cloud placeholder or a folder, or the read fails.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn preview_text(
    generation: u64,
    id: u32,
    state: State<'_, AppState>,
) -> Result<TextPreview, String> {
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
    let Some(n) = tree.node(id) else {
        return Err(format!("node {id} not in tree"));
    };
    if n.is_cloud_placeholder() {
        return Err("Stored in the cloud (not downloaded) — preview is disabled.".into());
    }
    if n.is_dir() {
        return Err("Folders have no text preview.".into());
    }
    let path = tree.node_path(id);
    let name = tree.name(id);
    drop(guard);
    // Read AT MOST PREVIEW_CAP bytes — the old `std::fs::read` slurped
    // the WHOLE file (a 40 GB video preview briefly doubled its size in
    // RAM) before truncating. `take` bounds the read at the syscall
    // level; `truncated` comes from the file's real length.
    let file = std::fs::File::open(&path).map_err(|e| format!("Couldn't read {name}: {e}"))?;
    let file_len = file.metadata().map_or(0, |m| m.len());
    let mut bytes =
        Vec::with_capacity(file_len.min(u64::try_from(PREVIEW_CAP).unwrap_or(u64::MAX)) as usize);
    let mut capped = file.take(u64::try_from(PREVIEW_CAP).unwrap_or(u64::MAX));
    capped
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Couldn't read {name}: {e}"))?;
    let read = bytes.len().min(PREVIEW_CAP);
    let truncated = file_len > u64::try_from(PREVIEW_CAP).unwrap_or(u64::MAX);
    Ok(TextPreview {
        text: String::from_utf8_lossy(&bytes[..read]).into_owned(),
        truncated,
        read,
    })
}

/// Resolve a node's display path under the generation guard (shared by
/// the shell commands).
fn node_display_path(
    state: &State<'_, AppState>,
    generation: u64,
    id: u32,
) -> Result<String, String> {
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
    if tree.node(id).is_none() {
        return Err(format!("node {id} not in tree"));
    }
    Ok(tree.node_path(id))
}

/// Compact hover-chip payload (spec §7 hover chip: icon, name, size, %
/// of scan, file-count pill). Names come from the `get_names` LRU; this
/// carries the rest.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HoverDetails {
    pub id: u32,
    /// Node name (the chip's title).
    pub name: String,
    pub is_dir: bool,
    /// On-disk size.
    pub size: u64,
    /// Share of the whole scan (the chip's "%").
    pub share_of_scan: f64,
    /// Descendant file count (folders).
    pub file_count: u64,
    pub is_cloud: bool,
    pub is_protected: bool,
    /// Category label (icon tint).
    pub category: String,
    /// `0xRRGGBB`.
    pub category_color: u32,
}

/// Hover details for one node (spec §7; the JS side caches per id).
///
/// # Errors
/// String error when the generation is stale or the node is missing.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn hover_details(
    generation: u64,
    id: u32,
    state: State<'_, AppState>,
) -> Result<HoverDetails, String> {
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
    let n = tree
        .node(id)
        .ok_or_else(|| format!("node {id} not in tree"))?;
    let root_total = tree.node(tree.root).map_or(0, |r| r.on_disk);
    let file_count = tree
        .dir_extras
        .get(n.dir_index as usize)
        .filter(|_| n.is_dir())
        .map_or(0, |e| e.file_count);
    let name = String::from_utf16_lossy(tree.name_u16(id));
    let cat = if n.is_dir() {
        tree.dominant_category(id)
    } else {
        n.category()
    };
    Ok(HoverDetails {
        name,
        id,
        is_dir: n.is_dir(),
        size: n.on_disk,
        share_of_scan: if root_total > 0 {
            n.on_disk as f64 / root_total as f64
        } else {
            0.0
        },
        file_count,
        is_cloud: n.is_cloud_placeholder(),
        is_protected: n.is_protected(),
        category: if n.is_dir() {
            "Folder".to_string()
        } else {
            cat.label().to_string()
        },
        category_color: cat.color(),
    })
}
