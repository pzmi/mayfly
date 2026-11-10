//! `mayfly-testkit` (test plane): test-only harnesses, fakes and contract suites.
//!
//! - [`golden`]: every registered event kind has its byte-identical (or, for a legacy kind,
//!   correctly upcast) JSON samples, together showing every field and variant of its traced
//!   shape, and no fixture is unregistered (ver §4).
//! - [`determinism`]: every aggregate's `decide` gives equal results from equal states,
//!   refusals included, over accepted and refused cases, and its outcomes and refusal codes
//!   match its decisions golden (ver §3.1).
//! - [`journal`]: `MemoryJournal`, the in-memory `Journal` (ver §7.1).
//! - [`clock`]: `FakeClock`, `SeededRng` and `RecordingNotifier`, fakes for `AggregateStore`'s
//!   ports (ver §3.2, §3.3).
//! - [`secrets`]: `MemorySecretProvider`, the in-memory `SecretProvider` (ver §7.1).
//! - [`contracts`]: the port contract suites: the journal's and the secret providers' (ver §5).
//! - [`block_on`](block_on()): drives an async port from a synchronous test.
//!
//! The golden and determinism harnesses walk an `AggregateRegistry`, so an aggregate that joins
//! `mayfly_control_domain::ControlAggregates` is covered with no change here.
//!
//! Tests of these harnesses live in `tests/`, and a crate whose aggregates they check (such as
//! `mayfly-control-domain`) runs them from its own `tests/`, never from `#[cfg(test)]` in
//! `src/`: that crate dev-depends on the testkit, which depends on it, so a unit test in its
//! `src/` would be compiled against a second copy of the crate whose types the testkit does not
//! accept.

mod block_on;
pub mod clock;
pub mod contracts;
pub mod determinism;
pub mod golden;
pub mod journal;
pub mod secrets;

pub use block_on::block_on;
