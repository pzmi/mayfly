//! `mayfly-control` (control plane): Control-plane application: command bus, AggregateStore, projections, queries, reactors, reconciler, listener and outbox relay, with the ports Journal, Clock, Ids, ScaleSetApi, RunnerRegistry and SecretStore.
//!
//! - [`journal`]: the `Journal` port and the unit of work it commits atomically (plan §5.2).

pub mod journal;
