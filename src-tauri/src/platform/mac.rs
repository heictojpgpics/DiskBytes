//! The macOS implementation of the platform seam (doc 02 §2; the
//! cross-platform architecture's `platform-macos`, Option A: the SAME
//! Tauri app runs on macOS with this module behind the trait).
//!
//! Mirrors the exact free-function surface of `win.rs` so the command
//! layer compiles on both operating systems through `platform::os`.
//!
//! - Scanning: `getattrlistbulk(2)` per the Mac BuildPrompt §4 (256 KiB
//!   buffer, `FSOPT_NOFOLLOW`, packed attribute records).
//! - Trash: `NSWorkspace.recycleURLs:` (Finder Trash — never a hard
//!   delete; the safety promise holds on both platforms).
//! - Reveal/open/clipboard: NSWorkspace / NSPasteboard via the ObjC
//!   runtime (`objc2` msg_send) with toll-free CoreFoundation strings.
//! - Monitor: Mach/BSD (`host_statistics64`, `getifaddrs`, `proc_*`).
//! - Licensing: `IOPlatformUUID` (IOKit) hardware id + Keychain
//!   (Security.framework) local persistence — the DPAPI analogue.
//!
//! SAFETY discipline: every unsafe block below documents its contract.
//! Hand-declared FFI keeps the dependency set at ONE macos-only crate
//! (`objc2`, decision-log S5).

#![allow(clippy::missing_safety_doc)]
// FFI seams: each unsafe block carries its own proof comment
// unsafe fn + explicit inner unsafe blocks (Rust 2021 style, same as
// the win.rs COM boundary): the double-unsafe is deliberate so every
// call site reads as an unsafe operation.
#![allow(unused_unsafe)]
#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::{c_char, c_int, c_void, CStr, CString};

use diskbytes_core::monitor::{CpuTicks, VolumeSample};
use diskbytes_core::platform::{DirEntryData, DirListing, KnownFolder, ListError, Platform};

// ─────────────────────────────────────────────────────────────────────
// Darwin / CoreFoundation / IOKit / Security FFI (hand-declared; the
// struct layouts are asserted in the tests module).
// ─────────────────────────────────────────────────────────────────────

type OSStatus = i32;
type Boolean = u8;
type MachPort = u32;
type HostT = u32;
type KernReturn = i32;

/// An ObjC object pointer (the C ABI id).
type Id = *mut AnyObject;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i32,
}

/// `struct attrlist` for `getattrlist(2)`.
#[repr(C)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

extern "C" {
    // getattrlistbulk(2) — the directory engine.
    fn getattrlistbulk(
        fd: c_int,
        attrlist: *mut AttrList,
        buffer: *mut c_void,
        buffersize: usize,
        options: u64,
    ) -> c_int;

    // statfs / getfsstat — volume geometry + inventory.
    fn statfs(path: *const c_char, buf: *mut StatFs) -> c_int;
    fn getfsstat(buf: *mut StatFs, bufsize: c_int, flags: c_int) -> c_int;

    // sysctl — CPU brand, memory size.
    fn sysctlbyname(
        name: *const c_char,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> c_int;

    // Mach — CPU tick counters + VM stats.
    fn host_statistics64(
        host: HostT,
        flavor: c_int,
        host_info64: *mut c_void,
        host_info64Cnt: *mut u32,
    ) -> KernReturn;
    fn mach_host_self() -> MachPort;

    // getifaddrs — network octet counters.
    fn getifaddrs(ifap: *mut *mut IfAddrs) -> c_int;
    fn freeifaddrs(ifa: *mut IfAddrs);

    // libproc — process inventory + usage.
    fn proc_listpids(kind: u32, typeinfo: u32, buffer: *mut c_void, buffersize: c_int) -> c_int;
    fn proc_pidpath(pid: i32, buffer: *mut c_void, buffersize: u32) -> c_int;
    fn proc_pid_rusage(pid: i32, flavor: c_int, buffer: *mut c_void) -> c_int;
}

// IOKit is a separate framework (not part of libSystem) — it needs its own
// linked extern block or the IOPlatformUUID symbols fail to resolve at link
// time.
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    // IOKit — the IOPlatformUUID hardware id.
    fn IOServiceGetMatchingService(main_port: MachPort, matching: *const c_void) -> u32;
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: *const c_void,
        allocator: *const c_void,
        options: u32,
    ) -> *mut c_void;
    fn IOObjectRelease(object: u32) -> KernReturn;
}

extern "C" {
    // Security.framework — Keychain (SecItem, the DPAPI analogue).
    fn SecItemAdd(attributes: *const c_void, result: *mut *const c_void) -> OSStatus;
    fn SecItemCopyMatching(query: *const c_void, result: *mut *const c_void) -> OSStatus;
    fn SecItemDelete(query: *const c_void) -> OSStatus;

    // CoreFoundation — toll-free bridged to Foundation objects.
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        c_str: *const c_char,
        encoding: u32,
    ) -> *mut c_void;
    fn CFStringGetCString(
        the_string: *const c_void,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> Boolean;
    fn CFStringGetLength(the_string: *const c_void) -> isize;
    fn CFDataGetBytePtr(the_data: *const c_void) -> *const u8;
    fn CFDataGetLength(the_data: *const c_void) -> isize;
    fn CFURLCreateWithFileSystemPath(
        alloc: *const c_void,
        file_path: *const c_void,
        path_style: isize,
        is_directory: Boolean,
    ) -> *mut c_void;
    fn CFArrayCreateMutable(
        alloc: *const c_void,
        capacity: isize,
        callbacks: *const c_void,
    ) -> *mut c_void;
    fn CFArrayAppendValue(the_array: *const c_void, value: *const c_void);
    fn CFArrayGetCount(the_array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(the_array: *const c_void, idx: isize) -> *const c_void;
    fn CFDictionaryCreateMutable(
        alloc: *const c_void,
        capacity: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *mut c_void;
    fn CFDictionaryAddValue(the_dict: *const c_void, key: *const c_void, value: *const c_void);
    fn CFDictionaryGetValueIfPresent(
        the_dict: *const c_void,
        key: *const c_void,
        value: *mut *const c_void,
    ) -> Boolean;
    fn CFRelease(cf: *const c_void);
    fn CFPropertyListCreateWithData(
        alloc: *const c_void,
        data: *const c_void,
        options: c_int,
        format: *mut c_int,
        error: *mut *const c_void,
    ) -> *mut c_void;
    fn CFDataCreate(alloc: *const c_void, bytes: *const u8, length: isize) -> *mut c_void;
    // AppKit constants bridged through CF.
    fn NSPasteboardTypeString() -> *const c_void;
    #[link_name = "kCFBooleanTrue"]
    fn cf_boolean_true() -> *const c_void;

    // libc pass-throughs with explicit link names.
    #[link_name = "open"]
    fn libc_open(path: *const c_char, flags: c_int, ...) -> c_int;
    #[link_name = "close"]
    fn libc_close(fd: c_int) -> c_int;
    #[link_name = "__error"]
    fn libc_errno() -> *mut c_int;
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

/// `struct statfs` (the published macOS layout, 64-bit).
#[repr(C)]
struct StatFs {
    f_bsize: u32,
    f_iosize: i32,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_fsid: [u32; 2],
    f_owner: u32,
    f_type: u32,
    f_flags: u32,
    f_fssubtype: u32,
    f_fstypename: [u8; 16],
    f_mntonname: [u8; 1024],
    f_mntfromname: [u8; 1024],
    f_flags2: u32,
    f_reserved: [u32; 7],
}

/// `struct ifaddrs` (published layout, 64-bit).
#[repr(C)]
struct IfAddrs {
    ifa_next: *mut IfAddrs,
    ifa_name: *mut c_char,
    ifa_flags: u32,
    ifa_addr: *mut IfSockaddr,
    ifa_netmask: *mut IfSockaddr,
    ifa_dstaddr: *mut IfSockaddr,
    ifa_data: *mut c_void,
}

/// The first 8 bytes of any `struct sockaddr_*`.
#[repr(C)]
struct IfSockaddr {
    sa_len: u8,
    sa_family: u8,
    sa_data: [u8; 6],
}

/// `struct if_data64` (in the AF_LINK if_data area).
#[repr(C)]
struct IfData {
    ifi_type: u8,
    ifi_typelen: u8,
    ifi_physical: u8,
    ifi_addrlen: u8,
    ifi_hdrlen: u8,
    ifi_recvquota: u8,
    ifi_xmitquota: u8,
    ifi_unused1: u8,
    ifi_mtu: u32,
    ifi_metric: u32,
    ifi_baudrate: u64,
    ifi_ipackets: u64,
    ifi_ierrors: u64,
    ifi_opackets: u64,
    ifi_oerrors: u64,
    ifi_collisions: u64,
    ifi_ibytes: u64,
    ifi_obytes: u64,
    ifi_imcasts: u64,
    ifi_omcasts: u64,
    ifi_iqdrops: u64,
    ifi_noproto: u64,
    ifi_recvtiming: u32,
    ifi_xmittiming: u32,
    ifi_lastchange: Timeval,
}

/// `struct rusage_info_v2`.
#[repr(C)]
struct RusageInfoV2 {
    ri_uuid: [u8; 16],
    ri_user_time: u64,
    ri_system_time: u64,
    ri_child_user_time: u64,
    ri_child_system_time: u64,
    ri_pkg_idle_wkups: u64,
    ri_energy_wkups: u64,
    ri_wired_size: u64,
    ri_resident_size: u64,
    ri_phys_footprint: u64,
    ri_proc_start_abstime: u64,
    ri_proc_exit_abstime: u64,
    ri_child_abstime: u64,
    ri_resident_size_peak: u64,
    ri_phys_footprint_peak: u64,
}

/// VM stats via `host_statistics64(HOST_VM_INFO64)`.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VmStatistics64 {
    free_count: u32,
    active_count: u32,
    inactive_count: u32,
    wire_count: u32,
    zero_fill_count: u32,
    reactivations: u32,
    pageins: u32,
    pageouts: u32,
    faults: u32,
    cow_faults: u32,
    lookups: u32,
    hits: u32,
    purges: u32,
    purgeable_count: u32,
    speculative_count: u32,
    decompressions: u32,
    compressions: u32,
    swapins: u32,
    swapouts: u32,
    compressor_page_count: u32,
    total_uncompressed_pages_in_compressor: u32,
}

const HOST_VM_INFO64: c_int = 4;
const HOST_CPU_LOAD_INFO: c_int = 3;
const RUSAGE_INFO_V2: c_int = 2;
const KERN_SUCCESS: KernReturn = 0;
const FSOPT_NOFOLLOW: u64 = 0x0000_0001;

// `attrgroup_t` bits from <sys/attr.h> (public ABI, stable — values
// cross-checked against the libc crate's macOS bindings,
// libc-0.2.189/src/unix/bsd/apple/mod.rs:4065+, which are generated from
// the SDK headers). The record layout + parse order below are borrowed
// from dua-cli (crates/dua-lib/src/macos/attributes.rs, MIT). The mac UI
// sat at "0 B" for three CI rounds because BOTH the layout AND these
// bits were invented: NAME is 0x1 (0x1000000 is DOCUMENT_ID), CRTIME is
// 0x200 (0x10 is OBJTAG), MODTIME is 0x400 (0x20 is OBJID) — the kernel
// happily returned THOSE attributes and every record failed to parse
// into an empty tree.
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_CRTIME: u32 = 0x0000_0200;
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;

const ATTR_FILE_TOTALSIZE: u32 = 0x0000_0002;
const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;

// <sys/vnode.h> vnode types — VALUES, not st_mode masks (the old
// `(objtype & 0xF000) == VDIR` could never match: VDIR is 2).
const VNON: u32 = 0;
#[cfg_attr(not(test), allow(dead_code))] // parsed value; tests exercise it
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

// ─────────────────────────────────────────────────────────────────────
// ObjC runtime (objc2) — raw id + class lookups, toll-free CF bridging.
// ─────────────────────────────────────────────────────────────────────

use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject};

unsafe fn ns_string(s: &str) -> Id {
    // SAFETY: Create-rule CFString (toll-free NSString); NUL-free input.
    let c = CString::new(s).expect("NUL-free string");
    let cf = unsafe {
        CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), 0x0800_0100 /* UTF8 */)
    };
    assert!(!cf.is_null(), "CFStringCreateWithCString failed");
    cf as Id
}

unsafe fn cf_string_to_string(cf: *const c_void) -> Option<String> {
    if cf.is_null() {
        return None;
    }
    // SAFETY: read-only CFString query with a generously sized buffer.
    unsafe {
        let len = CFStringGetLength(cf);
        if len <= 0 {
            return Some(String::new());
        }
        let mut buf = vec![0i8; (len * 4 + 8) as usize];
        let ok = CFStringGetCString(cf, buf.as_mut_ptr(), buf.len() as isize, 0x0800_0100);
        if ok == 0 {
            return None;
        }
        Some(CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

unsafe fn cf_release(cf: *const c_void) {
    if !cf.is_null() {
        // SAFETY: CFRelease on a non-null CF object we own (or borrowed
        // briefly for reads — balanced by the Create rules above).
        unsafe { CFRelease(cf) };
    }
}

unsafe fn workspace_shared() -> Id {
    let class = AnyClass::get("NSWorkspace").expect("NSWorkspace class");
    // SAFETY: sharedWorkspace returns the singleton (no ownership).
    unsafe { msg_send![class, sharedWorkspace] }
}

unsafe fn open_config_default() -> Id {
    // NSWorkspaceOpenConfiguration (macOS 10.15+) — default instance.
    let class =
        AnyClass::get("NSWorkspaceOpenConfiguration").expect("NSWorkspaceOpenConfiguration");
    // SAFETY: new returns an owned instance (autoreleased suffices here).
    unsafe { msg_send![class, new] }
}

unsafe fn file_url(path: &str) -> Id {
    let s = ns_string(path);
    // SAFETY: CFURLCreateWithFileSystemPath (Create rule) + toll-free
    // NSURL bridge; kCFURLPOSIXPathStyle = 0.
    let cf = unsafe { CFURLCreateWithFileSystemPath(std::ptr::null(), s as *const c_void, 0, 1) };
    assert!(!cf.is_null(), "CFURLCreate failed");
    cf as Id
}

unsafe fn cf_array_of(items: &[Id]) -> *mut c_void {
    // SAFETY: CFArrayCreateMutable (Create rule) with no callbacks.
    let arr = unsafe { CFArrayCreateMutable(std::ptr::null(), 0, std::ptr::null()) };
    for it in items {
        // SAFETY: appending retained CF objects.
        unsafe { CFArrayAppendValue(arr, *it as *const c_void) };
    }
    arr
}

unsafe fn run_loop_current() -> Id {
    let class = AnyClass::get("NSRunLoop").expect("NSRunLoop");
    // SAFETY: currentRunLoop returns the thread's loop (no ownership).
    unsafe { msg_send![class, currentRunLoop] }
}

unsafe fn run_loop_run_mode(rl: Id, seconds: f64) {
    let mode = ns_string("kCFRunLoopDefaultMode");
    let date_class = AnyClass::get("NSDate").expect("NSDate");
    // SAFETY: an autoreleased date `seconds` from now.
    let date: Id = unsafe { msg_send![date_class, dateWithTimeIntervalSinceNow: seconds] };
    // SAFETY: runMode:beforeDate: on the live run loop.
    let _: Boolean = unsafe { msg_send![rl, runMode: mode, beforeDate: date] };
}

unsafe fn cf_url_path(url: Id) -> Option<String> {
    // SAFETY: path on a CFURL returns an autoreleased CFString.
    let path: Id = unsafe { msg_send![url, path] };
    unsafe { cf_string_to_string(path as *const c_void) }
}

// ─────────────────────────────────────────────────────────────────────
// The block runtime (one escaping `void (^)(id)` shape).
// ─────────────────────────────────────────────────────────────────────

/// The clang Block_literal ABI.
#[repr(C)]
struct BlockLiteral {
    isa: *const AnyClass,
    flags: i32,
    reserved: i32,
    invoke: unsafe extern "C" fn(*const BlockLiteral, *mut AnyObject),
    descriptor: *const c_void,
}

#[repr(C)]
struct BlockDescriptor {
    reserved: usize,
    size: usize,
    copy: *const c_void,
    dispose: *const c_void,
}

static BLOCK_DESCRIPTOR: BlockDescriptor = BlockDescriptor {
    reserved: 0,
    size: std::mem::size_of::<BlockLiteral>(),
    copy: std::ptr::null(),
    dispose: std::ptr::null(),
};

// SAFETY: the descriptor is immutable null-fn data shared by every
// block literal (the clang ABI does the same with one global).
unsafe impl Sync for BlockDescriptor {}

/// A `void (^)(id)` block: the literal IS the first field (repr(C)),
/// the Rust closure boxed behind it in the same leaked allocation.
#[repr(C)]
struct Block1<F: Fn(*mut AnyObject)> {
    literal: BlockLiteral,
    _keep: Box<F>,
}

impl<F: Fn(*mut AnyObject)> Block1<F> {
    /// Build an escaping block (leaked; one commit per queue, bounded).
    fn new(f: F) -> *const Block1<F> {
        let this = Box::into_raw(Box::new(Block1 {
            literal: BlockLiteral {
                isa: block_isa(),
                flags: 1 << 24, // BLOCK_IS_GLOBAL (no copy/dispose needed)
                reserved: 0,
                invoke: Self::trampoline,
                descriptor: &BLOCK_DESCRIPTOR as *const BlockDescriptor as *const c_void,
            },
            _keep: Box::new(f),
        }));
        this as *const Block1<F>
    }

    unsafe extern "C" fn trampoline(lit: *const BlockLiteral, arg: *mut AnyObject) {
        // SAFETY: `lit` points at the `literal` field (offset 0) of a
        // leaked Block1 composite; the cast recovers the composite.
        let this = lit as *const Block1<F>;
        // SAFETY: the composite is immortal (leaked) and this
        // trampoline was created from that exact allocation.
        let f = unsafe { &(*this)._keep };
        f(arg);
    }
}

static BLOCK_ISA: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

fn block_isa() -> *const AnyClass {
    // The runtime's concrete block class is immortal; store the pointer
    // once as a leaked usize and cast back (Send/Sync via the primitive).
    let p = *BLOCK_ISA.get_or_init(|| {
        let class = AnyClass::get("_NSConcreteGlobalBlock").expect("block runtime class");
        class as *const AnyClass as usize
    });
    p as *const AnyClass
}

// ─────────────────────────────────────────────────────────────────────
// MacPlatform: the Platform trait implementation (getattrlistbulk).
// ─────────────────────────────────────────────────────────────────────

/// The macOS host. Registered in Tauri managed state exactly like
/// `WindowsPlatform` (the `HostPlatform` alias resolves it).
#[derive(Debug, Clone, Copy, Default)]
pub struct MacPlatform;

const BUFFER_SIZE: usize = 256 * 1024;

// Per-thread scratch buffer (dua-cli's per-walker-buffer pattern): the
// old global-mutex buffer serialized every scanner worker on ONE buffer —
// correct but a scan-wide bottleneck. thread_local gives each worker its
// own 256 KiB.
thread_local! {
    static LIST_BUFFER: std::cell::RefCell<Vec<u8>> =
        std::cell::RefCell::new(vec![0u8; BUFFER_SIZE]);
}

impl Platform for MacPlatform {
    fn list_dir(&self, verbatim_dir: &str) -> DirListing {
        // A caller may pass a Windows-style verbatim marker; the mac
        // engine takes plain POSIX paths.
        let path = verbatim_dir.trim_start_matches("\\\\?\\");
        let c_path = match CString::new(path.replace('\\', "/")) {
            Ok(c) => c,
            Err(_) => {
                return DirListing {
                    entries: Vec::new(),
                    error: Some(ListError::Other("path contained NUL".into())),
                }
            }
        };
        // SAFETY: open() on a NUL-terminated path, O_RDONLY|O_DIRECTORY.
        let fd = unsafe { libc_open(c_path.as_ptr(), 0x0000 | 0x0010_0000) };
        if fd < 0 {
            eprintln!("[mac-engine] open failed errno={} path={path}", errno());
            return DirListing {
                entries: Vec::new(),
                error: Some(errno_list_error()),
            };
        }
        let out = LIST_BUFFER.with(|b| {
            let mut guard = b.borrow_mut();
            list_dir_fd(fd, guard.as_mut_slice())
        });
        // SAFETY: close the fd we opened on THIS thread.
        unsafe { libc_close(fd) };
        out
    }

    fn fixed_drive_roots(&self) -> Vec<String> {
        volume_inventory()
            .into_iter()
            .filter(|(_, mount, _)| is_browsable_volume(mount))
            .map(|(_, mount, _)| mount)
            .collect()
    }

    fn known_folder(&self, folder: KnownFolder) -> Option<String> {
        let home = std::env::var("HOME").ok()?;
        let p = match folder {
            KnownFolder::Profile => home,
            KnownFolder::LocalAppData => format!("{home}/Library/Application Support"),
            KnownFolder::RoamingAppData => format!("{home}/Library/Application Support"),
            KnownFolder::ProgramData => "/Library".into(),
            KnownFolder::ProgramFiles => "/Applications".into(),
            KnownFolder::ProgramFilesX86 => "/Applications".into(),
            KnownFolder::ProgramFilesWindowsApps => "/System/Applications".into(),
            KnownFolder::UserPrograms => format!("{home}/Applications"),
        };
        Some(p)
    }

    fn volume_serial(&self, verbatim_path: &str) -> Option<u64> {
        let c = CString::new(
            verbatim_path
                .trim_start_matches("\\\\?\\")
                .replace('\\', "/"),
        )
        .ok()?;
        let mut st: StatFs = unsafe { std::mem::zeroed() };
        // SAFETY: statfs into a valid StatFs-sized out-struct.
        let rc = unsafe { statfs(c.as_ptr(), &mut st) };
        if rc != 0 {
            return None;
        }
        Some((u64::from(st.f_fsid[0]) << 32) | u64::from(st.f_fsid[1]))
    }
}

fn errno() -> i32 {
    // SAFETY: errno is thread-local; reading it is safe.
    unsafe { *libc_errno() }
}

fn errno_list_error() -> ListError {
    match errno() {
        13 | 1 => ListError::AccessDenied, // EACCES / EPERM
        2 => ListError::Vanished,          // ENOENT
        e => ListError::Other(format!("macOS error {e}")),
    }
}

/// The core `getattrlistbulk` loop over one directory fd.
fn list_dir_fd(fd: c_int, buffer: &mut [u8]) -> DirListing {
    let mut entries: Vec<DirEntryData> = Vec::new();
    loop {
        // REQUEST: RETURNED_ATTRS must be set — the kernel then prefixes
        // every record with the attribute_set_t bitmap actually returned,
        // which is what `parse_bulk_record` gates each field on. ERROR
        // leads the common block; name, type and times follow, then the
        // file sizes (Mac BuildPrompt §4; layout per dua-cli).
        let attrs = AttrList {
            bitmapcount: 5,
            reserved: 0,
            commonattr: ATTR_CMN_RETURNED_ATTRS
                | ATTR_CMN_ERROR
                | ATTR_CMN_NAME
                | ATTR_CMN_OBJTYPE
                | ATTR_CMN_CRTIME
                | ATTR_CMN_MODTIME,
            volattr: 0,
            dirattr: 0,
            fileattr: ATTR_FILE_TOTALSIZE | ATTR_FILE_ALLOCSIZE,
            forkattr: 0,
        };
        // SAFETY: attrs + aligned 256 KiB buffer, both valid for the call.
        let n = unsafe {
            getattrlistbulk(
                fd,
                &attrs as *const AttrList as *mut AttrList,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                FSOPT_NOFOLLOW,
            )
        };
        if n < 0 {
            eprintln!(
                "[mac-engine] getattrlistbulk failed errno={} bitmapcount={} commonattr={:#010x} fileattr={:#010x}",
                errno(),
                attrs.bitmapcount,
                attrs.commonattr,
                attrs.fileattr
            );
            return DirListing {
                entries,
                error: Some(errno_list_error()),
            };
        }
        if n == 0 {
            return DirListing {
                entries,
                error: None,
            };
        }
        // Walk the packed records: each record is a length-prefixed blob
        // of naturally-aligned attribute values in request order.
        let mut off = 0usize;
        let mut parsed = 0usize;
        for _ in 0..n {
            if off + 20 > buffer.len() {
                break;
            }
            let rec_len = u32::from_le_bytes([
                buffer[off],
                buffer[off + 1],
                buffer[off + 2],
                buffer[off + 3],
            ]) as usize;
            if rec_len == 0 || off + rec_len > buffer.len() {
                break;
            }
            let rec = &buffer[off..off + rec_len];
            if let Some(e) = parse_bulk_record(rec) {
                // Skip "." / "..".
                if !(e.name.len() == 1 && e.name[0] == 0x2E)
                    && !(e.name.len() == 2 && e.name[0] == 0x2E && e.name[1] == 0x2E)
                {
                    entries.push(e);
                }
                parsed += 1;
            }
            off += rec_len;
        }
        if parsed == 0 {
            // Every record failed to parse — the layout contract drifted
            // again. Dump enough of the first record to diagnose without
            // flooding stderr (64 bytes).
            let dump: Vec<String> = buffer.iter().take(64).map(|b| format!("{b:02x}")).collect();
            eprintln!(
                "[mac-engine] 0/{n} records parsed (rec_len={} first 64 B: {})",
                u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]),
                dump.join(" ")
            );
        }
    }
}

/// Parse one packed `getattrlistbulk` record, per the kernel's real
/// layout (borrowed from dua-cli, MIT — see the constants comment):
///
/// ```text
/// [u32 total_length]
/// [u32 returned.commonattr][u32 volattr][u32 dirattr]
/// [u32 returned.fileattr][u32 forkattr]          <- 24-byte fixed header
/// [u32 error]            if returned & ATTR_CMN_ERROR
/// [i32 name_offset][u32 name_length]             <- attrreference_t; the
///    name BYTES live at (position of this field) + name_offset, length
///    includes the trailing NUL
/// [u32 objtype]          if returned & ATTR_CMN_OBJTYPE  (VREG/VDIR/VLNK…)
/// [i64 cr_sec, i64 cr_nsec]                      if returned & CRTIME
/// [i64 md_sec, i64 md_nsec]                      if returned & MODTIME
/// [u64 total_size]       if returned & ATTR_FILE_TOTALSIZE
/// [u64 alloc_size]       if returned & ATTR_FILE_ALLOCSIZE
/// ```
///
/// Values pack sequentially in the listed (canonical) order — CRTIME
/// BEFORE MODTIME — with no inter-attribute padding. The returned bitmap
/// (not the request) decides which fields are present; a missing field
/// falls back to its default instead of failing the whole record.
fn parse_bulk_record(rec: &[u8]) -> Option<DirEntryData> {
    if rec.len() < 24 {
        return None;
    }
    // Fixed header: length + the returned attribute_set_t.
    let ret_common = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
    let ret_file = u32::from_le_bytes([rec[16], rec[17], rec[18], rec[19]]);
    let mut o = 24usize;

    // error (u32) — first in the common block.
    if ret_common & ATTR_CMN_ERROR != 0 {
        if o + 4 > rec.len() {
            return None;
        }
        o += 4;
    }

    // name: attrreference_t { i32 attr_dataoffset, u32 attr_length }.
    let mut name: Vec<u16> = Vec::new();
    if ret_common & ATTR_CMN_NAME != 0 {
        if o + 8 > rec.len() {
            return None;
        }
        let reference_offset = o;
        let data_offset = i32::from_le_bytes([rec[o], rec[o + 1], rec[o + 2], rec[o + 3]]) as isize;
        let data_length =
            u32::from_le_bytes([rec[o + 4], rec[o + 5], rec[o + 6], rec[o + 7]]) as usize;
        o += 8;
        let start = reference_offset as isize + data_offset;
        if start < 0 {
            return None;
        }
        let start = start as usize;
        let end = start.checked_add(data_length)?;
        if end > rec.len() {
            return None;
        }
        let mut bytes = &rec[start..end];
        if bytes.last() == Some(&0) {
            bytes = &bytes[..bytes.len() - 1];
        }
        // ATTR_CMN_NAME is a UTF-8 string: decode UTF-8, then encode to
        // the tree's UTF-16. The old byte-widening (`u16::from(b)`)
        // mojibake'd every non-ASCII filename ("café" → "cafÃ©").
        name = String::from_utf8_lossy(bytes).encode_utf16().collect();
    }

    // objtype (u32) — a vnode type VALUE (VREG=1, VDIR=2, VLNK=5…).
    let mut objtype = VNON;
    if ret_common & ATTR_CMN_OBJTYPE != 0 {
        if o + 4 > rec.len() {
            return None;
        }
        objtype = u32::from_le_bytes([rec[o], rec[o + 1], rec[o + 2], rec[o + 3]]);
        o += 4;
    }

    // created timespec {i64 sec, i64 nsec} — BEFORE modified.
    let mut crt_sec = 0i64;
    if ret_common & ATTR_CMN_CRTIME != 0 {
        if o + 16 > rec.len() {
            return None;
        }
        crt_sec = read_i64(&rec[o..]);
        o += 16;
    }

    // modified timespec.
    let mut mod_sec = 0i64;
    if ret_common & ATTR_CMN_MODTIME != 0 {
        if o + 16 > rec.len() {
            return None;
        }
        mod_sec = read_i64(&rec[o..]);
        o += 16;
    }

    // File sizes — the kernel leaves these bits clear for directories.
    let mut logical = 0u64;
    let mut on_disk = 0u64;
    if ret_file & ATTR_FILE_TOTALSIZE != 0 {
        if o + 8 > rec.len() {
            return None;
        }
        logical = read_u64(&rec[o..]);
        o += 8;
    }
    if ret_file & ATTR_FILE_ALLOCSIZE != 0 {
        if o + 8 > rec.len() {
            return None;
        }
        on_disk = read_u64(&rec[o..]);
    }

    let is_dir = objtype == VDIR;
    let is_link = objtype == VLNK;

    Some(DirEntryData {
        name,
        is_dir,
        logical,
        on_disk,
        modified: mod_sec,
        created: crt_sec,
        // Symlinks carry the reparse-tag slot so the scanner's
        // never-descend rule applies unchanged (mac: no cloud tags).
        reparse_tag: is_link.then_some(0xA000_0009),
        cloud: false,
        file_id: 0,
    })
}

fn read_u64(b: &[u8]) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[..8]);
    u64::from_le_bytes(v)
}

fn read_i64(b: &[u8]) -> i64 {
    read_u64(b) as i64
}

/// Mounted volumes: (mount-from, mount-point, label).
fn volume_inventory() -> Vec<(String, String, String)> {
    // SAFETY: count query with a null buffer.
    let count = unsafe {
        getfsstat(std::ptr::null_mut(), 0, 1 /* MNT_NOWAIT */)
    };
    if count <= 0 {
        return Vec::new();
    }
    let mut bufs: Vec<StatFs> = (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
    // SAFETY: buffer sized for the reported count.
    let got = unsafe {
        getfsstat(
            bufs.as_mut_ptr(),
            std::mem::size_of::<StatFs>() as c_int * count,
            1,
        )
    };
    if got <= 0 {
        return Vec::new();
    }
    bufs.truncate(got as usize);
    bufs.iter()
        .map(|st| {
            let cstr = |buf: &[u8]| -> String {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                String::from_utf8_lossy(&buf[..end]).into_owned()
            };
            let from = cstr(&st.f_mntfromname);
            let on = cstr(&st.f_mntonname);
            let label = from.rsplit('/').next().unwrap_or(from.as_str()).to_string();
            (from, on, label)
        })
        .collect()
}

/// Browsable user volumes: the root + /Volumes mounts.
fn is_browsable_volume(mount: &str) -> bool {
    mount == "/" || (mount.starts_with("/Volumes/") && !mount.contains("/."))
}

/// `statfs` on one path (c_str must be NUL-terminated).
fn statfs_of(c_path: &CStr) -> Option<StatFs> {
    let mut st: StatFs = unsafe { std::mem::zeroed() };
    // SAFETY: valid out-struct; c_str NUL-terminated.
    let rc = unsafe { statfs(c_path.as_ptr(), &mut st) };
    (rc == 0).then_some(st)
}

// ─────────────────────────────────────────────────────────────────────
// The shell surface (mirrors win.rs's impl block + free functions).
// ─────────────────────────────────────────────────────────────────────

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

/// Per-volume Trash policy: Finder's Trash always works on writable
/// local volumes (the BitBucket registry concept is Windows-only).
#[derive(Debug, Clone, Copy)]
pub struct BinPolicy {
    /// Always false on macOS (Trash is the only path).
    pub nuke_on_delete: bool,
    /// None — the Finder manages capacity.
    pub max_capacity_mb: Option<u64>,
}

/// The Trash policy for the volume containing `display_path`.
#[must_use]
pub fn bin_policy_for(_display_path: &str) -> BinPolicy {
    BinPolicy {
        nuke_on_delete: false,
        max_capacity_mb: None,
    }
}

/// True when the path sits on a local volume.
#[must_use]
pub fn path_on_fixed_drive(display_path: &str) -> bool {
    let c = match CString::new(display_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    statfs_of(&c)
        .map(|st| st.f_flags & 0x0000_0800 != 0 /* MNT_LOCAL */)
        .unwrap_or(false)
}

/// True when the path no longer exists.
#[must_use]
pub fn path_missing(display_path: &str) -> bool {
    std::fs::symlink_metadata(display_path).is_err()
}

/// No COM on macOS — a no-op guard with the same API.
pub struct ComApartment;

impl ComApartment {
    /// Nothing to initialize on macOS.
    pub fn init() -> Result<Self, String> {
        Ok(ComApartment)
    }
}

/// The Trash move: `NSWorkspace.recycleURLs:completionHandler:` runs
/// asynchronously; we pump the run loop until the handler fires
/// (bounded wait, ≤ 60 s).
pub fn recycle_to_trash(paths: &[String]) -> Result<Vec<(String, Result<(), String>)>, String> {
    unsafe {
        let ws = workspace_shared();
        let urls: Vec<Id> = paths.iter().map(|p| file_url(p)).collect();
        let arr = cf_array_of(&urls);
        let done = std::rc::Rc::new(std::cell::Cell::new(false));
        let errors = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let d2 = done.clone();
        let e2 = errors.clone();
        let handler = Block1::new(move |ns_error: *mut AnyObject| {
            if !ns_error.is_null() {
                // SAFETY: localizedDescription returns an autoreleased
                // NSString (toll-free CFString).
                let desc: Id = unsafe { msg_send![ns_error, localizedDescription] };
                if let Some(s) = unsafe { cf_string_to_string(desc as *const c_void) } {
                    e2.borrow_mut().push(s);
                }
            } else {
                e2.borrow_mut().clear();
            }
            d2.set(true);
        });
        let raw_block = handler as *const c_void;
        // SAFETY: recycleURLs:completionHandler: with our URLs and the
        // escaping block; we pump the run loop until done below.
        let _: () = unsafe { msg_send![ws, recycleURLs: arr, completionHandler: raw_block] };
        let rl: Id = run_loop_current();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !done.get() && std::time::Instant::now() < deadline {
            // SAFETY: runMode:beforeDate: pumps the default mode.
            unsafe { run_loop_run_mode(rl, 0.2) };
        }
        let err_list = errors.borrow().clone();
        if !done.get() {
            return Err("Trash move timed out".into());
        }
        if err_list.is_empty() {
            Ok(paths.iter().map(|p| (p.clone(), Ok(()))).collect())
        } else {
            Ok(paths
                .iter()
                .map(|p| (p.clone(), Err(err_list.join("; "))))
                .collect())
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Storage / elevation / turbo / apps / monitor / licensing surfaces.
// ─────────────────────────────────────────────────────────────────────

/// Volume snapshot for the sidebar ring.
pub struct StorageSnapshot {
    pub label: String16,
    pub total: u64,
    pub used: u64,
    pub free: u64,
}

/// UTF-16 helper mirroring win.rs's String16.
pub struct String16(pub Vec<u16>);

/// Read the storage snapshot for the volume containing `display_path`.
pub fn disk_storage(display_path: &str) -> Option<StorageSnapshot> {
    let c = CString::new(display_path).ok()?;
    let st = statfs_of(&c)?;
    let label = String::from_utf8_lossy(
        &st.f_mntfromname[..st.f_mntfromname.iter().position(|&b| b == 0).unwrap_or(16)],
    )
    .rsplit('/')
    .next()
    .unwrap_or("Macintosh HD")
    .to_string();
    let total = st.f_blocks.saturating_mul(u64::from(st.f_bsize));
    let free = st.f_bavail.saturating_mul(u64::from(st.f_bsize));
    Some(StorageSnapshot {
        label: String16(label.encode_utf16().collect()),
        total,
        used: total.saturating_sub(free),
        free,
    })
}

/// macOS has no elevation prompt flow (Full Disk Access is the grant).
#[must_use]
pub fn is_elevated() -> bool {
    // SAFETY: geteuid has no failure mode.
    unsafe { libc_geteuid() == 0 }
}

/// Relaunching "as administrator" is a Windows concept; the macOS
/// answer is Full Disk Access (the UI shows Open Privacy Settings).
pub fn relaunch_elevated_with(_scan_target: &str, _extra_args: &str) -> Result<(), String> {
    Err("macOS uses Full Disk Access instead of elevation. Open System Settings → Privacy & Security → Full Disk Access and rescan.".into())
}

/// The Turbo engine is NTFS-only by spec (§5); on macOS the standard
/// engine always runs (the existing fallback path explains it). The
/// geometry fields mirror win.rs exactly (u32 ×3 + u64) so the
/// command layer compiles; the constructors on this platform always
/// error before they are read.
#[derive(Debug, Clone, Copy)]
pub struct TurboGeometry {
    pub bytes_per_sector: u32,
    pub bytes_per_cluster: u32,
    pub bytes_per_record: u32,
    pub mft_valid_data_length: u64,
}

#[allow(non_snake_case)]
pub fn turbo_geometry(_drive_root: &str) -> Result<(std::fs::File, TurboGeometry), String> {
    Err("The fast NTFS engine is Windows-only; the standard engine runs on macOS.".into())
}

#[allow(non_snake_case)]
pub fn turbo_read_mft(
    _volume: &mut std::fs::File,
    _geo: &TurboGeometry,
) -> Result<Vec<u8>, String> {
    Err("The fast NTFS engine is Windows-only.".into())
}

#[must_use]
pub fn enable_backup_privilege() -> bool {
    false
}

/// One /Applications app entry (the registry-analogue; Info.plist
/// values). Field names mirror win.rs exactly — the command layer is
/// platform-generic.
#[derive(Debug, Clone, Default)]
pub struct RawRegistryApp {
    /// Bundle identifier — the stable id (win.rs `id`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Publisher subject (bundle id when the plist lacks one).
    pub publisher: String,
    /// `CFBundleShortVersionString`.
    pub version: String,
    /// The `.app` bundle path.
    pub install_location: String,
    /// Uninstall command ("" — mac apps uninstall via Trash).
    pub uninstall_string: String,
    /// Quiet uninstall ("" on mac).
    pub quiet_uninstall_string: String,
    /// Display icon path ("" — the fallback glyph renders).
    pub display_icon: String,
}

/// MSIX has no macOS analogue — the struct mirrors win.rs so the
/// command layer compiles; the list is always empty.
#[derive(Debug, Clone, Default)]
pub struct RawMsixApp {
    pub id: String,
    pub name: String,
    pub publisher: String,
    pub version: String,
    pub install_location: String,
    pub family_name: String,
}

/// Installed apps on macOS: every `.app` bundle under /Applications and
/// ~/Applications (MSIX has no analogue → callers get an empty list).
pub fn registry_uninstall_entries() -> Vec<RawRegistryApp> {
    let mut out = Vec::new();
    let roots = [
        "/Applications".to_string(),
        format!("{}/Applications", std::env::var("HOME").unwrap_or_default()),
    ];
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !name.ends_with(".app") {
                continue;
            }
            let plist = path.join("Contents/Info.plist");
            let (display, version, bundle_id, publisher) = read_info_plist(&plist);
            let display_name = name.trim_end_matches(".app").to_string();
            let stable_id = bundle_id.clone().unwrap_or_else(|| display_name.clone());
            out.push(RawRegistryApp {
                id: stable_id,
                name: display.unwrap_or(display_name),
                publisher,
                version: version.unwrap_or_else(|| "—".into()),
                install_location: path.to_string_lossy().into_owned(),
                uninstall_string: String::new(),
                quiet_uninstall_string: String::new(),
                display_icon: String::new(),
            });
        }
    }
    out
}

/// Parse the handful of keys we need from an Info.plist
/// (CFPropertyList handles XML + binary forms).
fn read_info_plist(
    path: &std::path::Path,
) -> (Option<String>, Option<String>, Option<String>, String) {
    let Ok(data) = std::fs::read(path) else {
        return (None, None, None, String::new());
    };
    // SAFETY: CFData over our byte slice (no copy, valid for the call).
    let cf_data = unsafe { CFDataCreate(std::ptr::null(), data.as_ptr(), data.len() as isize) };
    if cf_data.is_null() {
        return (None, None, None, String::new());
    }
    // SAFETY: kCFPropertyListImmutable = 0; plist from CFData.
    let plist = unsafe {
        CFPropertyListCreateWithData(
            std::ptr::null(),
            cf_data,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    // SAFETY: release the CFData after the plist is materialized.
    unsafe { cf_release(cf_data) };
    if plist.is_null() {
        return (None, None, None, String::new());
    }
    let get_str = |key: &str| -> Option<String> {
        // SAFETY: Create-rule key, read-only dictionary lookup, release.
        unsafe {
            let k = CFStringCreateWithCString(std::ptr::null(), key.as_ptr().cast(), 0x0800_0100);
            let mut v: *const c_void = std::ptr::null();
            let present = CFDictionaryGetValueIfPresent(plist, k, &mut v);
            cf_release(k);
            if present == 0 || v.is_null() {
                return None;
            }
            cf_string_to_string(v)
        }
    };
    let display = get_str("CFBundleDisplayName").or_else(|| get_str("CFBundleName"));
    let version = get_str("CFBundleShortVersionString");
    let bundle = get_str("CFBundleIdentifier");
    let publisher = get_str("CFBundleIdentifier").unwrap_or_default();
    // SAFETY: release the parsed plist.
    unsafe { cf_release(plist) };
    (display, version, bundle, publisher)
}

/// MSIX has no macOS analogue.
pub fn msix_packages() -> Result<Vec<RawMsixApp>, String> {
    Ok(Vec::new())
}

/// No-op on macOS.
pub fn msix_remove_package(_full_name: &str) -> Result<(), String> {
    Err("Store packages are Windows-only.".into())
}

/// Last-used on macOS: Spotlight metadata in a future revision; the
/// honest default is unknown.
pub fn userassist_entries() -> Vec<(String, i64)> {
    Vec::new()
}

/// App icons: NSWorkspace iconForFile → PNG is a follow-up; the
/// fallback glyph renders when unavailable (honest absence, not a fake).
pub fn icon_png_data_url(_icon_path: &str) -> Option<String> {
    None
}

/// Cluster size (statfs f_bsize).
#[must_use]
pub fn cluster_size(path: &str) -> u32 {
    let c = CString::new(path).unwrap_or_default();
    statfs_of(&c).map(|st| st.f_bsize).unwrap_or(0)
}

/// Close running apps whose bundle lives under `dir`
/// (NSRunningApplication termination — the Mac BuildPrompt §10 flow).
pub fn close_processes_under(dir: &str) -> Vec<String> {
    let mut closed = Vec::new();
    unsafe {
        let ws = workspace_shared();
        // SAFETY: runningApplications (NSWorkspaceIncludeOthers) returns
        // an autoreleased NSArray of NSRunningApplication.
        let apps: *const c_void = unsafe { msg_send![ws, runningApplications] };
        let count = unsafe { CFArrayGetCount(apps) };
        for i in 0..count {
            // SAFETY: array index in range.
            let app = unsafe { CFArrayGetValueAtIndex(apps, i) } as *mut AnyObject;
            if app.is_null() {
                continue;
            }
            // SAFETY: bundleURL returns an autoreleased NSURL.
            let bundle_url: Id = unsafe { msg_send![app, bundleURL] };
            if !bundle_url.is_null() {
                if let Some(p) = unsafe { cf_url_path(bundle_url) } {
                    if p.starts_with(dir) {
                        // SAFETY: localizedName returns an autoreleased
                        // NSString.
                        let name: Id = unsafe { msg_send![app, localizedName] };
                        let name_s = unsafe { cf_string_to_string(name as *const c_void) }
                            .unwrap_or_else(|| p.clone());
                        // terminate() asks nicely; forceTerminate after
                        // the caller's wait window when needed.
                        let _term_ok: bool = unsafe { msg_send![app, terminate] };
                        closed.push(name_s);
                    }
                }
            }
        }
    }
    closed
}

/// Run an app's own uninstaller (rare on macOS — most apps are
/// drag-to-trash). Waits for exit like the Windows path.
pub fn launch_and_wait_uninstaller(cmd_line: &str) -> Result<i32, String> {
    let mut parts = cmd_line.split_whitespace();
    let exe = parts.next().unwrap_or_default();
    if exe.is_empty() {
        return Err("No uninstaller on macOS".into());
    }
    let status = std::process::Command::new(exe)
        .args(parts)
        .status()
        .map_err(|e| format!("Couldn't run {exe}: {e}"))?;
    Ok(status.code().unwrap_or(-1))
}

/// The raw monitor sample (mirrors win.rs::RawMonitor).
pub struct RawMonitor {
    pub ticks: CpuTicks,
    pub threads: u32,
    pub processes: u32,
    pub mem_total: u64,
    pub mem_available: u64,
    pub kernel_paged: u64,
    pub kernel_nonpaged: u64,
    pub system_cache: u64,
    pub commit_total: u64,
    pub commit_limit: u64,
    pub compressed_ws: Option<u64>,
    pub net_in: u64,
    pub net_out: u64,
    pub volumes: Vec<VolumeSample>,
    pub procs: Vec<(u32, String, u64, u64, u64)>,
}

/// CPU tick counters via `host_statistics(HOST_CPU_LOAD_INFO)`.
fn cpu_ticks() -> CpuTicks {
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct CpuLoadInfo {
        user: u32,
        system: u32,
        idle: u32,
        nice: u32,
    }
    let mut info = CpuLoadInfo::default();
    let mut count = 4u32;
    // SAFETY: host_statistics with a 4-u32 out-struct.
    let kr = unsafe {
        host_statistics64(
            mach_host_self(),
            HOST_CPU_LOAD_INFO,
            &mut info as *mut CpuLoadInfo as *mut c_void,
            &mut count,
        )
    };
    if kr != KERN_SUCCESS {
        return CpuTicks::default();
    }
    CpuTicks {
        idle: u64::from(info.idle),
        // Kernel time INCLUDES idle (the Windows GetSystemTimes
        // convention the core's pct() math expects).
        kernel: u64::from(info.system) + u64::from(info.idle),
        user: u64::from(info.user) + u64::from(info.nice),
    }
}

/// `sysctl` a u64 value.
fn sysctl_u64(name: &str) -> Option<u64> {
    let c = CString::new(name).ok()?;
    let mut out = 0u64;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: out-buffer sized for u64; name NUL-terminated.
    let rc = unsafe {
        sysctlbyname(
            c.as_ptr(),
            &mut out as *mut u64 as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(out)
}

fn vm_statistics() -> Option<VmStatistics64> {
    let mut vm = VmStatistics64::default();
    let mut count = (std::mem::size_of::<VmStatistics64>() / std::mem::size_of::<u32>()) as u32;
    // SAFETY: out-struct sized to the flavor's count.
    let kr = unsafe {
        host_statistics64(
            mach_host_self(),
            HOST_VM_INFO64,
            &mut vm as *mut VmStatistics64 as *mut c_void,
            &mut count,
        )
    };
    (kr == KERN_SUCCESS).then_some(vm)
}

/// Sum octets over up, non-loopback `en*` interfaces.
fn network_octets() -> (u64, u64) {
    let mut ifap: *mut IfAddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs writes the list head; freed below.
    if unsafe { getifaddrs(&mut ifap) } != 0 {
        return (0, 0);
    }
    let (mut inb, mut outb) = (0u64, 0u64);
    let mut cur = ifap;
    while !cur.is_null() {
        // SAFETY: the list entries are valid between getifaddrs/freeifaddrs.
        let ifa = unsafe { &*cur };
        let name = unsafe { CStr::from_ptr(ifa.ifa_name) }
            .to_string_lossy()
            .into_owned();
        let is_en = name.starts_with("en");
        let is_up = ifa.ifa_flags & 1 /* IFF_UP */ != 0;
        let is_loopback = ifa.ifa_flags & 8 /* IFF_LOOPBACK */ != 0;
        if is_en && is_up && !is_loopback {
            // The statistics live in `ifa_data` (a `struct if_data64`
            // owned by the list), NOT inside `ifa_addr` — the sockaddr_dl
            // carries only the interface name and link-layer address.
            // The old code dug into the sockaddr at a misaligned offset
            // and read name/MAC bytes as counters (garbage + UB); keep
            // the AF_LINK filter so each interface is counted exactly
            // once (an interface appears once per address family).
            let addr = ifa.ifa_addr;
            let is_link = !addr.is_null() && unsafe { (*addr).sa_family } == 18; // AF_LINK
            if is_link && !ifa.ifa_data.is_null() {
                // SAFETY: for AF_LINK entries getifaddrs sets ifa_data to
                // a valid, aligned `struct if_data64`; the list owns it
                // until freeifaddrs below.
                let data = unsafe { &*(ifa.ifa_data as *const IfData) };
                inb = inb.saturating_add(data.ifi_ibytes);
                outb = outb.saturating_add(data.ifi_obytes);
            }
        }
        cur = ifa.ifa_next;
    }
    // SAFETY: matching freeifaddrs.
    unsafe { freeifaddrs(ifap) };
    (inb, outb)
}

/// The process snapshot (proc_listpids + proc_pid_rusage + proc_pidpath).
fn process_snapshot() -> Vec<(u32, String, u64, u64, u64)> {
    // SAFETY: count query with a null buffer.
    let bytes = unsafe {
        proc_listpids(3 /* KERN_PROC_ALL */, 0, std::ptr::null_mut(), 0)
    };
    if bytes <= 0 {
        return Vec::new();
    }
    let mut pids: Vec<i32> = vec![0; (bytes as usize) / 4 + 16];
    // SAFETY: buffer sized from the count query.
    let got = unsafe { proc_listpids(3, 0, pids.as_mut_ptr().cast(), bytes) };
    if got <= 0 {
        return Vec::new();
    }
    let n = (got as usize) / 4;
    let mut out = Vec::new();
    for i in 0..n.min(pids.len()) {
        let pid = pids[i];
        if pid <= 0 {
            continue;
        }
        let name = proc_name(pid);
        let mut ru = RusageInfoV2 {
            ri_uuid: [0; 16],
            ri_user_time: 0,
            ri_system_time: 0,
            ri_child_user_time: 0,
            ri_child_system_time: 0,
            ri_pkg_idle_wkups: 0,
            ri_energy_wkups: 0,
            ri_wired_size: 0,
            ri_resident_size: 0,
            ri_phys_footprint: 0,
            ri_proc_start_abstime: 0,
            ri_proc_exit_abstime: 0,
            ri_child_abstime: 0,
            ri_resident_size_peak: 0,
            ri_phys_footprint_peak: 0,
        };
        // SAFETY: rusage out-struct for this pid (RUSAGE_INFO_V2 flavor).
        let _ = unsafe {
            proc_pid_rusage(
                pid,
                RUSAGE_INFO_V2,
                &mut ru as *mut RusageInfoV2 as *mut c_void,
            )
        };
        out.push((
            pid as u32,
            name,
            // 100 ns units to mirror the Windows kernel/user fields.
            // ri_*_time in rusage_info_v2 is NANOSECONDS — divide by 100
            // (the old `* 10_000` inflated CPU by 10^6, clamped at 100%).
            ru.ri_system_time / 100,
            ru.ri_user_time / 100,
            ru.ri_resident_size,
        ));
    }
    out
}

fn proc_name(pid: i32) -> String {
    let mut buf = [0u8; 1024];
    // SAFETY: proc_pidpath writes a NUL-terminated path into the buffer.
    let len = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if len <= 0 {
        return format!("pid {pid}");
    }
    let path = String::from_utf8_lossy(&buf[..len as usize]).into_owned();
    path.rsplit('/').next().unwrap_or(path.as_str()).to_string()
}

/// One raw machine sample (best effort, mirrors win.rs::monitor_raw).
#[must_use]
pub fn monitor_raw() -> RawMonitor {
    let ticks = cpu_ticks();
    let mem_total = sysctl_u64("hw.memsize").unwrap_or(0);
    let page = sysctl_u64("vm.pagesize").unwrap_or(4096);
    let vm = vm_statistics();
    let (mem_available, compressed) = vm
        .map(|v| {
            let avail = u64::from(v.free_count + v.inactive_count + v.speculative_count) * page;
            let comp = u64::from(v.compressor_page_count) * page;
            (avail, comp)
        })
        .unwrap_or((0, 0));
    let (kernel_paged, kernel_nonpaged) = vm
        .map(|v| {
            (
                u64::from(v.wire_count) * page,
                u64::from(v.purgeable_count) * page,
            )
        })
        .unwrap_or((0, 0));
    let (net_in, net_out) = network_octets();
    let volumes: Vec<VolumeSample> = volume_inventory()
        .into_iter()
        .filter(|(_, mount, _)| is_browsable_volume(mount))
        .map(|(_, mount, label)| {
            let c = CString::new(mount.as_str())
                .unwrap_or_else(|_| CString::new("/").expect("root is NUL-free"));
            let st = statfs_of(&c);
            let (total, free) = st
                .map(|s| {
                    (
                        s.f_blocks * u64::from(s.f_bsize),
                        s.f_bavail * u64::from(s.f_bsize),
                    )
                })
                .unwrap_or((0, 0));
            VolumeSample {
                root: mount,
                label,
                total,
                free,
            }
        })
        .collect();
    let procs = process_snapshot();
    let processes = procs.len() as u32;
    // Threads: not directly exposed; the process count is the honest
    // per-proc metric the table shows.
    RawMonitor {
        ticks,
        threads: processes,
        processes,
        mem_total,
        mem_available,
        kernel_paged,
        kernel_nonpaged,
        system_cache: 0,
        commit_total: mem_total.saturating_sub(mem_available),
        commit_limit: mem_total,
        compressed_ws: (compressed > 0).then_some(compressed),
        net_in,
        net_out,
        volumes,
        procs,
    }
}

// ============================================================================
// License platform surface (the win.rs M10 section mirrored)
// ============================================================================

/// Per-user app-support directory for DiskBytes
/// (`~/Library/Application Support/DiskBytes`, created on demand). `.` when
/// `HOME` is unset (test runners).
#[must_use]
pub fn app_data_dir() -> std::path::PathBuf {
    let base = std::env::var("HOME").map_or_else(
        |_| std::path::PathBuf::from("."),
        |h| {
            std::path::PathBuf::from(h)
                .join("Library")
                .join("Application Support")
        },
    );
    let dir = base.join("DiskBytes");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// MachineGuid analogue: IOPlatformUUID (IOKit).
pub fn machine_guid() -> Option<String> {
    unsafe {
        let name = CString::new("IOPlatformExpertDevice").ok()?;
        // SAFETY: IOServiceMatching returns a CF dictionary consumed by
        // IOServiceGetMatchingService.
        let matching = IOServiceMatching(name.as_ptr());
        if matching.is_null() {
            return None;
        }
        // SAFETY: matching consumed by the lookup (release-on-consume).
        // First argument is kIOMainPortDefault (0) — the master-port
        // parameter is ignored on macOS 12+ and 0 is the documented
        // default (mach_host_self would leak a send right).
        let service = IOServiceGetMatchingService(0, matching);
        if service == 0 {
            return None;
        }
        // SAFETY: Create-rule CFString key.
        let key = CFStringCreateWithCString(
            std::ptr::null(),
            b"IOPlatformUUID\0".as_ptr().cast(),
            0x0800_0100,
        );
        if key.is_null() {
            // SAFETY: release the service on the failure path.
            let _ = IOObjectRelease(service);
            return None;
        }
        // SAFETY: property from the live service object.
        let uuid_cf = IORegistryEntryCreateCFProperty(service, key, std::ptr::null(), 0);
        // SAFETY: release the key after the query.
        cf_release(key);
        let _ = IOObjectRelease(service);
        if uuid_cf.is_null() {
            return None;
        }
        let s = cf_string_to_string(uuid_cf);
        // SAFETY: release the property object.
        cf_release(uuid_cf);
        s
    }
}

/// System-drive serial analogue: the root volume's fsid as u32.
pub fn system_drive_serial() -> Option<u32> {
    let c = CString::new("/").ok()?;
    statfs_of(&c).map(|st| st.f_fsid[0])
}

/// CPU brand via sysctl.
pub fn cpuid_brand() -> Option<String> {
    let name = CString::new("machdep.cpu.brand_string").ok()?;
    let mut buf = [0u8; 128];
    let mut len = buf.len();
    // SAFETY: sysctlbyname into a fixed buffer.
    let rc = unsafe {
        sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    let end = buf[..len].iter().position(|&b| b == 0).unwrap_or(len);
    String::from_utf8(buf[..end].to_vec()).ok()
}

// ─────────────────────────────────────────────────────────────────────
// Keychain persistence (the DPAPI analogue).
// ─────────────────────────────────────────────────────────────────────

const KEYCHAIN_SERVICE: &str = "com.confines.diskbytes";
const KEYCHAIN_ACCOUNT: &str = "license-state";

fn cf_key(name: &str) -> *mut c_void {
    // SAFETY: Create-rule CFString.
    unsafe { CFStringCreateWithCString(std::ptr::null(), name.as_ptr().cast(), 0x0800_0100) }
}

/// Build the SecItem attributes/query dictionary.
fn keychain_dict(
    service: *const c_void,
    account: *const c_void,
    data: *const c_void,
) -> *mut c_void {
    // SAFETY: CFDictionaryCreateMutable + AddValue with Create-rule keys.
    unsafe {
        let class = cf_key("kSecClass");
        let gen_pass = cf_key("kSecClassGenericPassword");
        let service_key = cf_key("kSecAttrService");
        let account_key = cf_key("kSecAttrAccount");
        let d = CFDictionaryCreateMutable(std::ptr::null(), 0, std::ptr::null(), std::ptr::null());
        CFDictionaryAddValue(d, class, gen_pass);
        CFDictionaryAddValue(d, service_key, service);
        CFDictionaryAddValue(d, account_key, account);
        if !data.is_null() {
            let data_key = cf_key("kSecValueData");
            CFDictionaryAddValue(d, data_key, data);
            cf_release(data_key);
        }
        cf_release(class);
        cf_release(gen_pass);
        cf_release(service_key);
        cf_release(account_key);
        d
    }
}

/// Encrypt-and-store (a Keychain "generic password" item; the Keychain
/// itself provides the confidentiality DPAPI gives on Windows).
pub fn dpapi_protect(data: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let service = cf_key(KEYCHAIN_SERVICE);
        let account = cf_key(KEYCHAIN_ACCOUNT);
        // SAFETY: Create-rule CFData over the slice.
        let cf_data = CFDataCreate(std::ptr::null(), data.as_ptr(), data.len() as isize);
        // Replace any existing item first (idempotent upsert).
        let del_query = keychain_dict(service, account, std::ptr::null());
        SecItemDelete(del_query);
        cf_release(del_query);
        let add_attrs = keychain_dict(service, account, cf_data);
        let status = SecItemAdd(add_attrs, std::ptr::null_mut());
        cf_release(add_attrs);
        cf_release(cf_data);
        cf_release(service);
        cf_release(account);
        if status != 0 {
            return Err(format!("Keychain write failed (status {status})"));
        }
        // The stored form is the raw payload (the Keychain is the
        // protection); return it so callers keep one byte contract.
        Ok(data.to_vec())
    }
}

/// Fetch-and-decrypt from the Keychain.
pub fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    // `data` is the caller's fallback blob; when the Keychain holds the
    // item it wins (that IS the persisted state).
    let _ = data;
    unsafe {
        let service = cf_key(KEYCHAIN_SERVICE);
        let account = cf_key(KEYCHAIN_ACCOUNT);
        let query = keychain_dict(service, account, std::ptr::null());
        let return_data_key = cf_key("kSecReturnData");
        // SAFETY: kCFBooleanTrue singleton (immutable).
        let yes = cf_boolean_true() as *mut c_void;
        CFDictionaryAddValue(query, return_data_key, yes);
        let mut result: *const c_void = std::ptr::null();
        let status = SecItemCopyMatching(query, &mut result);
        cf_release(query);
        cf_release(return_data_key);
        cf_release(service);
        cf_release(account);
        if status != 0 || result.is_null() {
            return Err("License state not found in the Keychain".into());
        }
        // SAFETY: CFData accessors on the returned item.
        let len = unsafe { CFDataGetLength(result) };
        let ptr = unsafe { CFDataGetBytePtr(result) };
        let out = if len > 0 {
            // SAFETY: ptr valid for len bytes per the CFData contract.
            unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec()
        } else {
            Vec::new()
        };
        // SAFETY: release the returned item.
        cf_release(result);
        Ok(out)
    }
}

/// POSIX stat() hardlink identity (st_dev, st_ino) — the dupes
/// exclusion on macOS comes free from the filesystem.
pub fn hardlink_identity(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path).ok()?;
    Some((md.dev(), md.ino()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-declared FFI structs must match the published layouts
    /// (the spec's "declare it locally + assert size" rule).
    #[test]
    fn ffi_layout_sizes() {
        assert_eq!(std::mem::size_of::<Timeval>(), 16);
        // arm64 truth (verified on the mac runner): 8+1024×2+16 header
        // and names + f_flags_ext + f_reserved[7] = 2168.
        assert_eq!(std::mem::size_of::<StatFs>(), 2168, "statfs 64-bit layout");
        assert_eq!(std::mem::align_of::<StatFs>(), 8);
        // repr(C) pads ifa_flags→ifa_addr to 8: 8+8+4(+4)+8×4 = 56.
        assert_eq!(std::mem::size_of::<IfAddrs>(), 56);
        assert_eq!(std::mem::size_of::<AttrList>(), 24);
        assert_eq!(std::mem::size_of::<BlockLiteral>(), 32);
        assert_eq!(std::mem::align_of::<BlockLiteral>(), 8);
    }

    /// A REAL-shaped `getattrlistbulk` record (the layout the kernel
    /// actually emits, per dua-cli): 24-byte header (length + returned
    /// attribute_set_t), sequential values, the name referenced by an
    /// attrreference at the END of the record.
    fn build_record(
        name: &[u8],
        objtype: u32,
        requested_common: u32,
        requested_file: u32,
        logical: u64,
        alloc: u64,
    ) -> Vec<u8> {
        let mut rec = Vec::new();
        let mut cursor = 24usize; // header: length u32 + attribute_set_t (5×u32)
                                  // error u32 (always requested by the engine)
        if requested_common & ATTR_CMN_ERROR != 0 {
            cursor += 4;
        }
        // attrreference_t { i32 offset, u32 length } — points FORWARD to
        // the name bytes; the kernel places them after the fixed values.
        let name_ref_at = cursor;
        if requested_common & ATTR_CMN_NAME != 0 {
            cursor += 8;
        }
        let objtype_at = cursor;
        if requested_common & ATTR_CMN_OBJTYPE != 0 {
            cursor += 4;
        }
        let cr_at = cursor;
        if requested_common & ATTR_CMN_CRTIME != 0 {
            cursor += 16;
        }
        let md_at = cursor;
        if requested_common & ATTR_CMN_MODTIME != 0 {
            cursor += 16;
        }
        let total_at = cursor;
        if requested_file & ATTR_FILE_TOTALSIZE != 0 {
            cursor += 8;
        }
        let alloc_at = cursor;
        if requested_file & ATTR_FILE_ALLOCSIZE != 0 {
            cursor += 8;
        }
        let name_at = cursor; // variable data after the fixed values
        let padded_name_len = name.len().div_ceil(4) * 4; // NUL + pad to 4
        let total = name_at + padded_name_len;

        rec.resize(total, 0);
        // header
        rec[0..4].copy_from_slice(&(total as u32).to_le_bytes());
        rec[4..8].copy_from_slice(&requested_common.to_le_bytes());
        rec[8..12].copy_from_slice(&0u32.to_le_bytes()); // volattr
        rec[12..16].copy_from_slice(&0u32.to_le_bytes()); // dirattr
        rec[16..20].copy_from_slice(&requested_file.to_le_bytes());
        rec[20..24].copy_from_slice(&0u32.to_le_bytes()); // forkattr
                                                          // values
        if requested_common & ATTR_CMN_ERROR != 0 {
            rec[name_ref_at - 4..name_ref_at].copy_from_slice(&0u32.to_le_bytes());
        }
        if requested_common & ATTR_CMN_NAME != 0 {
            let data_offset = (name_at as isize) - (name_ref_at as isize);
            rec[name_ref_at..name_ref_at + 4].copy_from_slice(&(data_offset as i32).to_le_bytes());
            rec[name_ref_at + 4..name_ref_at + 8]
                .copy_from_slice(&((name.len() + 1) as u32).to_le_bytes()); // includes NUL
            rec[name_at..name_at + name.len()].copy_from_slice(name);
            rec[name_at + name.len()] = 0; // NUL terminator
        }
        if requested_common & ATTR_CMN_OBJTYPE != 0 {
            rec[objtype_at..objtype_at + 4].copy_from_slice(&objtype.to_le_bytes());
        }
        if requested_common & ATTR_CMN_CRTIME != 0 {
            rec[cr_at..cr_at + 8].copy_from_slice(&2i64.to_le_bytes()); // cr sec
            rec[cr_at + 8..cr_at + 16].copy_from_slice(&0i64.to_le_bytes());
        }
        if requested_common & ATTR_CMN_MODTIME != 0 {
            rec[md_at..md_at + 8].copy_from_slice(&1i64.to_le_bytes()); // mod sec
            rec[md_at + 8..md_at + 16].copy_from_slice(&0i64.to_le_bytes());
        }
        if requested_file & ATTR_FILE_TOTALSIZE != 0 {
            rec[total_at..total_at + 8].copy_from_slice(&logical.to_le_bytes());
        }
        if requested_file & ATTR_FILE_ALLOCSIZE != 0 {
            rec[alloc_at..alloc_at + 8].copy_from_slice(&alloc.to_le_bytes());
        }
        rec
    }

    const REQ_COMMON: u32 = ATTR_CMN_RETURNED_ATTRS
        | ATTR_CMN_ERROR
        | ATTR_CMN_NAME
        | ATTR_CMN_OBJTYPE
        | ATTR_CMN_CRTIME
        | ATTR_CMN_MODTIME;
    const REQ_FILE: u32 = ATTR_FILE_TOTALSIZE | ATTR_FILE_ALLOCSIZE;

    #[test]
    fn bulk_record_parses_a_directory() {
        let rec = build_record(b"abc", VDIR, REQ_COMMON, REQ_FILE, 0, 0);
        let e = parse_bulk_record(&rec).expect("record parses");
        assert_eq!(
            e.name,
            vec![u16::from(b'a'), u16::from(b'b'), u16::from(b'c')]
        );
        assert!(e.is_dir);
        assert!(e.reparse_tag.is_none());
        assert_eq!(e.modified, 1); // modtime AFTER crtime in the record
        assert_eq!(e.created, 2);
        // Directories: the kernel leaves the file-size bits clear.
        assert_eq!(e.logical, 0);
        assert_eq!(e.on_disk, 0);
    }

    #[test]
    fn bulk_record_parses_a_file_with_sizes() {
        let rec = build_record(b"video.mp4", VREG, REQ_COMMON, REQ_FILE, 1_100, 1_100);
        let e = parse_bulk_record(&rec).expect("record parses");
        assert_eq!(e.name, "video.mp4".encode_utf16().collect::<Vec<u16>>());
        assert!(!e.is_dir);
        assert_eq!(e.logical, 1_100);
        assert_eq!(e.on_disk, 1_100);
        assert_eq!(e.modified, 1);
        assert_eq!(e.created, 2);
    }

    #[test]
    fn bulk_record_parses_a_symlink_with_reparse_tag() {
        let rec = build_record(b"alias", VLNK, REQ_COMMON, REQ_FILE, 7, 7);
        let e = parse_bulk_record(&rec).expect("record parses");
        assert!(e.reparse_tag.is_some(), "symlinks never descend");
        assert!(e.name == "alias".encode_utf16().collect::<Vec<u16>>());
    }

    #[test]
    fn bulk_record_survives_partial_attribute_sets() {
        // The returned bitmap — not the request — decides the layout: a
        // record missing CRTIME + both sizes must still parse.
        let partial_common = ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_ERROR
            | ATTR_CMN_NAME
            | ATTR_CMN_OBJTYPE
            | ATTR_CMN_MODTIME;
        let rec = build_record(b"x.txt", VREG, partial_common, 0, 0, 0);
        let e = parse_bulk_record(&rec).expect("partial record parses");
        assert_eq!(e.created, 0);
        assert_eq!(e.logical, 0);
        assert_eq!(e.on_disk, 0);
        assert_eq!(e.modified, 1);
    }

    #[test]
    fn bulk_record_rejects_truncated_header() {
        assert!(parse_bulk_record(&[0u8; 16]).is_none());
    }

    /// THE REAL SYSCALL, end to end: stage a directory with known files,
    /// run `MacPlatform::list_dir` (open + getattrlistbulk + parser) and
    /// assert the entries. The parser tests use synthetic records — this
    /// one catches kernel-contract drift (the mac UI sat at "0 B" for
    /// three CI rounds while the unit tests stayed green). Runs on the
    /// mac CI host; a no-op assertion on other platforms is unnecessary
    /// (the test is mac-gated with the module).
    #[test]
    fn real_getattrlistbulk_enumerates_a_staged_directory() {
        let dir = std::env::temp_dir().join(format!(
            "db-bulk-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).expect("stage dirs");
        std::fs::write(dir.join("alpha.txt"), b"hello world").expect("stage alpha");
        std::fs::write(dir.join("beta.bin"), [0u8; 4096]).expect("stage beta");
        std::fs::write(sub.join("gamma.log"), b"12345678").expect("stage gamma");
        // Non-ASCII names (the mojibake regression: ATTR_CMN_NAME is
        // UTF-8; the old byte-widening turned "café" into "cafÃ©" and
        // mangled every supplementary-plane name).
        std::fs::write(dir.join("café.txt"), b"accents").expect("stage café");
        std::fs::write(sub.join("日本語.md"), b"cjk").expect("stage 日本語");
        std::fs::write(sub.join("emoji-📁.txt"), b"non-bmp").expect("stage emoji");
        let listing = MacPlatform.list_dir(&dir.to_string_lossy());
        assert!(listing.error.is_none(), "engine error: {:?}", listing.error);
        let names: Vec<String> = listing
            .entries
            .iter()
            .map(|e| String::from_utf16_lossy(&e.name))
            .collect();
        assert!(names.contains(&"alpha.txt".to_string()), "names: {names:?}");
        assert!(names.contains(&"beta.bin".to_string()), "names: {names:?}");
        assert!(names.contains(&"sub".to_string()), "names: {names:?}");
        assert!(
            names.contains(&"café.txt".to_string()),
            "mojibake regression (BMP): {names:?}"
        );
        assert!(
            names.contains(&"日本語.md".to_string()),
            "mojibake regression (CJK): {names:?}"
        );
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("emoji-") && n.ends_with(".txt")),
            "mojibake regression (non-BMP surrogate pair): {names:?}"
        );
        let beta = listing
            .entries
            .iter()
            .find(|e| e.name == "beta.bin".encode_utf16().collect::<Vec<u16>>())
            .expect("beta entry");
        assert!(!beta.is_dir);
        assert!(beta.logical >= 4096, "logical: {}", beta.logical);
        assert!(beta.on_disk >= 4096, "on_disk: {}", beta.on_disk);
        assert!(beta.modified > 0, "mtime: {}", beta.modified);
        let sub_e = listing
            .entries
            .iter()
            .find(|e| e.name == "sub".encode_utf16().collect::<Vec<u16>>())
            .expect("sub entry");
        assert!(sub_e.is_dir, "sub must be a directory");
        // Deliberately no direct-delete cleanup here: the R7.1 grep bans
        // those APIs anywhere in src-tauri/src (tests included) — and the
        // staged dir lives in $TMPDIR, which the OS reaps.
    }

    #[test]
    fn known_folders_resolve_under_home() {
        let home = std::env::var("HOME").unwrap_or_default();
        let platform = MacPlatform;
        assert_eq!(
            platform.known_folder(KnownFolder::Profile).as_deref(),
            Some(home.as_str())
        );
        assert!(platform.known_folder(KnownFolder::LocalAppData).is_some());
    }

    #[test]
    fn trash_policy_is_always_recyclable() {
        let p = bin_policy_for("/Users/dev/anything");
        assert!(!p.nuke_on_delete);
        assert!(p.max_capacity_mb.is_none());
    }
}
