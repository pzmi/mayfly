//! The bounded polling helper: the one place test code may sleep.

use std::thread;
use std::time::{Duration, Instant};

pub fn wait_until(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while !condition() {
        if start.elapsed() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    true
}
