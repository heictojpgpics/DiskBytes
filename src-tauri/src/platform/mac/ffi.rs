//! Hand-declared FFI surface: libc/ Mach structs, extern blocks,
//! getattrlist constants. Verbatim move from the old mac.rs — the
//! struct fields are `pub(crate)` because the split submodules
//! (dir/monitor/sysinfo/license) read them directly; in the old
//! monolith everything shared one module scope.
#![allow(clippy::upper_case_acronyms)]

use std::ffi::{c_char, c_int, c_void};

/// Security.framework `OSStatus`.
pub(crate) type OSStatus = i32;
/// CoreFoundation `Boolean`.
pub(crate) type Boolean = u8;
/// Mach host port.
pub(crate) type MachPort = u32;
/// Mach host_t.
pub(crate) type HostT = u32;
/// Mach kern_return_t.
pub(crate) type KernReturn = i32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Timeval {
    pub(crate) tv_sec: i64,
    pub(crate) tv_usec: i32,
}

/// `struct attrlist` for `getattrlist(2)`.
#[repr(C)]
pub(crate) struct AttrList {
    pub(crate) bitmapcount: u16,
    pub(crate) reserved: u16,
    pub(crate) commonattr: u32,
    pub(crate) volattr: u32,
    pub(crate) dirattr: u32,
    pub(crate) fileattr: u32,
    pub(crate) forkattr: u32,
}

extern "C" {
    // getattrlistbulk(2) — the directory engine.
    pub(crate) fn getattrlistbulk(
        fd: c_int,
        attrlist: *mut AttrList,
        buffer: *mut c_void,
        buffersize: usize,
        options: u64,
    ) -> c_int;

    // statfs / getfsstat — volume geometry + inventory.
    pub(crate) fn statfs(path: *const c_char, buf: *mut StatFs) -> c_int;
    pub(crate) fn getfsstat(buf: *mut StatFs, bufsize: c_int, flags: c_int) -> c_int;

    // sysctl — CPU brand, memory size.
    pub(crate) fn sysctlbyname(
        name: *const c_char,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> c_int;

    // Mach — CPU tick counters + VM stats.
    pub(crate) fn host_statistics64(
        host: HostT,
        flavor: c_int,
        host_info64: *mut c_void,
        host_info64Cnt: *mut u32,
    ) -> KernReturn;
    pub(crate) fn mach_host_self() -> MachPort;

    // getifaddrs — network octet counters.
    pub(crate) fn getifaddrs(ifap: *mut *mut IfAddrs) -> c_int;
    pub(crate) fn freeifaddrs(ifa: *mut IfAddrs);

    // libproc — process inventory + usage.
    pub(crate) fn proc_listpids(
        kind: u32,
        typeinfo: u32,
        buffer: *mut c_void,
        buffersize: c_int,
    ) -> c_int;
    pub(crate) fn proc_pidpath(pid: i32, buffer: *mut c_void, buffersize: u32) -> c_int;
    pub(crate) fn proc_pid_rusage(pid: i32, flavor: c_int, buffer: *mut c_void) -> c_int;
}

// IOKit is a separate framework (not part of libSystem) — it needs its own
// linked extern block or the IOPlatformUUID symbols fail to resolve at link
// time.
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    // IOKit — the IOPlatformUUID hardware id.
    pub(crate) fn IOServiceGetMatchingService(main_port: MachPort, matching: *const c_void) -> u32;
    pub(crate) fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    pub(crate) fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: *const c_void,
        allocator: *const c_void,
        options: u32,
    ) -> *mut c_void;
    pub(crate) fn IOObjectRelease(object: u32) -> KernReturn;
}

extern "C" {
    // Security.framework — Keychain (SecItem, the DPAPI analogue).
    pub(crate) fn SecItemAdd(attributes: *const c_void, result: *mut *const c_void) -> OSStatus;
    pub(crate) fn SecItemCopyMatching(query: *const c_void, result: *mut *const c_void)
        -> OSStatus;
    pub(crate) fn SecItemDelete(query: *const c_void) -> OSStatus;

    // CoreFoundation — toll-free bridged to Foundation objects.
    pub(crate) fn CFStringCreateWithCString(
        alloc: *const c_void,
        c_str: *const c_char,
        encoding: u32,
    ) -> *mut c_void;
    pub(crate) fn CFStringGetCString(
        the_string: *const c_void,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> Boolean;
    pub(crate) fn CFStringGetLength(the_string: *const c_void) -> isize;
    pub(crate) fn CFDataGetBytePtr(the_data: *const c_void) -> *const u8;
    pub(crate) fn CFDataGetLength(the_data: *const c_void) -> isize;
    pub(crate) fn CFURLCreateWithFileSystemPath(
        alloc: *const c_void,
        file_path: *const c_void,
        path_style: isize,
        is_directory: Boolean,
    ) -> *mut c_void;
    pub(crate) fn CFArrayCreateMutable(
        alloc: *const c_void,
        capacity: isize,
        callbacks: *const c_void,
    ) -> *mut c_void;
    pub(crate) fn CFArrayAppendValue(the_array: *const c_void, value: *const c_void);
    pub(crate) fn CFArrayGetCount(the_array: *const c_void) -> isize;
    pub(crate) fn CFArrayGetValueAtIndex(the_array: *const c_void, idx: isize) -> *const c_void;
    pub(crate) fn CFDictionaryCreateMutable(
        alloc: *const c_void,
        capacity: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *mut c_void;
    pub(crate) fn CFDictionaryAddValue(
        the_dict: *const c_void,
        key: *const c_void,
        value: *const c_void,
    );
    pub(crate) fn CFDictionaryGetValueIfPresent(
        the_dict: *const c_void,
        key: *const c_void,
        value: *mut *const c_void,
    ) -> Boolean;
    pub(crate) fn CFRelease(cf: *const c_void);
    pub(crate) fn CFPropertyListCreateWithData(
        alloc: *const c_void,
        data: *const c_void,
        options: c_int,
        format: *mut c_int,
        error: *mut *const c_void,
    ) -> *mut c_void;
    pub(crate) fn CFDataCreate(
        alloc: *const c_void,
        bytes: *const u8,
        length: isize,
    ) -> *mut c_void;
    // AppKit constants bridged through CF.
    pub(crate) fn NSPasteboardTypeString() -> *const c_void;
    #[link_name = "kCFBooleanTrue"]
    pub(crate) fn cf_boolean_true() -> *const c_void;

    // libc pass-throughs with explicit link names.
    #[link_name = "open"]
    pub(crate) fn libc_open(path: *const c_char, flags: c_int, ...) -> c_int;
    #[link_name = "close"]
    pub(crate) fn libc_close(fd: c_int) -> c_int;
    #[link_name = "__error"]
    pub(crate) fn libc_errno() -> *mut c_int;
    #[link_name = "geteuid"]
    pub(crate) fn libc_geteuid() -> u32;
}

/// `struct statfs` (the published macOS layout, 64-bit).
#[repr(C)]
pub(crate) struct StatFs {
    pub(crate) f_bsize: u32,
    pub(crate) f_iosize: i32,
    pub(crate) f_blocks: u64,
    pub(crate) f_bfree: u64,
    pub(crate) f_bavail: u64,
    pub(crate) f_files: u64,
    pub(crate) f_ffree: u64,
    pub(crate) f_fsid: [u32; 2],
    pub(crate) f_owner: u32,
    pub(crate) f_type: u32,
    pub(crate) f_flags: u32,
    pub(crate) f_fssubtype: u32,
    pub(crate) f_fstypename: [u8; 16],
    pub(crate) f_mntonname: [u8; 1024],
    pub(crate) f_mntfromname: [u8; 1024],
    pub(crate) f_flags2: u32,
    pub(crate) f_reserved: [u32; 7],
}

/// `struct ifaddrs` (published layout, 64-bit).
#[repr(C)]
pub(crate) struct IfAddrs {
    pub(crate) ifa_next: *mut IfAddrs,
    pub(crate) ifa_name: *mut c_char,
    pub(crate) ifa_flags: u32,
    pub(crate) ifa_addr: *mut IfSockaddr,
    pub(crate) ifa_netmask: *mut IfSockaddr,
    pub(crate) ifa_dstaddr: *mut IfSockaddr,
    pub(crate) ifa_data: *mut c_void,
}

/// The first 8 bytes of any `struct sockaddr_*`.
#[repr(C)]
pub(crate) struct IfSockaddr {
    pub(crate) sa_len: u8,
    pub(crate) sa_family: u8,
    pub(crate) sa_data: [u8; 6],
}

/// `struct if_data64` (in the AF_LINK if_data area).
#[repr(C)]
pub(crate) struct IfData {
    pub(crate) ifi_type: u8,
    pub(crate) ifi_typelen: u8,
    pub(crate) ifi_physical: u8,
    pub(crate) ifi_addrlen: u8,
    pub(crate) ifi_hdrlen: u8,
    pub(crate) ifi_recvquota: u8,
    pub(crate) ifi_xmitquota: u8,
    pub(crate) ifi_unused1: u8,
    pub(crate) ifi_mtu: u32,
    pub(crate) ifi_metric: u32,
    pub(crate) ifi_baudrate: u64,
    pub(crate) ifi_ipackets: u64,
    pub(crate) ifi_ierrors: u64,
    pub(crate) ifi_opackets: u64,
    pub(crate) ifi_oerrors: u64,
    pub(crate) ifi_collisions: u64,
    pub(crate) ifi_ibytes: u64,
    pub(crate) ifi_obytes: u64,
    pub(crate) ifi_imcasts: u64,
    pub(crate) ifi_omcasts: u64,
    pub(crate) ifi_iqdrops: u64,
    pub(crate) ifi_noproto: u64,
    pub(crate) ifi_recvtiming: u32,
    pub(crate) ifi_xmittiming: u32,
    pub(crate) ifi_lastchange: Timeval,
}

/// `struct rusage_info_v2`.
#[repr(C)]
pub(crate) struct RusageInfoV2 {
    pub(crate) ri_uuid: [u8; 16],
    pub(crate) ri_user_time: u64,
    pub(crate) ri_system_time: u64,
    pub(crate) ri_child_user_time: u64,
    pub(crate) ri_child_system_time: u64,
    pub(crate) ri_pkg_idle_wkups: u64,
    pub(crate) ri_energy_wkups: u64,
    pub(crate) ri_wired_size: u64,
    pub(crate) ri_resident_size: u64,
    pub(crate) ri_phys_footprint: u64,
    pub(crate) ri_proc_start_abstime: u64,
    pub(crate) ri_proc_exit_abstime: u64,
    pub(crate) ri_child_abstime: u64,
    pub(crate) ri_resident_size_peak: u64,
    pub(crate) ri_phys_footprint_peak: u64,
}

/// VM stats via `host_statistics64(HOST_VM_INFO64)`.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub(crate) struct VmStatistics64 {
    pub(crate) free_count: u32,
    pub(crate) active_count: u32,
    pub(crate) inactive_count: u32,
    pub(crate) wire_count: u32,
    pub(crate) zero_fill_count: u32,
    pub(crate) reactivations: u32,
    pub(crate) pageins: u32,
    pub(crate) pageouts: u32,
    pub(crate) faults: u32,
    pub(crate) cow_faults: u32,
    pub(crate) lookups: u32,
    pub(crate) hits: u32,
    pub(crate) purges: u32,
    pub(crate) purgeable_count: u32,
    pub(crate) speculative_count: u32,
    pub(crate) decompressions: u32,
    pub(crate) compressions: u32,
    pub(crate) swapins: u32,
    pub(crate) swapouts: u32,
    pub(crate) compressor_page_count: u32,
    pub(crate) total_uncompressed_pages_in_compressor: u32,
}

pub(crate) const HOST_VM_INFO64: c_int = 4;
pub(crate) const HOST_CPU_LOAD_INFO: c_int = 3;
pub(crate) const RUSAGE_INFO_V2: c_int = 2;
pub(crate) const KERN_SUCCESS: KernReturn = 0;
pub(crate) const FSOPT_NOFOLLOW: u64 = 0x0000_0001;

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
pub(crate) const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
pub(crate) const ATTR_CMN_ERROR: u32 = 0x2000_0000;
pub(crate) const ATTR_CMN_NAME: u32 = 0x0000_0001;
pub(crate) const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
pub(crate) const ATTR_CMN_CRTIME: u32 = 0x0000_0200;
pub(crate) const ATTR_CMN_MODTIME: u32 = 0x0000_0400;

pub(crate) const ATTR_FILE_TOTALSIZE: u32 = 0x0000_0002;
pub(crate) const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;

// <sys/vnode.h> vnode types — VALUES, not st_mode masks (the old
// `(objtype & 0xF000) == VDIR` could never match: VDIR is 2).
pub(crate) const VNON: u32 = 0;
#[cfg_attr(not(test), allow(dead_code))] // parsed value; tests exercise it
pub(crate) const VREG: u32 = 1;
pub(crate) const VDIR: u32 = 2;
pub(crate) const VLNK: u32 = 5;
