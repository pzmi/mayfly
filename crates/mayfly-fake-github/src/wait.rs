//! The fake's only real-time waits (ADR 0023): a poll's hold until its deadline, and a delay
//! fault. Waiting is what the fake fakes here; its tests run on a paused Tokio clock and never
//! wait themselves.

use std::time::Duration;

use tokio::time::Instant;

/// Waits until `deadline`.
pub(crate) async fn until(deadline: Instant) {
    tokio::time::sleep_until(deadline).await;
}

/// Waits `delay`.
pub(crate) async fn delay(delay: Duration) {
    tokio::time::sleep(delay).await;
}
