//! Fixture: a pure crate reads the clock through `UNIX_EPOCH.elapsed()`, no `now` in sight.

pub fn age_millis() -> u128 {
    let epoch = std::time::UNIX_EPOCH;
    epoch.elapsed().map_or(0, |d| d.as_millis())
}
