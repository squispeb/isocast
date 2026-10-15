//! Host-clock media deadlines. Linux's timer coalescing is far smaller
//! than macOS's launchd policy ever stretched a sleep, but media
//! clocks still take absolute waits so background batch loads can't
//! stretch a frame's wake: `clock_nanosleep(TIMER_ABSTIME)`.
//!
//! The engine's measured-stretching escalation needs no extra timer
//! mode here: Linux's default timing is the good case macOS's kqueue
//! dance existed to reach.

use std::time::Duration;

use isocast_engine::Time;

/// Sleep until `at` on the host clock (`CLOCK_MONOTONIC`).
pub(crate) fn sleep_until(at: Time) {
    let now = host_time();
    if at <= now {
        return;
    }
    let remaining = at.saturating_since(now);
    // POSIX: clock_nanosleep returns the error number directly (not -1
    // with errno), and with TIMER_ABSTIME an interruption must be
    // retried against the SAME deadline, so the deadline is computed
    // once and the wait repeated until it has passed.
    let target = timespec_after(remaining);
    // SAFETY: target holds whole seconds and nanoseconds on a
    // monotonic clock; with TIMER_ABSTIME an interrupted wait repeats
    // against the same deadline.
    unsafe {
        while libc::clock_nanosleep(
            libc::CLOCK_MONOTONIC,
            libc::TIMER_ABSTIME,
            &target,
            std::ptr::null_mut(),
        ) == libc::EINTR
        {
            // The deadline is absolute; repeat.
        }
    }
}

/// A `timespec` `after` from now.
fn timespec_after(after: Duration) -> libc::timespec {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes exactly the struct given,
    // CLOCK_MONOTONIC is always available.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
    }
    let after_nanos = after.as_nanos() as i128;
    let total = now.tv_sec as i128 * 1_000_000_000 + now.tv_nsec as i128 + after_nanos;
    libc::timespec {
        tv_sec: (total / 1_000_000_000) as libc::time_t,
        tv_nsec: (total % 1_000_000_000) as libc::c_long,
    }
}

/// Run the calling thread ahead of background work: real-time
/// scheduling if the process is privileged for it, quietly nothing
/// otherwise. Linux's timer slack makes this rare, so a failure is
/// not reported.
pub(crate) fn raise_thread() {
    // SAFETY: the process's own scheduling policy, with a
    // zero-initialized parameter set.
    unsafe {
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = 10;
        libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);
    }
}

/// The current host time. `CLOCK_MONOTONIC`, nanoseconds — the master
/// clock macOS's mach time provides.
pub(crate) fn host_time() -> Time {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes exactly the struct given.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
    }
    Time::from_nanos(now.tv_sec * 1_000_000_000 + now.tv_nsec)
}
