//! The `ScaleSet` aggregate through its public API: the Phase × Command matrix, the rules of
//! `doc/domain-model.md` §6.1 and plan §7.2 that `formal/scaleset.qnt` states as invariants, and
//! those invariants over random message sequences.

mod matrix;
mod properties;
mod rules;
mod support;
