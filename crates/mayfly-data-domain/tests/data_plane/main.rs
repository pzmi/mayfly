//! The data-plane domain through its public API: the session, executor, ledger and supervisor
//! tables (`matrix`), the rules of each `Accept:` clause of KER-08 (`executor`, `ledger`) and
//! the supervisor and inventory rules of I25 (`supervisor`), and the runner watch of DP-09
//! (`runners`).

mod executor;
mod ledger;
mod matrix;
mod runners;
mod supervisor;
mod support;
