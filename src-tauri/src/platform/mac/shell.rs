//! Shell integration (open/reveal/clipboard) + Trash policy.

use objc2::msg_send;
use objc2::runtime::AnyClass;

use super::ffi::NSPasteboardTypeString;
use super::objc::Id;
use super::objc::{cf_array_of, file_url, ns_string, open_config_default, workspace_shared};
use super::MacPlatform;

impl MacPlatform {
    /// Open a path with its default app (Finder for folders). The
    /// special "shell:RecycleBinFolder" sentinel opens the Trash.
    /// (The Result is the win.rs signature — the command layer calls it
    /// identically on both platforms; NSWorkspace openURLs has no
    /// failure path to report here.)
    ///
    /// # Errors
    /// Never on macOS (see the signature note above).
    #[allow(clippy::unnecessary_wraps)]
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
    /// activateFileViewerSelectingURLs). (Result = the win.rs signature.)
    ///
    /// # Errors
    /// Never on macOS (see the signature note above).
    #[allow(clippy::unnecessary_wraps)]
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
    ///
    /// # Errors
    /// When NSPasteboard is missing or refuses the write.
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
