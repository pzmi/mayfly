//! The testkit harnesses' own tests: each proves its harness passes a correct aggregate and
//! fails a deliberately broken one (one test binary, so the shared test aggregates are all
//! used).

mod determinism;
mod event_set;
mod golden;
mod support;
