//! The data-plane domain through its public API: the session, executor, ledger and supervisor
//! tables (`matrix`), the rules of each `Accept:` clause of KER-08 (`executor`, `ledger`) and
//! the supervisor and inventory rules of I25 (`supervisor`).

mod executor;
mod ledger;
mod matrix;
mod supervisor;
mod support;
