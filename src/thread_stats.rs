//! Native current-thread CPU counter. Called by the winit main thread at the
//! existing 0.5-second diagnostics tick; includes input callbacks and excludes
//! sleeping/surface waits. 100% means one core, independent of logical CPU count.
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct ThreadStats {
    previous: Option<(std::thread::ThreadId, Duration, Instant)>,
}
impl ThreadStats {
    pub fn reset(&mut self) {
        self.previous = None;
    }
    pub fn sample(&mut self) -> Option<f32> {
        self.observe(std::thread::current().id(), read_cpu(), Instant::now())
    }
    fn observe(
        &mut self,
        thread: std::thread::ThreadId,
        cpu: Option<Duration>,
        now: Instant,
    ) -> Option<f32> {
        let percent = cpu.and_then(|cpu| {
            self.previous
                .and_then(|(previous_thread, previous_cpu, previous_time)| {
                    if previous_thread != thread {
                        return None;
                    }
                    let wall = now.checked_duration_since(previous_time)?;
                    let delta = cpu.checked_sub(previous_cpu)?;
                    if wall.is_zero() {
                        return None;
                    }
                    let p = delta.as_secs_f64() / wall.as_secs_f64() * 100.0;
                    // Permit small kernel counter quantization; impossible jumps remain
                    // unavailable rather than being clamped into a plausible busy value.
                    (p.is_finite() && (0.0..=101.0).contains(&p)).then_some(p.min(100.0) as f32)
                })
        });
        self.previous = cpu.map(|cpu| (thread, cpu, now));
        percent
    }
}

#[cfg(target_os = "linux")]
fn read_cpu() -> Option<Duration> {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::zeroed();
    // libc uses the target ABI's timespec and clockid_t, never hard-coded layouts.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, time.as_mut_ptr()) } != 0 {
        return None;
    }
    let time = unsafe { time.assume_init() };
    if time.tv_sec < 0 || !(0..1_000_000_000).contains(&time.tv_nsec) {
        return None;
    }
    Some(Duration::new(time.tv_sec as u64, time.tv_nsec as u32))
}

#[cfg(windows)]
fn read_cpu() -> Option<Duration> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let mut creation = MaybeUninit::<FILETIME>::zeroed();
    let mut exit = MaybeUninit::<FILETIME>::zeroed();
    let mut kernel = MaybeUninit::<FILETIME>::zeroed();
    let mut user = MaybeUninit::<FILETIME>::zeroed();
    // Current-thread pseudo-handle requires no OpenThread or CloseHandle.
    if unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            creation.as_mut_ptr(),
            exit.as_mut_ptr(),
            kernel.as_mut_ptr(),
            user.as_mut_ptr(),
        )
    } == 0
    {
        return None;
    }
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    let sum =
        ticks(unsafe { kernel.assume_init() }).checked_add(ticks(unsafe { user.assume_init() }))?;
    Some(Duration::new(
        sum / 10_000_000,
        ((sum % 10_000_000) * 100) as u32,
    ))
}

#[cfg(target_os = "macos")]
fn read_cpu() -> Option<Duration> {
    #[link(name = "System")]
    unsafe extern "C" {
        fn mach_thread_self() -> libc::mach_port_t;
        static mach_task_self_: libc::mach_port_t;
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }
    struct ThreadPort(libc::mach_port_t);
    impl Drop for ThreadPort {
        fn drop(&mut self) {
            // mach_thread_self increments the send-right reference each call.
            // Release it on success, kernel failure, and invalid sample alike.
            if self.0 != 0 {
                unsafe {
                    let _ = mach_port_deallocate(mach_task_self_, self.0);
                }
            }
        }
    }
    let port = ThreadPort(unsafe { mach_thread_self() });
    if port.0 == 0 {
        return None;
    }
    let mut info = std::mem::MaybeUninit::<libc::thread_basic_info>::zeroed();
    let mut count = libc::THREAD_BASIC_INFO_COUNT;
    let rc = unsafe {
        libc::thread_info(
            port.0,
            libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    if rc != libc::KERN_SUCCESS || count != libc::THREAD_BASIC_INFO_COUNT {
        return None;
    }
    let info = unsafe { info.assume_init() };
    let duration = |time: libc::time_value_t| {
        if time.seconds < 0 || !(0..1_000_000).contains(&time.microseconds) {
            return None;
        }
        Some(Duration::new(
            time.seconds as u64,
            time.microseconds as u32 * 1000,
        ))
    };
    duration(info.user_time)?.checked_add(duration(info.system_time)?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn read_cpu() -> Option<Duration> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_core_normalization_missing_reset_and_impossible_jumps() {
        let mut s = ThreadStats::default();
        let id = std::thread::current().id();
        let t = Instant::now();
        assert_eq!(s.observe(id, Some(Duration::ZERO), t), None);
        assert_eq!(
            s.observe(
                id,
                Some(Duration::from_millis(250)),
                t + Duration::from_millis(500)
            ),
            Some(50.0)
        );
        assert_eq!(s.observe(id, None, t + Duration::from_secs(1)), None);
        assert_eq!(
            s.observe(
                id,
                Some(Duration::from_millis(300)),
                t + Duration::from_millis(1500)
            ),
            None
        );
        assert_eq!(
            s.observe(
                id,
                Some(Duration::from_millis(200)),
                t + Duration::from_secs(2)
            ),
            None
        );
        assert_eq!(
            s.observe(
                id,
                Some(Duration::from_millis(200)),
                t + Duration::from_secs(2)
            ),
            None
        );
        assert_eq!(
            s.observe(
                id,
                Some(Duration::from_secs(20)),
                t + Duration::from_millis(2500)
            ),
            None
        );
        s.reset();
        assert_eq!(
            s.observe(
                id,
                Some(Duration::from_secs(20)),
                t + Duration::from_secs(3)
            ),
            None
        );
    }
    #[test]
    fn a_different_thread_never_inherits_another_threads_baseline() {
        let mut s = ThreadStats::default();
        let t = Instant::now();
        let id = std::thread::current().id();
        let other = std::thread::spawn(|| std::thread::current().id())
            .join()
            .unwrap();
        s.observe(id, Some(Duration::ZERO), t);
        assert_eq!(
            s.observe(
                other,
                Some(Duration::from_millis(1)),
                t + Duration::from_millis(500)
            ),
            None
        );
    }
}
