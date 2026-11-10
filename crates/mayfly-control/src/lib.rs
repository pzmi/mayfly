//! `mayfly-control` (control plane): Control-plane application: command bus, AggregateStore, projections, queries, reactors, reconciler, listener and durable consumers (directive relay, attempt saga), with the ports Journal, Clock, Ids, ScaleSetApi, RunnerRegistry and SecretProvider.
//!
//! - [`journal`]: the `Journal` port and the unit of work it commits atomically (plan §5.2,
//!   `doc/event-store.md` §3).
//! - [`store`]: `AggregateStore`, the one write path over the journal, with its `Clock`, `Rng`
//!   and `StoreNotifier` ports (`doc/event-store.md` §3.7).
//! - [`delivery`]: durable delivery: the `DeliveryLedger` port, the `Consumer` trait, the
//!   `DeliveryWorker` with `catch_up` and kicks, and the `directive_relay` consumer
//!   (`doc/event-store.md` §3.4, §5).
//! - [`secrets`]: the `SecretProvider` port, its refusals and the `doctor` check (ADR 0033).

pub mod delivery;
pub mod journal;
pub mod secrets;
pub mod store;
