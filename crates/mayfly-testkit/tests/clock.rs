//! The port fakes `AggregateStore` is tested with.

use mayfly_control::store::{Clock, Rng};
use mayfly_domain::Millis;
use mayfly_testkit::block_on;
use mayfly_testkit::clock::{FakeClock, SeededRng};

#[test]
fn should_repeat_draws_when_the_seed_is_the_same() {
    // given
    let (first, second, other) = (SeededRng::new(7), SeededRng::new(7), SeededRng::new(8));

    // when
    let a: Vec<u64> = (0..16).map(|_| first.below(1_000)).collect();
    let b: Vec<u64> = (0..16).map(|_| second.below(1_000)).collect();
    let c: Vec<u64> = (0..16).map(|_| other.below(1_000)).collect();

    // then
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert!(a.iter().all(|v| *v < 1_000));
    assert_eq!(first.below(0), 0);
}

#[test]
fn should_advance_only_when_told_or_slept_on_when_the_clock_is_fake() {
    // given
    let clock = FakeClock::at(Millis::new(1_000));

    // when
    let before = clock.now();
    clock.advance(Millis::new(5));
    block_on(clock.sleep(Millis::new(20)));

    // then
    assert_eq!(before, Millis::new(1_000));
    assert_eq!(clock.now(), Millis::new(1_025));
    assert_eq!(clock.sleeps(), [Millis::new(20)]);
}
