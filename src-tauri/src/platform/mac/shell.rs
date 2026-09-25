//! Shell integration (open/reveal/clipboard) + Trash policy.

use objc2::msg_send;
use objc2::runtime::AnyClass;

use super::objc::Id;
use super::objc::{cf_array_of, file_url, ns_string, open_config_default, workspace_shared};
use super::MacPlatform;

impl MacPlatform {
    /// Open a path with its default app (Finder for folders). The
    /// special "shell:RecycleBinFolder" sentinel opens the Trash.
    pub fn open_path(path: &str) -> Result<(), String> {
        let target = if path == "shell:RecycleBinFolder" {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            format!("{home}/.Trash")
        } else {
            path.to_string()
        };
        unsafe {
            let ws = workspace_shared();
            let url = file_url(&target);
            let arr = cf_array_of(&[url]);
            let cfg = open_config_default();
            // SAFETY: NSWorkspace openURLs:configuration: with a
            // one-element CFArray (toll-free NSArray).
            let _: () = unsafe { msg_send![ws, openURLs: arr, configuration: cfg] };
            Ok(())
        }
    }

    /// Reveal in Finder with selection (NSWorkspace
    /// activateFileViewerSelectingURLs).
    pub fn reveal_in_explorer(path: &str) -> Result<(), String> {
        unsafe {
            let ws = workspace_shared();
            let url = file_url(path);
            let arr = cf_array_of(&[url]);
            // SAFETY: NSWorkspace activateFileViewerSelectingURLs:.
            let _: () = unsafe { msg_send![ws, activateFileViewerSelectingURLs: arr] };
            Ok(())
        }
    }

    /// Copy text to the pasteboard (NSPasteboard generalPasteboard).
    pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
        unsafe {
            let class = AnyClass::get("NSPasteboard").ok_or("NSPasteboard missing")?;
            // SAFETY: generalPasteboard returns the shared board.
            let pb: Id = unsafe { msg_send![class, generalPasteboard] };
            let s = ns_string(text);
            let ttype = NSPasteboardTypeString();
            // SAFETY: clearContents + setString:forType: on the board.
            let _: () = unsafe { msg_send![pb, clearContents] };
            let ok: bool = unsafe { msg_send![pb, setString: s, forType: ttype] };
            ok.then_some(())
                .ok_or_else(|| "pasteboard write failed".into())
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Recycle (Trash) surface — mirrors win.rs.
// ─────────────────────────────────────────────────────────────────────
