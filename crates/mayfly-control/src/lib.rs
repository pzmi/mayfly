//! `mayfly-control` (control plane): Control-plane application: command bus, AggregateStore, projections, queries, reactors, reconciler, listener and durable consumers (directive relay, attempt saga), with the ports Journal, Clock, Ids, ScaleSetApi, RunnerRegistry and SecretProvider.
//!
//! - [`journal`]: the `Journal` port and the unit of work it commits atomically (plan §5.2,
//!   `doc/event-store.md` §3).
//! - [`store`]: `AggregateStore`, the one write path over the journal, with its `Clock`, `Rng`
//!   and `StoreNotifier` ports (`doc/event-store.md` §3.7).
//! - [`projection`]: projection specs, their inline folds, the `ProjectionStore` and
//!   `ProjectionRebuild` ports, the offline rebuild and the first read models (`doc/event-store.md`
//!   §3.6).
//! - [`queries`]: cross-stream views computed at query time over projection rows.
//! - [`delivery`]: durable delivery: the `DeliveryLedger` port, the `Consumer` trait, the
//!   `DeliveryWorker` with `catch_up` and kicks, the `directive_relay` consumer and its sink
//!   over the link (`doc/event-store.md` §3.4, §5).
//! - [`placement`]: the image placement manager (P17, P18): pins placed on every host, bases
//!   materialised and evicted, runner-version expiry tripping the breaker, withdrawal refused
//!   while pinned (CP-07).
//! - [`reconciler`]: the reconciler (P5): a pass that reads the fleet, plans with the pure
//!   `reconcile::plan_pass` and issues reservations, retirements, releases and capacity inputs.
//! - [`saga`]: the `attempt_saga` durable consumer: the next command per attempt (P6) and the
//!   deadline timeouts that make an unanswered directive `Unknown` (P7).
//! - [`sessions`]: the data-plane sessions by host: the epoch the relay stamps and the saga
//!   issues under, and the mode that holds effects (I29).
//! - [`start`]: policy P2, an attempt started from its host's lease under a read of the host
//!   stream at the revision the lease was read at.
//! - [`inbox`]: the observation inbox (P8): observations as the units they map to, keyed by
//!   data plane and observation, acked only after they commit.
//! - [`recovery`]: the startup use case (P1) and the application of a recovery plan (P20).
//! - [`secrets`]: the `SecretProvider` port, its refusals and the `doctor` check (ADR 0033).
//! - [`github`]: the `ScaleSetApi` and `RunnerRegistry` ports (plan §7.2, §7.3), their value
//!   types and failure classes (GH-03).
//! - [`terms`]: the terms policy (P4): each pool's terms applied on every host, at the pool's
//!   terms revision, by a level-triggered pass (CP-08).

pub mod delivery;
pub mod github;
mod ids;
pub mod inbox;
pub mod journal;
pub mod placement;
pub mod projection;
pub mod queries;
pub mod reconciler;
pub mod recovery;
pub mod saga;
pub mod secrets;
pub mod sessions;
pub mod start;
pub mod store;
pub mod terms;
