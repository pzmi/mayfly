//! `mayfly-testkit-shared` (test plane, shared): test support that belongs to neither plane.
//!
//! It depends on the shared crates only (`mayfly-domain`, `mayfly-protocol`; ADR 0019), so
//! the control plane's `mayfly-testkit`, the data plane's kit, any plane's own tests and the
//! shared crates' tests can use it without crossing the plane split.
//!
//! - [`poll`]: the bounded wait for a real-OS condition, the only place test support may sleep
//!   (`ARCH-test-sleep`).
//! - [`child_env`]: the probe that a child got a cleared environment, for any contract whose
//!   port runs a child (ADR 0058).
//! - [`fixtures`]: reads a tree of golden fixtures for a crate whose tests may not touch the
//!   file system (the protocol's golden harness, ver section 4).
//! - [`link_contract`]: the link contract suite every CP↔DP transport passes (ver section 5,
//!   DP-02).

pub mod child_env;
pub mod fixtures;
pub mod link_contract;
pub mod poll;
