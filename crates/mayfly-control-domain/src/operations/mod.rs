//! The Operations context (`doc/domain-model.md` §3): how one host recovers, contains doubt
//! and is repaired. [`recovery`] holds the startup decision table (plan §10); [`hostrun`] the
//! `HostRun` aggregate (runs, epochs, safe mode), [`plan`] the `RecoveryPlan` aggregate,
//! [`quarantine`] the `QuarantineCase` aggregate and [`mode`] the pure precedence of the
//! effective operating mode.

pub mod hostrun;
pub mod mode;
pub mod plan;
pub mod quarantine;
pub mod recovery;

pub use mode::{EffectiveMode, ModeFlags, PRECEDENCE, causes, is_set, mode};
