//! Fixture: a pure crate keeps per-thread mutable state in `thread_local!`.

thread_local! {
    static SEEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub fn seen() -> u64 {
    SEEN.with(std::cell::Cell::get)
}
