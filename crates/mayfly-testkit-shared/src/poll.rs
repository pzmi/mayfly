//! The bounded poll helper (ver §2.5): the one sanctioned wait in test support.
//!
//! A test waits on a real-OS condition (a process exited, a socket appeared, a fake's state
//! changed) only through [`wait_until`]. Everything that can be driven by a channel, a barrier,
//! a scripted fake or fake time is driven that way instead. The wait is bounded, names its
//! reason on timeout, and is never a correctness oracle: a test asserts the state reached,
//! never how long it took.
//!
//! This file is the single exemption of `ARCH-test-sleep`; its behaviour is pinned by the tests
//! below.

use std::fmt;
use std::thread;
use std::time::{Duration, Instant};

/// The first pause between two checks of the condition.
const FIRST_PAUSE: Duration = Duration::from_millis(1);

/// The longest pause between two checks: a slow condition costs at most this much latency.
const LONGEST_PAUSE: Duration = Duration::from_millis(20);

/// The condition did not hold within the deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedOut {
    reason: String,
    deadline: Duration,
}

impl TimedOut {
    /// What the caller was waiting for.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// The bound that elapsed.
    pub fn deadline(&self) -> Duration {
        self.deadline
    }
}

impl fmt::Display for TimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "timed out after {:?} waiting for {}",
            self.deadline, self.reason
        )
    }
}

impl std::error::Error for TimedOut {}

/// Checks `condition` until it holds or `deadline` has elapsed, pausing between checks with a
/// bounded backoff. The condition is checked once more at the deadline, so a condition that
/// becomes true during the last pause is not missed.
///
/// # Errors
///
/// [`TimedOut`] carrying `reason` when the condition did not hold within `deadline`. Pinned by
/// `should_report_the_reason_when_the_condition_never_holds`.
pub fn try_wait_until(
    deadline: Duration,
    reason: &str,
    mut condition: impl FnMut() -> bool,
) -> Result<(), TimedOut> {
    let start = Instant::now();
    let mut pause = FIRST_PAUSE;
    loop {
        if condition() {
            return Ok(());
        }
        let elapsed = start.elapsed();
        if elapsed >= deadline {
            return Err(TimedOut {
                reason: reason.to_owned(),
                deadline,
            });
        }
        thread::sleep(pause.min(deadline - elapsed));
        pause = (pause * 2).min(LONGEST_PAUSE);
    }
}

/// [`try_wait_until`] for a test body: a timeout fails the test with the reason in the panic
/// message. Pinned by `should_panic_with_the_reason_when_wait_until_times_out`.
///
/// # Panics
///
/// When the condition did not hold within `deadline`.
#[track_caller]
pub fn wait_until(deadline: Duration, reason: &str, condition: impl FnMut() -> bool) {
    if let Err(timed_out) = try_wait_until(deadline, reason, condition) {
        panic!("{timed_out}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const SHORT: Duration = Duration::from_millis(30);
    const LONG: Duration = Duration::from_secs(10);

    #[test]
    fn should_return_at_once_when_the_condition_already_holds() {
        // given
        let checks = Cell::new(0_u32);

        // when
        let result = try_wait_until(LONG, "already true", || {
            checks.set(checks.get() + 1);
            true
        });

        // then
        assert_eq!(result, Ok(()));
        assert_eq!(checks.get(), 1);
    }

    #[test]
    fn should_return_when_the_condition_holds_after_several_checks() {
        // given
        let checks = Cell::new(0_u32);

        // when
        let result = try_wait_until(LONG, "third check", || {
            checks.set(checks.get() + 1);
            checks.get() == 3
        });

        // then
        assert_eq!(result, Ok(()));
        assert_eq!(checks.get(), 3);
    }

    #[test]
    fn should_see_a_change_made_by_another_thread_when_it_is_waited_on() {
        // given
        let flag = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&flag);
        let worker = thread::spawn(move || setter.store(true, Ordering::SeqCst));

        // when
        let result = try_wait_until(LONG, "the worker's flag", || flag.load(Ordering::SeqCst));
        worker.join().expect("worker panicked");

        // then
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn should_report_the_reason_when_the_condition_never_holds() {
        // given
        let deadline = SHORT;

        // when
        let result = try_wait_until(deadline, "a socket to appear", || false);

        // then
        let timed_out = result.expect_err("a false condition must time out");
        assert_eq!(timed_out.reason(), "a socket to appear");
        assert_eq!(timed_out.deadline(), deadline);
        assert_eq!(
            timed_out.to_string(),
            "timed out after 30ms waiting for a socket to appear"
        );
    }

    #[test]
    fn should_panic_with_the_reason_when_wait_until_times_out() {
        // given
        let wait = || wait_until(SHORT, "the daemon to exit", || false);

        // when
        let panic = catch_unwind(AssertUnwindSafe(wait)).expect_err("must panic");

        // then
        let message = panic
            .downcast_ref::<String>()
            .expect("a formatted panic message");
        assert!(message.contains("the daemon to exit"), "{message}");
        assert!(message.contains("timed out"), "{message}");
    }

    #[test]
    fn should_not_panic_when_wait_until_sees_the_condition() {
        // given
        let checks = Cell::new(0_u32);

        // when
        wait_until(LONG, "second check", || {
            checks.set(checks.get() + 1);
            checks.get() >= 2
        });

        // then
        assert_eq!(checks.get(), 2);
    }
}
