//! Positive fixture: tooling may touch the filesystem.

pub fn read(path: &std::path::Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}
