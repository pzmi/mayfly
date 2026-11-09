//! Positive fixture: test support may touch the filesystem, and its poll helper may sleep.

pub mod poll;

pub fn scratch() -> std::path::PathBuf {
    std::env::temp_dir()
}
