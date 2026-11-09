//! Positive fixture: test support may touch the filesystem.

pub fn scratch() -> std::path::PathBuf {
    std::env::temp_dir()
}
