//! Contract suites (ver §5): one suite per port, written once over a factory and run from each
//! implementation's crate, with a `check_*` form that reports failed cases so the suite's own
//! tests can show a deliberately broken implementation failing.

pub mod journal;
