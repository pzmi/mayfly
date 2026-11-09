//! Fixture: a pure crate mints ids from a global atomic counter, state that survives across
//! decisions and makes them depend on call order.

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub fn next_id() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}
