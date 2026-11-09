//! The `Host` aggregate through its public API: the State × Command matrix, the admission
//! rules of `doc/domain-model.md` §6.3 that `formal/scheduling.qnt` states as invariants, and
//! properties over random command sequences.

mod matrix;
mod properties;
mod rules;
mod support;
