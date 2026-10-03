//! Scheduler accounting for tests that time a round trip on a shared host.
//!
//! A round trip timed with `Instant` includes every moment a thread on its
//! path was runnable but waiting for a CPU. On an idle host that is close to
//! nothing; on a loaded one (a CI runner, the parallel `cargo test` harness, a
//! busy workstation) it alone can exceed a budget written for sozu. Linux
//! reports that wait per thread as the second field of
//! `/proc/self/task/<tid>/schedstat`, "time spent waiting on a runqueue" in
//! nanoseconds (`Documentation/scheduler/sched-stats.rst`), so a test can take
//! it out of what it measures.
//!
//! What it does NOT remove is time a thread spent blocked: a `sleep`, a wait
//! in `epoll_wait` for a timer, a connect that the kernel has not completed.
//! Those are exactly what a latency budget on sozu is meant to catch.

use std::time::Duration;

/// Kernel id of the calling thread, to pass to [`run_queue_delay`]. `None`
/// where the kernel offers no per-thread scheduler statistics.
pub fn current_tid() -> Option<i32> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: gettid(2) takes no argument and cannot fail.
        Some(unsafe { libc::gettid() })
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Cumulative time the threads `tids` have spent runnable but not running.
///
/// A thread whose statistics cannot be read (`None`, an exited thread, a
/// kernel built without `CONFIG_SCHED_INFO`) counts as zero, so the caller
/// subtracts less, never more: the difference of two readings is the delay to
/// take out of the span between them, and with `saturating_sub` a thread that
/// exits in between only shrinks it.
pub fn run_queue_delay(tids: &[Option<i32>]) -> Duration {
    let nanos = tids
        .iter()
        .flatten()
        .filter_map(|tid| std::fs::read_to_string(format!("/proc/self/task/{tid}/schedstat")).ok())
        .filter_map(|stat| stat.split_whitespace().nth(1)?.parse::<u64>().ok())
        .sum();
    Duration::from_nanos(nanos)
}
