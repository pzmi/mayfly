//! `mayfly-testkit` (test plane): test-only harnesses, fakes and contract suites.
//!
//! - [`golden`]: every registered event kind has exactly one byte-identical JSON fixture and
//!   no fixture is unregistered (ver §4).
//! - [`determinism`]: every aggregate's `decide` gives equal results from equal states,
//!   refusals included, over accepted and refused cases (ver §3.1).
//! - [`journal`]: `MemoryJournal`, the in-memory `Journal` (ver §7.1).
//! - [`contracts`]: the port contract suites, starting with the journal's (ver §5).
//! - [`block_on`](block_on()): drives an async port from a synchronous test.
//!
//! The golden and determinism harnesses walk an `AggregateRegistry`, so an aggregate that joins
//! `mayfly_control_domain::ControlAggregates` is covered with no change here.

mod block_on;
pub mod contracts;
pub mod determinism;
pub mod golden;
pub mod journal;

pub use block_on::block_on;
