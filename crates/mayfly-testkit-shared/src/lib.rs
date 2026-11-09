//! `mayfly-testkit-shared` (test plane, shared): test support that belongs to neither plane.
//!
//! It depends on no workspace crate (ADR 0019), so the control plane's `mayfly-testkit`, the
//! data plane's kit and any plane's own tests can use it without crossing the plane split.
//!
//! - [`poll`]: the bounded wait for a real-OS condition, the only place test support may sleep
//!   (`ARCH-test-sleep`).

pub mod poll;
