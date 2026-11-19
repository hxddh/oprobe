//! Client process CPU time and memory from the OS, without extra crates: `getrusage` on Unix,
//! `GetProcessTimes` / `K32GetProcessMemoryInfo` on Windows.

use std::time::{Duration, Instant};

/// Cumulative CPU time (user + system) and resident memory of this process.
#[derive(Clone, Copy, Debug)]
pub struct Usage {
    pub cpu: Duration,
    /// Resident set size in bytes (peak RSS where the OS only reports that).
    pub rss_bytes: u64,
}

#[cfg(unix)]
pub fn usage() -> Option<Usage> {
    // `struct rusage` starts with two `struct timeval` (16 bytes each on 64-bit Unix) followed by
    // `long` counters, `ru_maxrss` first. 18 words cover the whole struct on Linux and macOS.
    #[repr(C)]
    struct RUsage([i64; 18]);
    unsafe extern "C" {
        fn getrusage(who: i32, usage: *mut RUsage) -> i32;
    }
    if std::mem::size_of::<usize>() != 8 {
        return None;
    }
    let mut r = RUsage([0; 18]);
    // SAFETY: RUSAGE_SELF (0) fills a `struct rusage`, which fits in the buffer.
    if unsafe { getrusage(0, &mut r) } != 0 {
        return None;
    }
    let tv = |sec: i64, usec: i64| {
        // `tv_usec` is 32-bit on macOS; mask the padding.
        Duration::from_secs(sec.max(0) as u64) + Duration::from_micros((usec & 0xFFFF_FFFF) as u64)
    };
    let cpu = tv(r.0[0], r.0[1]) + tv(r.0[2], r.0[3]);
    let maxrss = r.0[4].max(0) as u64;
    let rss_bytes = if cfg!(target_vendor = "apple") {
        maxrss
    } else {
        maxrss * 1024
    };
    Some(Usage { cpu, rss_bytes })
}

#[cfg(windows)]
pub fn usage() -> Option<Usage> {
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct FileTime(u32, u32);
    #[repr(C)]
    #[derive(Default)]
    struct MemCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set: usize,
        working_set: usize,
        quota_peak_paged: usize,
        quota_paged: usize,
        quota_peak_nonpaged: usize,
        quota_nonpaged: usize,
        pagefile: usize,
        peak_pagefile: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetProcessTimes(
            h: isize,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn K32GetProcessMemoryInfo(h: isize, c: *mut MemCounters, cb: u32) -> i32;
    }
    let (mut c, mut e, mut k, mut u) = Default::default();
    let mut m = MemCounters {
        cb: std::mem::size_of::<MemCounters>() as u32,
        ..Default::default()
    };
    // SAFETY: plain Win32 calls on the current-process pseudo handle with valid out-pointers.
    unsafe {
        let h = GetCurrentProcess();
        if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) == 0 {
            return None;
        }
        K32GetProcessMemoryInfo(h, &mut m, m.cb);
    }
    let ft = |t: FileTime| Duration::from_nanos((((t.1 as u64) << 32) | t.0 as u64) * 100);
    Some(Usage {
        cpu: ft(k) + ft(u),
        rss_bytes: m.working_set as u64,
    })
}

#[cfg(not(any(unix, windows)))]
pub fn usage() -> Option<Usage> {
    None
}

/// Turns successive [`usage`] readings into CPU % (of one core) over the interval.
pub struct CpuMeter {
    last: Option<(Instant, Duration)>,
}

impl CpuMeter {
    pub fn new() -> Self {
        Self {
            last: usage().map(|u| (Instant::now(), u.cpu)),
        }
    }

    /// `(cpu_pct, rss_mib)` since the previous call.
    pub fn sample(&mut self) -> Option<(f64, f64)> {
        let u = usage()?;
        let now = Instant::now();
        let prev = self.last.replace((now, u.cpu));
        let (t0, c0) = prev?;
        let wall = now.saturating_duration_since(t0).as_secs_f64();
        if wall <= 0.0 {
            return None;
        }
        let pct = u.cpu.saturating_sub(c0).as_secs_f64() / wall * 100.0;
        Some((pct, u.rss_bytes as f64 / 1_048_576.0))
    }
}

impl Default for CpuMeter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_cpu_spent_spinning() {
        let Some(before) = usage() else { return };
        assert!(before.rss_bytes > 0);
        let mut m = CpuMeter::new();
        let t = Instant::now();
        let mut x = 0u64;
        while t.elapsed() < Duration::from_millis(200) {
            x = x.wrapping_mul(31).wrapping_add(7);
        }
        std::hint::black_box(x);
        let (pct, rss) = m.sample().expect("sample");
        assert!(pct > 30.0 && pct < 400.0, "cpu {pct}");
        assert!(rss > 1.0);
        assert!(usage().unwrap().cpu > before.cpu);
    }
}
