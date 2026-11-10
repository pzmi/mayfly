//! `mayfly-testkit-data` (test plane, data): the data plane's test kit (ADR 0019).
//!
//! Its closure is the data plane and the shared crates, so data-plane crates may dev-depend on
//! it and control-plane crates may not. Every fake is a cheap handle: clones share one state, so
//! a test keeps a handle while the executor owns another. Nothing here sleeps or spawns: time
//! moves only when a test advances the [`FakeClock`], and a call held by the
//! [`FakeVmProvider`] waits until the test releases it.
//!
//! - [`provider`]: `FakeVmProvider`, scripted faults per call, held calls, instance-id changes.
//! - [`host`]: the fake image store, guest transport, host probe and floor.
//! - [`clock`]: `FakeClock` and `SequentialIds`.
//! - [`link`]: an in-memory directive source, observation sink and handshake for the executor.

pub mod clock;
pub mod host;
pub mod link;
pub mod provider;

pub use clock::{FakeClock, SequentialIds};
pub use host::{FakeGuest, FakeHostProbe, FakeImageStore, GuestScript, MemoryFloor};
pub use link::{DirectiveFeed, FeedSource, StaticWelcome, Tap, TapSink, directives, observations};
pub use provider::{FakeVm, FakeVmProvider, Fault, Held, Op};
