//! The repository root, resolved from this crate's own location rather than the current
//! directory, so a stage run from a subdirectory checks the same files as one run from the
//! root (site-preview ADR 0042).

use std::path::{Path, PathBuf};

/// The repository root: two levels above `tools/xtask`.
pub fn root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .nth(2)
        .unwrap_or(manifest)
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_find_workspace_manifest_when_root_is_resolved() {
        // given
        let root = root();

        // when
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("root manifest");

        // then
        assert!(manifest.contains("[workspace]"), "{}", root.display());
        assert!(root.join("tools/xtask/Cargo.toml").is_file());
    }
}
