//! T23 / G3 row 7 (ENG-30 row 7 increment 13): real process peak memory.
//!
//! `docs/reports/G3-residency-hash.md` lists "process peak memory" among the
//! required retained-memory evidence, distinct from the resident-dense-byte
//! and backing-byte figures the residency pass already reports — those only
//! ever see what `spall_voxel`/`spall_sim` account for; this is the whole
//! process's actual measured footprint (jobs, snapshots, capture buffers,
//! allocator overhead, everything).
//!
//! No new dependency: Windows links `psapi.dll` directly (a stable, always
//! present system DLL since Windows XP) through a hand-written `extern
//! "system"` declaration of `GetProcessMemoryInfo`; Linux reads
//! `/proc/self/status`'s `VmHWM` line (the kernel's own "peak resident set"
//! counter, no permissions needed). Anywhere else this returns `None`,
//! matching this codebase's existing convention of reporting a genuinely
//! unavailable measurement as `None`/`gpu_timing_available: false` rather
//! than fabricating a number (`docs/validation.md`).

/// Peak resident-set / working-set size of this process, in bytes, since
/// process start — `None` on a platform this module does not support.
pub fn process_peak_bytes() -> Option<u64> {
    imp::process_peak_bytes()
}

#[cfg(target_os = "windows")]
mod imp {
    use std::mem::size_of;

    // Layout matches `PROCESS_MEMORY_COUNTERS` from `<psapi.h>` /
    // `winapi::um::psapi`. Only the fields up through `PeakWorkingSetSize`
    // (the value we read) need to be correct; the struct is otherwise opaque
    // to us and we never write to it, so extra trailing fields the OS may
    // fill in are harmless as long as `cb` (declared struct size) matches.
    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
    }

    pub fn process_peak_bytes() -> Option<u64> {
        let mut counters = ProcessMemoryCounters {
            cb: size_of::<ProcessMemoryCounters>() as u32,
            page_fault_count: 0,
            peak_working_set_size: 0,
            working_set_size: 0,
            quota_peak_paged_pool_usage: 0,
            quota_paged_pool_usage: 0,
            quota_peak_non_paged_pool_usage: 0,
            quota_non_paged_pool_usage: 0,
            pagefile_usage: 0,
            peak_pagefile_usage: 0,
        };
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle valid for the
        // process lifetime (no handle to close); `counters` is a correctly
        // sized, exclusively borrowed local the call fills in-place.
        let ok = unsafe {
            GetProcessMemoryInfo(GetCurrentProcess(), &mut counters as *mut _, counters.cb)
        };
        (ok != 0).then_some(counters.peak_working_set_size as u64)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    /// Parses `VmHWM:` (peak resident set, kibibytes) from
    /// `/proc/self/status`. Absent/unparseable -> `None`, never a guess.
    pub fn process_peak_bytes() -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                let kib: u64 = rest.trim().trim_end_matches(" kB").trim().parse().ok()?;
                return Some(kib.saturating_mul(1024));
            }
        }
        None
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod imp {
    pub fn process_peak_bytes() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_plausible_value_or_an_honest_none() {
        // This process has definitely allocated at least a few MiB by the
        // time a test binary is running; a `Some` value must be sane, and a
        // `None` (an unsupported platform) must never masquerade as zero.
        if let Some(bytes) = process_peak_bytes() {
            assert!(bytes > 1_000_000, "implausibly small: {bytes}");
        }
    }
}
