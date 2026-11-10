//! `mayfly-conformance` (test plane): Quint ITF decoding, per-model drivers and trace replay
//! (ver §6.2). Control-plane test infrastructure: no plane crate dev-depends on it (ADR 0019).
//!
//! - [`itf`]: the narrow ITF decoder and the MBT variables (`mbt::actionTaken`,
//!   `mbt::nondetPicks`).
//! - [`manifest`]: the committed regression manifest and the generated ones.
//! - [`replay`]: the [`replay::Driver`] contract, the canonical [`replay::Observation`], the
//!   per-step comparison (initial state included) and the [`replay::Divergence`] report.
//! - [`lifecycle`]: `formal/lifecycle.qnt` against the `Attempt` aggregate through
//!   `AggregateStore` over `MemoryJournal`.
//! - [`run`]: replays every trace of a manifest with the driver of its model; a model without a
//!   driver yet (`event_store` until ES-07; the VER-02 models) is decoded and checked, and
//!   reported as such, never counted as replayed.
//!
//! Traces are made by `cargo xtask conformance` (generated, seeded) and committed under
//! `formal/traces/regression/` (pinned by `formal-freshness`); the binary `mayfly-conformance`
//! replays a manifest for xtask, and `tests/` replays the committed lifecycle traces and runs
//! the negative controls.

pub mod itf;
pub mod lifecycle;
pub mod manifest;
pub mod replay;
pub mod run;
