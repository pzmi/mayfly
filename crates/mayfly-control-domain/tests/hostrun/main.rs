//! The `HostRun` aggregate through its public API: the Phase × Command matrix, the rules of
//! `doc/domain-model.md` §6.8 (I17, safe mode's exits, one run at a time, the epoch fence) and
//! properties over random command sequences.

mod matrix;
mod properties;
mod rules;
mod support;
