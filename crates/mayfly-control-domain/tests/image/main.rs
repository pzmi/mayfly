//! The `Image` aggregate through its public API: the Status × Command matrix, the rules of
//! `doc/domain-model.md` §6.6 (immutability, monotonic status, runner expiry, epoch fence) and
//! the properties over random command sequences.

mod matrix;
mod properties;
mod rules;
mod support;
