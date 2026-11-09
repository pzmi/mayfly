//! `mayfly-testkit` (test plane): test-only harnesses and, from later tasks on, the fake clock,
//! the in-memory journal, fake ports and the scenario builder.
//!
//! - [`golden`]: every registered event kind has exactly one byte-identical JSON fixture and
//!   no fixture is unregistered (ver §4).
//! - [`determinism`]: every aggregate's `decide` gives equal results from equal states,
//!   refusals included, over accepted and refused cases (ver §3.1).
//!
//! Both walk an `AggregateRegistry`, so an aggregate that joins
//! `mayfly_control_domain::ControlAggregates` is covered with no change here.

pub mod determinism;
pub mod golden;
