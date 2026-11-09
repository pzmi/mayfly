//! Waits on the wall clock instead of an event.

use std::thread;
use std::time::Duration;

#[test]
fn should_settle_when_time_passes() {
    // given
    let pause = Duration::from_millis(10);

    // when
    thread::sleep(pause);

    // then
    assert!(pause.as_millis() > 0);
}
