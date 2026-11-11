//! The `QuarantineCase` aggregate through its public API: the Phase × Command matrix (with
//! every actor), the rules of `doc/domain-model.md` §6.10 and properties over random command
//! sequences: no case resolves without an operator, and escalation never acts.

mod matrix;
mod properties;
mod rules;
mod support;
