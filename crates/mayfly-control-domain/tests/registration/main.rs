//! The `RunnerRegistration` aggregate through its public API: the Phase × Command matrix, the
//! rules of `doc/domain-model.md` §6.5 and P10–P14 that `formal/registration.qnt` states as
//! invariants, and policy P11's `retirement_step`.

mod matrix;
mod properties;
mod rules;
mod support;
