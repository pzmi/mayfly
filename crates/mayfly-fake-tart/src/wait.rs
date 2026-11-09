//! The fake's one real-time wait.
//!
//! A fault's `delay_ms`, an `exec` job, `ip --wait` and `stop`'s grace period are real time by
//! nature: it is the behaviour being faked, not a test waiting for something. ADR 0023 lets this
//! file, and only this file, sleep despite `ARCH-test-sleep`.

use std::time::Duration;

/// Blocks for `length`.
pub(crate) fn pause(length: Duration) {
    std::thread::sleep(length);
}
