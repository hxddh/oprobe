//! Client process CPU time and memory: `getrusage` on Unix, `GetProcessTimes` /
//! `GetProcessMemoryInfo` on Windows.

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
    // SAFETY: getrusage(RUSAGE_SELF) fills the zero-initialised struct it is given.
    let r = unsafe {
        let mut r: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut r) != 0 {
            return None;
        }
        r
    };
    let tv = |t: libc::timeval| {
        Duration::from_secs(t.tv_sec.max(0) as u64) + Duration::from_micros(t.tv_usec.max(0) as u64)
    };
    let maxrss = r.ru_maxrss.max(0) as u64;
    // ru_maxrss is bytes on Apple platforms, KiB elsewhere.
    let rss_bytes = if cfg!(target_vendor = "apple") {
        maxrss
    } else {
        maxrss * 1024
    };
    Some(Usage {
        cpu: tv(r.ru_utime) + tv(r.ru_stime),
        rss_bytes,
    })
}

#[cfg(windows)]
pub fn usage() -> Option<Usage> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
    // SAFETY: Win32 calls on the current-process pseudo handle with valid out-pointers.
    let mem = unsafe {
        let h = GetCurrentProcess();
        if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) == 0 {
            return None;
        }
        let mut m: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        m.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if GetProcessMemoryInfo(h, &mut m, m.cb) != 0 {
            m.WorkingSetSize as u64
        } else {
            0
        }
    };
    let ft = |t: FILETIME| {
        Duration::from_nanos((((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64) * 100)
    };
    Some(Usage {
        cpu: ft(k) + ft(u),
        rss_bytes: mem,
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
