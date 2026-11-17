//! Re-pinning the main thread: `run_job` on core a, then on core b, both
//! from the main thread. The second job must run on core b; reading the
//! allowed set from the main thread's (already pinned) mask would refuse b
//! as `CoreNotAllowed { allowed: "a" }`.
//!
//! No libtest harness (`harness = false` in `Cargo.toml`): libtest runs
//! every test on a spawned thread, never on the main thread.

#[cfg(target_os = "linux")]
fn main() {
    use aqpbm_core::harness::engine::StepReport;
    use aqpbm_core::harness::fake::FakeClock;
    use aqpbm_core::harness::protocol::{EngineClock, LoadProbe, Protocol, ProtocolError};

    struct IdleProbe;
    impl LoadProbe for IdleProbe {
        fn core_busy_fraction(&mut self, _core: usize) -> Result<f64, ProtocolError> {
            Ok(0.0)
        }
        fn loadavg(&mut self) -> f64 {
            0.0
        }
    }

    // The first two cores this process may run on.
    let set_bytes = std::mem::size_of::<libc::cpu_set_t>();
    // SAFETY: `allowed` is a plain bit mask owned by this frame; the return
    // value is checked; `CPU_ISSET` reads one bit below the set size.
    let cores: Vec<usize> = unsafe {
        let mut allowed: libc::cpu_set_t = std::mem::zeroed();
        assert_eq!(libc::sched_getaffinity(0, set_bytes, &mut allowed), 0);
        (0..8 * set_bytes)
            .filter(|&core| libc::CPU_ISSET(core, &allowed))
            .take(2)
            .collect()
    };
    if cores.len() < 2 {
        println!("pin_main_thread: skipped, fewer than 2 allowed cores");
        return;
    }

    let clock = FakeClock::default();
    let protocol = Protocol {
        warmup_runs: 0,
        measured_runs: 1,
        ..Protocol::default()
    };
    for &core in &cores {
        let mut ran_on = -1;
        let record = protocol
            .run_job(
                core,
                &mut IdleProbe,
                &EngineClock::CallingThread(&clock),
                &mut || {
                    // SAFETY: `sched_getcpu` has no preconditions.
                    ran_on = unsafe { libc::sched_getcpu() };
                    Ok(StepReport::default())
                },
            )
            .unwrap_or_else(|error| panic!("job on core {core}: {error}"));
        assert_eq!(record.core, core);
        assert_eq!(ran_on, core as i32, "the job on core {core} ran elsewhere");
    }
    println!("pin_main_thread: ok, re-pinned the main thread {cores:?}");
}

#[cfg(not(target_os = "linux"))]
fn main() {}
