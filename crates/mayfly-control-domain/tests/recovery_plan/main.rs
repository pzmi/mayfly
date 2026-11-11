//! The `RecoveryPlan` aggregate through its public API: the Phase × Command matrix, the rules
//! of `doc/domain-model.md` §6.9 (I31's bindings, F45, intent then outcome, actions only from
//! the recovery table's proposals, one operator approval) and properties over random command
//! sequences.

mod matrix;
mod properties;
mod rules;
mod support;
