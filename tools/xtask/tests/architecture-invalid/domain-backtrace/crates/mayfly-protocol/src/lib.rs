//! Fixture: a pure crate captures a backtrace, which reads `RUST_BACKTRACE` from the
//! environment.

pub fn trace() -> String {
    std::backtrace::Backtrace::capture().to_string()
}
