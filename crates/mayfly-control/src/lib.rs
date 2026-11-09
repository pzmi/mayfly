//! `mayfly-control` (control plane): Control-plane application: command bus, AggregateStore, projections, queries, reactors, reconciler, listener and durable consumers (directive relay, attempt saga), with the ports Journal, Clock, Ids, ScaleSetApi, RunnerRegistry and SecretStore.
//!
//! - [`journal`]: the `Journal` port and the unit of work it commits atomically (plan §5.2,
//!   `doc/event-store.md` §3).
//! - [`store`]: `AggregateStore`, the one write path over the journal, with its `Clock`, `Rng`
//!   and `StoreNotifier` ports (`doc/event-store.md` §3.7).

pub mod journal;
pub mod store;
