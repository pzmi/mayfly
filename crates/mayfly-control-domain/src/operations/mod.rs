//! The Operations context (`doc/domain-model.md` §3): how one host recovers, contains doubt
//! and is repaired. [`recovery`] holds the startup decision table (plan §10); the `HostRun`,
//! `RecoveryPlan` and `QuarantineCase` aggregates and the mode precedence function follow.

pub mod recovery;
