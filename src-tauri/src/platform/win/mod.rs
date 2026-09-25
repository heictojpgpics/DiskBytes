//! `platform/win/` — the ONLY module tree allowed to call windows-rs
//! directly (doc 02 §2). Implements [`Platform`] for Windows:
//!
//! - `list_dir`: the spec §4 engine — `NtQueryDirectoryFile` with
//!   `FileIdFullDirectoryInformation` (class 38), one reusable 256 KiB
//!   8-byte-aligned buffer, `\\?\` paths,
//!   `FILE_LIST_DIRECTORY | SYNCHRONIZE`, share read/write/delete,
//!   `FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT`,
//!   `RestartScan = true` on the first call, loop to
//!   `STATUS_NO_MORE_FILES`, records walked by `NextEntryOffset`.
//!   Buffer/loop/FFI discipline ported from dua-cli's shipped reader
//!   (`dua-lib/src/windows.rs`, reference clone) — unaligned reads,
//!   `offset_of!` header size, checked record bounds.
//! - drive roots, known folders and volume serials for This PC /
//!   apps-roots / WinSxS identity.
//!
//! This module is FFI-dense by design (doc 02 §2 makes it the single
//! windows-rs seam). Every unsafe block below carries a `// SAFETY:`
//! comment stating its invariant; the lint that demands doc-form
//! `# Safety` sections is allowed module-wide because inline comments
//! travel with the code they guard.

#![allow(unsafe_code)]
#![allow(clippy::undocumented_unsafe_blocks)]
#![allow(clippy::cast_possible_wrap)] // FILETIME ticks: valid values are < 2^63/1e7
#![allow(clippy::cast_possible_truncation)] // buffer_len is 256 KiB by construction (BUFFER_WORDS)
#![allow(clippy::cast_sign_loss)]
// FILETIME/FileId reinterpreted as unsigned ticks/ids (NT layout)
// The NtQuerySystemInformation record walk casts a u8 base pointer to
// the 8-aligned record struct; the buffer is Vec<u64>-backed (aligned
// by construction) and the offsets are record-multiples.
#![allow(clippy::cast_ptr_alignment)]

// Split into cohesive submodules (worklog wave 2a): every item keeps
// its original visibility through the glob re-exports, so the
// `crate::platform::os::X` alias surface is byte-identical.
pub mod apps;
pub mod com;
pub mod dir;
pub mod license;
pub mod monitor;
pub mod recycle;
pub mod sysinfo;
pub mod turbo;

pub use apps::*;
pub use com::*;
pub use dir::*;
pub use license::*;
pub use monitor::*;
pub use recycle::*;
pub use sysinfo::*;
pub use turbo::*;
