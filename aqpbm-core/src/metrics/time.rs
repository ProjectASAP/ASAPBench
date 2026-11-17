//! Wall-clock + CPU-time primitives. The CPU side reads `getrusage`, so it
//! works on any unix (macOS included) and reports zeros elsewhere; wall-clock
//! uses `std::time::Instant` and is portable.

use std::time::Instant;

/// Boundary-stamped wall-clock timer. `start()` captures the
/// start; `elapsed_ns()` returns nanoseconds since then. Safe
/// to sample many times.
#[derive(Debug, Clone, Copy)]
pub struct WallClock {
    start: Instant,
}

impl WallClock {
    pub fn start() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    pub fn elapsed_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
}

/// CPU time (user + sys) delta for the current process, captured at `start()`
/// and finalised by `finish()`. Unix only; elsewhere `finish()` returns zeros.
#[derive(Debug, Clone, Copy)]
pub struct CpuTimeSampler {
    start: RawCpuTime,
}

impl CpuTimeSampler {
    pub fn start() -> Self {
        Self {
            start: RawCpuTime::now(),
        }
    }

    pub fn finish(self) -> CpuTimeSample {
        let end = RawCpuTime::now();
        CpuTimeSample {
            user_ns: end.user_ns.saturating_sub(self.start.user_ns),
            sys_ns: end.sys_ns.saturating_sub(self.start.sys_ns),
        }
    }
}

/// CPU time of the calling thread (user + system), from
/// `CLOCK_THREAD_CPUTIME_ID`; 0 off unix. Unlike [`CpuTimeSampler`], work on
/// other threads of the process does not count: a thread that sleeps 100 ms
/// while another thread spins reads about 0 ns.
#[derive(Debug, Clone, Copy)]
pub struct ThreadCpuTime;

impl ThreadCpuTime {
    /// The calling thread's CPU time so far, in ns.
    #[cfg(unix)]
    pub fn now_ns() -> u64 {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `clock_gettime` writes one timespec into `now`, which this
        // frame owns; the return value is checked.
        let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
        if rc != 0 {
            return 0;
        }
        (now.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(now.tv_nsec as u64)
    }

    /// The calling thread's CPU time so far, in ns: 0 off unix.
    #[cfg(not(unix))]
    pub fn now_ns() -> u64 {
        0
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CpuTimeSample {
    pub user_ns: u64,
    pub sys_ns: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct RawCpuTime {
    user_ns: u64,
    sys_ns: u64,
}

#[cfg(unix)]
impl RawCpuTime {
    fn now() -> Self {
        use std::mem::MaybeUninit;
        let mut ru = MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: `getrusage` writes a full rusage struct when
        // called with RUSAGE_SELF; the return value is checked.
        let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) };
        if rc != 0 {
            return Self::default();
        }
        let ru = unsafe { ru.assume_init() };
        let to_ns = |tv: libc::timeval| -> u64 {
            (tv.tv_sec as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add((tv.tv_usec as u64).saturating_mul(1_000))
        };
        Self {
            user_ns: to_ns(ru.ru_utime),
            sys_ns: to_ns(ru.ru_stime),
        }
    }
}

#[cfg(not(unix))]
impl RawCpuTime {
    fn now() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallclock_monotonic() {
        let w = WallClock::start();
        std::thread::sleep(std::time::Duration::from_millis(1));
        assert!(w.elapsed_ns() >= 1_000_000);
    }

    #[cfg(unix)]
    #[test]
    fn cputime_nonzero_after_busy_loop() {
        let s = CpuTimeSampler::start();
        // Small busy loop to accumulate user CPU.
        let mut acc: u64 = 0;
        for i in 0..1_000_000u64 {
            acc = acc.wrapping_add(i);
        }
        std::hint::black_box(acc);
        let sample = s.finish();
        assert!(
            sample.user_ns > 0 || sample.sys_ns > 0,
            "expected some CPU time, got user={} sys={}",
            sample.user_ns,
            sample.sys_ns
        );
    }

    #[cfg(unix)]
    #[test]
    fn thread_cpu_time_advances_with_work_on_this_thread() {
        let before = ThreadCpuTime::now_ns();
        let mut acc: u64 = 0;
        for i in 0..10_000_000u64 {
            acc = acc.wrapping_add(i);
        }
        std::hint::black_box(acc);
        let after = ThreadCpuTime::now_ns();
        assert!(
            after > before,
            "thread CPU did not advance: {before} -> {after}"
        );
    }
}
