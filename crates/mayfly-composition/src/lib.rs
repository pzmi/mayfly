//! `mayfly-composition` (shared plane): config, wiring, runtime, supervision tree, startup
//! sequence and roles.
//!
//! - [`config`]: the configuration schema and its validation (plan §11).
//! - [`state_dir`]: the state-dir lock, the store's owner lease on SQLite (plan §13).
//! - [`daemon`]: `mayflyd --role all`, both planes wired over the in-process link, the
//!   start-up sequence of plan §10 and the P24 stop (RT-02, ADR 0091).
//! - [`supervise`]: the supervision tree's root cancellation, signals and the store's
//!   after-commit notifier.
//! - [`clocks`], [`identity`], [`details`]: the production clocks and randomness, the host's
//!   persisted identity, and what a directive needs from the configuration.
//! - [`paths`]: the platform's standard places when the configuration names none.
//! - [`vm_shim`]: the per-VM shim's entry (ADR 0037).
//! - [`exit_code`]: the daemon's distinct exit codes (ADR 0031).

pub mod clocks;
pub mod config;
pub mod daemon;
pub mod details;
pub mod exit_code;
pub mod identity;
pub mod paths;
pub mod state_dir;
pub mod supervise;
pub mod vm_shim;
