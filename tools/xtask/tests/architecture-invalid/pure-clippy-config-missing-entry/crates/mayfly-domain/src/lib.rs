//! Fixture: a pure crate's clippy.toml lost an entry, so `path.exists()` passes Clippy.

pub fn present(path: &std::path::Path) -> bool {
    path.exists()
}
