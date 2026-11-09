//! Fixture: a module whose only #[path] is conditional; the default file is a branch too.

#[cfg_attr(target_os = "macos", path = "mac.rs")]
mod imp;
