//! ObjC / CoreFoundation runtime helpers: NSString bridging, file
//! URLs, arrays, run-loop pumping, Block1 trampoline.

use std::ffi::{c_void, CStr, CString};

use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject};

use super::ffi::{
    Boolean, CFArrayAppendValue, CFArrayCreateMutable, CFRelease, CFStringCreateWithCString,
    CFStringGetCString, CFStringGetLength, CFURLCreateWithFileSystemPath,
};

// ─────────────────────────────────────────────────────────────────────
// ObjC runtime (objc2) — raw id + class lookups, toll-free CF bridging.
// ─────────────────────────────────────────────────────────────────────

/// An ObjC object pointer (the C ABI id).
pub(crate) type Id = *mut AnyObject;

pub(crate) unsafe fn ns_string(s: &str) -> Id {
    // SAFETY: Create-rule CFString (toll-free NSString); NUL-free input.
    let c = CString::new(s).expect("NUL-free string");
    let cf = unsafe {
        CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), 0x0800_0100 /* UTF8 */)
    };
    assert!(!cf.is_null(), "CFStringCreateWithCString failed");
    cf as Id
}

pub(crate) unsafe fn cf_string_to_string(cf: *const c_void) -> Option<String> {
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

pub(crate) unsafe fn cf_release(cf: *const c_void) {
    if !cf.is_null() {
        // SAFETY: CFRelease on a non-null CF object we own (or borrowed
        // briefly for reads — balanced by the Create rules above).
        unsafe { CFRelease(cf) };
    }
}

pub(crate) unsafe fn workspace_shared() -> Id {
    let class = AnyClass::get("NSWorkspace").expect("NSWorkspace class");
    // SAFETY: sharedWorkspace returns the singleton (no ownership).
    unsafe { msg_send![class, sharedWorkspace] }
}

pub(crate) unsafe fn open_config_default() -> Id {
    // NSWorkspaceOpenConfiguration (macOS 10.15+) — default instance.
    let class =
        AnyClass::get("NSWorkspaceOpenConfiguration").expect("NSWorkspaceOpenConfiguration");
    // SAFETY: new returns an owned instance (autoreleased suffices here).
    unsafe { msg_send![class, new] }
}

pub(crate) unsafe fn file_url(path: &str) -> Id {
    let s = ns_string(path);
    // SAFETY: CFURLCreateWithFileSystemPath (Create rule) + toll-free
    // NSURL bridge; kCFURLPOSIXPathStyle = 0.
    let cf = unsafe { CFURLCreateWithFileSystemPath(std::ptr::null(), s as *const c_void, 0, 1) };
    assert!(!cf.is_null(), "CFURLCreate failed");
    cf as Id
}

pub(crate) unsafe fn cf_array_of(items: &[Id]) -> *mut c_void {
    // SAFETY: CFArrayCreateMutable (Create rule) with no callbacks.
    let arr = unsafe { CFArrayCreateMutable(std::ptr::null(), 0, std::ptr::null()) };
    for it in items {
        // SAFETY: appending retained CF objects.
        unsafe { CFArrayAppendValue(arr, *it as *const c_void) };
    }
    arr
}

pub(crate) unsafe fn run_loop_current() -> Id {
    let class = AnyClass::get("NSRunLoop").expect("NSRunLoop");
    // SAFETY: currentRunLoop returns the thread's loop (no ownership).
    unsafe { msg_send![class, currentRunLoop] }
}

pub(crate) unsafe fn run_loop_run_mode(rl: Id, seconds: f64) {
    let mode = ns_string("kCFRunLoopDefaultMode");
    let date_class = AnyClass::get("NSDate").expect("NSDate");
    // SAFETY: an autoreleased date `seconds` from now.
    let date: Id = unsafe { msg_send![date_class, dateWithTimeIntervalSinceNow: seconds] };
    // SAFETY: runMode:beforeDate: on the live run loop.
    let _: Boolean = unsafe { msg_send![rl, runMode: mode, beforeDate: date] };
}

pub(crate) unsafe fn cf_url_path(url: Id) -> Option<String> {
    // SAFETY: path on a CFURL returns an autoreleased CFString.
    let path: Id = unsafe { msg_send![url, path] };
    unsafe { cf_string_to_string(path as *const c_void) }
}

// ─────────────────────────────────────────────────────────────────────
// The block runtime (one escaping `void (^)(id)` shape).
// ─────────────────────────────────────────────────────────────────────

/// The clang Block_literal ABI.
#[repr(C)]
pub(crate) struct BlockLiteral {
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
pub(crate) struct Block1<F: Fn(*mut AnyObject)> {
    literal: BlockLiteral,
    keep_alive: Box<F>,
}

impl<F: Fn(*mut AnyObject)> Block1<F> {
    /// Build an escaping block (leaked; one commit per queue, bounded).
    pub(crate) fn new(f: F) -> *const Block1<F> {
        let this = Box::into_raw(Box::new(Block1 {
            literal: BlockLiteral {
                isa: block_isa(),
                flags: 1 << 24, // BLOCK_IS_GLOBAL (no copy/dispose needed)
                reserved: 0,
                invoke: Self::trampoline,
                descriptor: &BLOCK_DESCRIPTOR as *const BlockDescriptor as *const c_void,
            },
            keep_alive: Box::new(f),
        }));
        this as *const Block1<F>
    }

    unsafe extern "C" fn trampoline(lit: *const BlockLiteral, arg: *mut AnyObject) {
        // SAFETY: `lit` points at the `literal` field (offset 0) of a
        // leaked Block1 composite; the cast recovers the composite.
        let this = lit as *const Block1<F>;
        // SAFETY: the composite is immortal (leaked) and this
        // trampoline was created from that exact allocation.
        let f = unsafe { &(*this).keep_alive };
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
