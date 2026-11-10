//! `mayfly-adapter-tart` (data plane): the `VmProvider` port over the Tart CLI (plan §8.2, D12).
//!
//! - [`provider`]: `TartProvider`, the port's implementation, bounded by per-call deadlines.
//! - [`tart`]: the `tart` commands it runs; [`parse`]: their output, against version-pinned
//!   fixtures.
//! - [`process`]: the process runner (argv only, cleared environment plus an allowlist,
//!   deadline, process-group kill, bounded and redacted output), the shim's launch, and the
//!   shim itself, the body of `mayfly vm-shim` (ADR 0037).
//! - [`shim`]: a shim's directory and the re-adoption inputs it gives a restarted data plane.
//! - [`config`]: what composition passes in.
//!
//! Never a real `tart` in tests: the contract runs against `mayfly-fake-tart`, and against real
//! Tart only in the hardware lane (HW-01).

pub mod config;
pub mod parse;
pub mod process;
pub mod provider;
pub mod shim;
pub mod tart;

pub use config::{Deadlines, ShimProgram, TartConfig};
pub use provider::{ProbeError, TartProvider};
pub use shim::ShimReading;
