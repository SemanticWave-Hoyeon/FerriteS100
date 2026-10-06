//! Application process diagnostics; no chart/kernel dependency.
use std::time::{Duration, Instant};

pub const UPDATE_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Default)]
struct RawSample {
    cpu_time: Option<Duration>,
    resident_bytes: Option<u64>,
}
#[derive(Debug, Default)]
pub struct Sample {
    pub cpu_percent: Option<f32>,
    pub resident_mib: Option<f32>,
}
pub struct ProcessStats {
    previous: Option<(Duration, Instant)>,
    logical_cpus: usize,
    updates: u64,
}
impl Default for ProcessStats {
    fn default() -> Self {
        Self {
            previous: None,
            logical_cpus: std::thread::available_parallelism().map_or(1, usize::from),
            updates: 0,
        }
    }
}
impl ProcessStats {
    pub fn logical_cpus(&self) -> usize {
        self.logical_cpus
    }
    pub fn updates(&self) -> u64 {
        self.updates
    }
    pub fn reset(&mut self) {
        self.previous = None;
    }
    pub fn sample(&mut self) -> Sample {
        let raw = read_process();
        self.observe(raw, Instant::now())
    }
    fn observe(&mut self, raw: RawSample, now: Instant) -> Sample {
        self.updates += 1;
        let cpu_percent = raw.cpu_time.and_then(|cpu| {
            self.previous.and_then(|(previous, time)| {
                let wall = now.checked_duration_since(time)?;
                let delta = cpu.checked_sub(previous)?;
                if wall.is_zero() {
                    return None;
                }
                Some(
                    (100. * delta.as_secs_f64()
                        / wall.as_secs_f64()
                        / self.logical_cpus.max(1) as f64)
                        .clamp(0., 100.) as f32,
                )
            })
        });
        // An API failure clears the baseline; never show stale or fabricated data.
        self.previous = raw.cpu_time.map(|cpu| (cpu, now));
        Sample {
            cpu_percent,
            resident_mib: raw
                .resident_bytes
                .map(|bytes| (bytes as f64 / 1_048_576.) as f32),
        }
    }
}

/// Wake diagnostics independently of chart loading and temporal portrayal.
/// When overdue, avoid spinning on a deadline in the past while redraw is queued.
pub fn debug_deadline(enabled: bool, last: Instant, now: Instant) -> Option<Instant> {
    enabled.then(|| {
        let deadline = last + UPDATE_INTERVAL;
        if deadline > now {
            deadline
        } else {
            now + UPDATE_INTERVAL
        }
    })
}

#[cfg(target_os = "macos")]
fn read_process() -> RawSample {
    use std::mem::MaybeUninit;
    unsafe {
        // getrusage CPU timeval fields are seconds + microseconds, independent
        // of Apple Silicon's Mach absolute-time tick frequency.
        let mut usage = MaybeUninit::<libc::rusage>::zeroed();
        let cpu_time = if libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) == 0 {
            let usage = usage.assume_init();
            let duration = |time: libc::timeval| {
                if time.tv_sec < 0 || !(0..1_000_000).contains(&time.tv_usec) {
                    return None;
                }
                Some(Duration::new(
                    time.tv_sec as u64,
                    time.tv_usec as u32 * 1000,
                ))
            };
            duration(usage.ru_utime).and_then(|user| {
                duration(usage.ru_stime).and_then(|system| user.checked_add(system))
            })
        } else {
            None
        };
        // ru_maxrss is a high-water mark, not current RAM usage. libproc's
        // resident size is current bytes and matches the Windows working set.
        let mut info = MaybeUninit::<libc::rusage_info_v2>::zeroed();
        let resident_bytes = if libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V2,
            info.as_mut_ptr().cast::<libc::rusage_info_t>(),
        ) == 0
        {
            Some(info.assume_init().ri_resident_size)
        } else {
            None
        };
        RawSample {
            cpu_time,
            resident_bytes,
        }
    }
}

#[cfg(windows)]
fn read_process() -> RawSample {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    unsafe {
        let process = GetCurrentProcess();
        let mut memory = MaybeUninit::<PROCESS_MEMORY_COUNTERS>::zeroed();
        let resident_bytes = if GetProcessMemoryInfo(
            process,
            memory.as_mut_ptr(),
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ) != 0
        {
            Some(memory.assume_init().WorkingSetSize as u64)
        } else {
            None
        };
        let mut creation = MaybeUninit::<FILETIME>::zeroed();
        let mut exit = MaybeUninit::<FILETIME>::zeroed();
        let mut kernel = MaybeUninit::<FILETIME>::zeroed();
        let mut user = MaybeUninit::<FILETIME>::zeroed();
        let cpu_time = if GetProcessTimes(
            process,
            creation.as_mut_ptr(),
            exit.as_mut_ptr(),
            kernel.as_mut_ptr(),
            user.as_mut_ptr(),
        ) != 0
        {
            let ticks =
                |time: FILETIME| ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64;
            ticks(kernel.assume_init())
                .checked_add(ticks(user.assume_init()))
                .map(|ticks| Duration::new(ticks / 10_000_000, ((ticks % 10_000_000) * 100) as u32))
        } else {
            None
        };
        RawSample {
            cpu_time,
            resident_bytes,
        }
    }
}
#[cfg(not(any(windows, target_os = "macos")))]
fn read_process() -> RawSample {
    RawSample::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delta_normalization_failures_and_memory_units() {
        let mut stats = ProcessStats {
            previous: None,
            logical_cpus: 8,
            updates: 0,
        };
        let now = Instant::now();
        let raw = |milliseconds, bytes| RawSample {
            cpu_time: Some(Duration::from_millis(milliseconds)),
            resident_bytes: Some(bytes),
        };
        let first = stats.observe(raw(1000, 2 * 1_048_576), now);
        assert_eq!(first.cpu_percent, None);
        assert_eq!(first.resident_mib, Some(2.));
        let busy = stats.observe(raw(3000, 3 * 1_048_576), now + Duration::from_secs(1));
        assert_eq!(busy.cpu_percent, Some(25.));
        assert_eq!(busy.resident_mib, Some(3.));
        let idle = stats.observe(raw(3000, 1_048_576), now + Duration::from_secs(2));
        assert_eq!(idle.cpu_percent, Some(0.));
        assert_eq!(idle.resident_mib, Some(1.));
        assert_eq!(
            stats
                .observe(raw(2999, 0), now + Duration::from_secs(3))
                .cpu_percent,
            None
        );
        assert_eq!(
            stats
                .observe(raw(4000, 0), now + Duration::from_secs(3))
                .cpu_percent,
            None
        );
        let failed = stats.observe(RawSample::default(), now + Duration::from_secs(4));
        assert_eq!(failed.cpu_percent, None);
        assert_eq!(failed.resident_mib, None);
        assert_eq!(
            stats
                .observe(raw(5000, 0), now + Duration::from_secs(5))
                .cpu_percent,
            None
        );
    }
    #[test]
    fn idle_refresh_is_disabled_with_debug_and_never_past_due() {
        let now = Instant::now();
        assert_eq!(debug_deadline(false, now, now), None);
        assert_eq!(debug_deadline(true, now, now), Some(now + UPDATE_INTERVAL));
        assert!(
            debug_deadline(true, now, now + Duration::from_secs(3)).unwrap()
                > now + Duration::from_secs(3)
        );
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn mac_real_process_reports_current_memory_and_cpu_delta() {
        let raw = read_process();
        assert!(raw.resident_bytes.unwrap() > 0);
        assert!(raw.cpu_time.is_some());
        let mut stats = ProcessStats::default();
        let first = stats.sample();
        assert!(first.resident_mib.unwrap() > 0.);
        assert_eq!(first.cpu_percent, None);
        let begin = Instant::now();
        let mut v = 1u64;
        while begin.elapsed() < Duration::from_millis(80) {
            v = std::hint::black_box(v.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
        std::hint::black_box(v);
        let busy = stats.sample();
        assert!(busy.cpu_percent.unwrap() > 0.);
        assert!(busy.resident_mib.unwrap() > 0.);
    }
}
