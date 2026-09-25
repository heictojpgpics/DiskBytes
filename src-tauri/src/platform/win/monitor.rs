//! Raw machine sampling: CPU ticks, memory, network, processes,
//! volumes. Carries the layout tests.

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetLogicalDriveStringsW, GetVolumeInformationW,
};

use super::wide;

// ============================================================================
// M9: Monitor raw sampling (spec §12; doc 02 §7)
// ============================================================================
// One stateless snapshot of the whole machine. The sampler thread in
// `commands/monitor.rs` holds two consecutive `RawMonitor`s and lets
// `core::monitor` compute the deltas (CPU %, per-process CPU, rates).

/// One raw machine snapshot (no deltas — deltas are computed by the
/// caller against the previous snapshot).
#[derive(Debug, Clone, Default)]
pub struct RawMonitor {
    /// `GetSystemTimes` ticks (kernel INCLUDES idle).
    pub ticks: diskbytes_core::monitor::CpuTicks,
    pub threads: u32,
    pub processes: u32,
    pub mem_total: u64,
    pub mem_available: u64,
    pub kernel_paged: u64,
    pub kernel_nonpaged: u64,
    pub system_cache: u64,
    pub commit_total: u64,
    pub commit_limit: u64,
    /// Memory Compression private usage (`None` → "—" in the UI).
    pub compressed_ws: Option<u64>,
    /// Filtered octet sums (up, non-loopback, Ethernet/802.11,
    /// HardwareInterface).
    pub net_in: u64,
    pub net_out: u64,
    /// Fixed + removable volumes.
    pub volumes: Vec<diskbytes_core::monitor::VolumeSample>,
    /// (pid, name, kernel_100ns, user_100ns, working set) — PID 0
    /// (System Idle) excluded.
    pub procs: Vec<(u32, String, u64, u64, u64)>,
}

/// Take one raw machine sample. Best effort: individual API failures
/// leave their fields at zero (the spec's values must be SANE, never
/// wrong-looking fabrications — zeroed fields read as "—").
pub fn monitor_raw() -> RawMonitor {
    // Gather every subsystem (best effort; failures read as zero/"—").
    let ticks = system_times();
    let perf = performance_information();
    let mem = global_memory();
    let net = network_octets();
    let procs = process_snapshot();
    let compressed_ws = memory_compression_ws(&procs);
    let volumes = volume_samples();
    let (
        threads,
        processes,
        kernel_paged,
        kernel_nonpaged,
        system_cache,
        commit_total,
        commit_limit,
    ) = perf.map_or((0, 0, 0, 0, 0, 0, 0), |pi| {
        (
            pi.ThreadCount,
            pi.ProcessCount,
            pi.KernelPaged as u64,
            pi.KernelNonpaged as u64,
            pi.SystemCache as u64,
            pi.CommitTotal as u64,
            pi.CommitLimit as u64,
        )
    });
    let (mem_total, mem_available) = mem.unwrap_or((0, 0));
    let (net_in, net_out) = net.unwrap_or((0, 0));
    RawMonitor {
        ticks,
        threads,
        processes,
        mem_total,
        mem_available,
        kernel_paged,
        kernel_nonpaged,
        system_cache,
        commit_total,
        commit_limit,
        compressed_ws,
        net_in,
        net_out,
        volumes,
        procs,
    }
}

/// `GetSystemTimes` as tick counters.
fn system_times() -> diskbytes_core::monitor::CpuTicks {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetSystemTimes;
    let (mut idle, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: three FILETIME out-structs, valid for the call.
    let ok = unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_ok() };
    if !ok {
        return diskbytes_core::monitor::CpuTicks::default();
    }
    let q = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    diskbytes_core::monitor::CpuTicks {
        idle: q(idle),
        kernel: q(kernel),
        user: q(user),
    }
}

/// `GetPerformanceInfo` snapshot.
fn performance_information(
) -> Option<windows::Win32::System::ProcessStatus::PERFORMANCE_INFORMATION> {
    use windows::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
    let mut pi = PERFORMANCE_INFORMATION {
        cb: std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32,
        ..Default::default()
    };
    // SAFETY: struct sized to its own cb for the versioned contract.
    let ok = unsafe {
        GetPerformanceInfo(
            &mut pi,
            std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32,
        )
        .is_ok()
    };
    ok.then_some(pi)
}

/// `GlobalMemoryStatusEx` → (total, available).
fn global_memory() -> Option<(u64, u64)> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut ms = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: dwLength set per the documented contract; out-struct valid.
    let ok = unsafe { GlobalMemoryStatusEx(&mut ms) }.is_ok();
    ok.then_some((ms.ullTotalPhys, ms.ullAvailPhys))
}

/// Filtered octet sums (spec §12: interfaces that are UP, not loopback,
/// type Ethernet (6) or IEEE 802.11 (71), with the HardwareInterface
/// flag — excludes VPN/WSL/Hyper-V virtual switches).
fn network_octets() -> Option<(u64, u64)> {
    use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
    const IF_TYPE_ETHERNET: u32 = 6; // RFC 1213 ifType
    const IF_TYPE_IEEE80211: u32 = 71;
    const IF_TYPE_LOOPBACK: u32 = 24;
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    // SAFETY: GetIfTable2 allocates the table; freed via FreeMibTable
    // below (the documented ownership contract).
    let err = unsafe { GetIfTable2(&mut table) };
    if !err.is_ok() || table.is_null() {
        return None;
    }
    let (mut inn, mut out) = (0u64, 0u64);
    // SAFETY: NumEntries bounds the Table slice; rows are valid for the
    // table's lifetime.
    unsafe {
        let n = (*table).NumEntries as usize;
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), n);
        for row in rows {
            let up = row.OperStatus == windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
            let hw = row.InterfaceAndOperStatusFlags._bitfield & 0x01 != 0; // netioapi.h bit 0
            let ty_ok = (row.Type == IF_TYPE_ETHERNET || row.Type == IF_TYPE_IEEE80211)
                && row.Type != IF_TYPE_LOOPBACK;
            if up && hw && ty_ok {
                inn = inn.saturating_add(row.InOctets);
                out = out.saturating_add(row.OutOctets);
            }
        }
        FreeMibTable(table.cast::<core::ffi::c_void>());
    }
    Some((inn, out))
}

/// All processes via ONE `NtQuerySystemInformation(SystemProcessInformation)`
/// call with a growing buffer (reads protected processes too — no
/// handles opened). PID 0 (System Idle) excluded per spec §12.
fn process_snapshot() -> Vec<(u32, String, u64, u64, u64)> {
    use windows::Wdk::System::SystemInformation::{
        NtQuerySystemInformation, SystemProcessInformation,
    };
    use windows::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION;
    // 256 KiB as 8-byte-aligned words (records are 8-aligned).
    let mut words: Vec<u64> = vec![0; 32 * 1024];
    loop {
        let mut needed = 0u32;
        // SAFETY: words/len pair; SystemProcessInformation is the class
        // 5 layout the struct mirrors; the walk below checks every
        // offset. The u64 backing guarantees record alignment.
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessInformation,
                words.as_mut_ptr().cast::<core::ffi::c_void>(),
                (words.len() * 8) as u32,
                &mut needed,
            )
        };
        if status == windows::Win32::Foundation::STATUS_INFO_LENGTH_MISMATCH {
            let next = (needed as usize / 8 + 1).max(words.len() * 2);
            words.resize(next, 0);
            continue;
        }
        if status != windows::Win32::Foundation::NTSTATUS(0) {
            return Vec::new(); // sampling failed: empty (reads as "—")
        }
        break;
    }
    let buf_len = words.len() * 8;
    let mut out = Vec::new();
    let base = words.as_ptr().cast::<u8>();
    let mut off = 0usize;
    loop {
        if off + std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>() > buf_len {
            break; // torn tail — stop
        }
        // SAFETY: bounds checked above; the record layout matches
        // SYSTEM_PROCESS_INFORMATION (the walk is offset-driven). The
        // buffer is 8-byte aligned (Vec<u64> backing) so the cast to
        // the struct pointer is alignment-sound.
        let rec = unsafe { base.add(off).cast::<SYSTEM_PROCESS_INFORMATION>() };
        let pid = unsafe { (*rec).UniqueProcessId }.0 as usize as u32;
        if pid != 0 {
            // Kernel/User times are hidden in Reserved1: the Vista+
            // layout puts CreateTime@0x20, UserTime@0x28, KernelTime@0x30
            // → Reserved1[32..40] and [40..48] (verified by the layout
            // test below).
            let r1 = &unsafe { (*rec).Reserved1 };
            let q = |i: usize| {
                let mut b = [0u8; 8];
                b.copy_from_slice(&r1[i..i + 8]);
                u64::from_le_bytes(b)
            };
            let user = q(32);
            let kernel = q(40);
            let name = unsafe {
                let us = (*rec).ImageName;
                let len = (us.Length / 2) as usize;
                if us.Buffer.is_null() || len == 0 {
                    String::new()
                } else if (us.Buffer.0 as usize) >= base as usize
                    && (us.Buffer.0 as usize) + len * 2 <= base as usize + buf_len
                {
                    let slice = std::slice::from_raw_parts(us.Buffer.0, len);
                    String::from_utf16_lossy(slice)
                } else {
                    String::new()
                }
            };
            let ws = unsafe { (*rec).WorkingSetSize } as u64;
            out.push((pid, name, kernel, user, ws));
        }
        let next = unsafe { (*rec).NextEntryOffset } as usize;
        if next == 0 {
            break;
        }
        off += next;
    }
    out
}

/// Memory Compression private usage: locate the process in the snapshot
/// (image name "Memory Compression"/"MemCompression"), then
/// `PROCESS_QUERY_LIMITED_INFORMATION` + `K32GetProcessMemoryInfo` with
/// the EX layout → `PrivateUsage`. `None` when unavailable (UI: "—").
fn memory_compression_ws(procs: &[(u32, String, u64, u64, u64)]) -> Option<u64> {
    use windows::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let pid = procs
        .iter()
        .find(|p| {
            let n = p.1.to_ascii_lowercase();
            n == "memory compression" || n == "memcompression"
        })
        .map(|p| p.0)?;
    // SAFETY: pid came from the snapshot; handle closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) };
    let Ok(handle) = handle else {
        return None;
    };
    let cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb,
        ..Default::default()
    };
    // SAFETY: counters sized to its cb (EX layout → PrivateUsage valid);
    // handle has the required access.
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            handle,
            std::ptr::addr_of_mut!(counters)
                .cast::<windows::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
    };
    // SAFETY: balance the OpenProcess.
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) };
    ok.as_bool().then_some(counters.PrivateUsage as u64)
}

/// Fixed + removable mounted volumes with labels + free space.
fn volume_samples() -> Vec<diskbytes_core::monitor::VolumeSample> {
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;
    const DRIVE_REMOVABLE: u32 = 2; // winbase.h
    let mut buf = [0u16; 512];
    // SAFETY: buffer + capacity in u16s per the documented contract.
    let len = unsafe { GetLogicalDriveStringsW(Some(&mut buf)) } as usize;
    if len == 0 || len >= buf.len() {
        return Vec::new();
    }
    let roots: Vec<String> = buf[..len]
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect();
    let mut out = Vec::new();
    for root in roots {
        let wide_root = wide(&root);
        // SAFETY: NUL-terminated root for both calls.
        let ty = unsafe { GetDriveTypeW(PCWSTR(wide_root.as_ptr())) };
        if ty != DRIVE_FIXED && ty != DRIVE_REMOVABLE {
            continue;
        }
        let mut free: u64 = 0;
        let mut total: u64 = 0;
        let mut total_free: u64 = 0;
        // SAFETY: out-pointers valid.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                PCWSTR(wide_root.as_ptr()),
                Some(&mut free),
                Some(&mut total),
                Some(&mut total_free),
            )
        };
        if ok.is_err() {
            continue;
        }
        let mut label = [0u16; 64];

        // SAFETY: label buffer slice; optional out-pointers are None.
        let _ = unsafe {
            GetVolumeInformationW(
                PCWSTR(wide_root.as_ptr()),
                Some(&mut label),
                None,
                None,
                None,
                None,
            )
        };
        let label_len = label.iter().position(|&c| c == 0).unwrap_or(label.len());
        out.push(diskbytes_core::monitor::VolumeSample {
            root: root.clone(),
            label: String::from_utf16_lossy(&label[..label_len]),
            total,
            free,
        });
    }
    out
}

#[cfg(test)]
mod monitor_layout_tests {
    use windows::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION;

    /// Lock the record layout the process walk depends on: ImageName
    /// sits at offset 0x38 (Vista+ layout) so the Reserved1 windows
    /// [32..40]=UserTime, [40..48]=KernelTime derivation is sound.
    #[test]
    fn system_process_information_layout() {
        let pi = SYSTEM_PROCESS_INFORMATION::default();
        let base = std::ptr::addr_of!(pi);
        let name = std::ptr::addr_of!(pi.ImageName);
        let reserved = std::ptr::addr_of!(pi.Reserved1);
        let pid = std::ptr::addr_of!(pi.UniqueProcessId);
        let ws = std::ptr::addr_of!(pi.WorkingSetSize);
        assert_eq!(
            name as usize - base as usize,
            0x38,
            "ImageName must be at 0x38"
        );
        assert_eq!(
            reserved as usize - base as usize,
            0x08,
            "Reserved1 must start at 0x08"
        );
        assert_eq!(
            pid as usize - base as usize,
            0x50,
            "UniqueProcessId must be at 0x50"
        );
        assert!(
            (ws as usize - base as usize) > 0x50,
            "WorkingSetSize follows the header"
        );
        assert_eq!(
            std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>() % 8,
            0,
            "records are 8-byte aligned"
        );
    }
}
