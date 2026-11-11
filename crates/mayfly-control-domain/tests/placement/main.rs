//! The `ImagePlacement` aggregate through its public API: the Phase × Command matrix, the rules
//! of `doc/domain-model.md` §6.7 and P17 (one pull per host and digest, bases named by digest and
//! runner version, eviction only unreferenced from `Materialised`, the base pointer with its
//! previous base for rollback) and the properties over random command sequences.

mod matrix;
mod properties;
mod rules;
mod support;
