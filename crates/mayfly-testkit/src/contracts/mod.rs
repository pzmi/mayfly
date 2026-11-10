//! Contract suites (ver §5): one suite per port (the journal's, the delivery ledger's, the projection store's, the secret providers'), written once over a factory and run from each
//! implementation's crate, with a `check_*` form that reports failed cases so the suite's own
//! tests can show a deliberately broken implementation failing.

pub mod delivery;
pub mod journal;
pub mod projections;
pub mod secrets;
