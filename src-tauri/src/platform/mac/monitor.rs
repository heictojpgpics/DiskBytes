//! Raw machine sampling: CPU ticks, memory, network, processes.

use std::ffi::{c_void, CStr, CString};

use diskbytes_core::monitor::{CpuTicks, VolumeSample};

use super::dir::{is_browsable_volume, statfs_of, volume_inventory};
use super::ffi::{
    freeifaddrs, getifaddrs, host_statistics64, mach_host_self, proc_listpids, proc_pid_rusage,
    proc_pidpath, sysctlbyname, IfAddrs, IfData, RusageInfoV2, VmStatistics64, HOST_CPU_LOAD_INFO,
    HOST_VM_INFO64, KERN_SUCCESS, RUSAGE_INFO_V2,
};

/// The raw monitor sample (mirrors win.rs::RawMonitor).
#[allow(clippy::struct_field_names)] // field names mirror win.rs exactly — the command layer is platform-generic
pub struct RawMonitor {
    /// CPU tick counters (`host_statistics64`).
    pub ticks: CpuTicks,
    /// Thread count (macOS exposes the process count honestly instead).
    pub threads: u32,
    /// Process count.
    pub processes: u32,
    /// Physical memory total (`hw.memsize`).
    pub mem_total: u64,
    /// Free + inactive + speculative pages × page size.
    pub mem_available: u64,
    /// Wired pages × page size.
    pub kernel_paged: u64,
    /// Purgeable pages × page size.
    pub kernel_nonpaged: u64,
    /// Always 0 (no direct analogue).
    pub system_cache: u64,
    /// mem_total − mem_available.
    pub commit_total: u64,
    /// mem_total.
    pub commit_limit: u64,
    /// Compressor pages × page size (Some when > 0).
    pub compressed_ws: Option<u64>,
    /// `en*` interface octets in.
    pub net_in: u64,
    /// `en*` interface octets out.
    pub net_out: u64,
    /// Browsable volume samples.
    pub volumes: Vec<VolumeSample>,
    /// (pid, name, kernel 100-ns, user 100-ns, resident bytes).
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
    for &pid in pids.iter().take(n) {
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

///
/// # Panics
/// Never — the buffer is fixed-size and the bounds checked.
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
///
/// # Panics
/// Never in practice: the CString/Mutex fallbacks cover every input;
/// the raw FFI calls have no panic paths (documented in-body).
#[must_use]
pub fn monitor_raw() -> RawMonitor {
    let ticks = cpu_ticks();
    let mem_total = sysctl_u64("hw.memsize").unwrap_or(0);
    let page = sysctl_u64("vm.pagesize").unwrap_or(4096);
    let vm = vm_statistics();
    let (mem_available, compressed) = vm.map_or((0, 0), |v| {
        let avail = u64::from(v.free_count + v.inactive_count + v.speculative_count) * page;
        let comp = u64::from(v.compressor_page_count) * page;
        (avail, comp)
    });
    let (kernel_paged, kernel_nonpaged) = vm.map_or((0, 0), |v| {
        (
            u64::from(v.wire_count) * page,
            u64::from(v.purgeable_count) * page,
        )
    });
    let (net_in, net_out) = network_octets();
    let volumes: Vec<VolumeSample> = volume_inventory()
        .into_iter()
        .filter(|(_, mount, _)| is_browsable_volume(mount))
        .map(|(_, mount, label)| {
            let c = CString::new(mount.as_str())
                .unwrap_or_else(|_| CString::new("/").expect("root is NUL-free"));
            let st = statfs_of(&c);
            let (total, free) = st.map_or((0, 0), |s| {
                (
                    s.f_blocks * u64::from(s.f_bsize),
                    s.f_bavail * u64::from(s.f_bsize),
                )
            });
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
