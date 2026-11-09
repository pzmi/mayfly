//! Fixture: a platform chosen at run time with `cfg!` instead of `#[cfg]`.

pub fn launcher() -> &'static str {
    if cfg!(target_os = "macos") { "open" } else { "xdg-open" }
}
