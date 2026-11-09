//! The repository root, resolved from this crate's own location rather than the current
//! directory, so a stage run from a subdirectory checks the same files as one run from the
//! root (site-preview ADR 0042).
//!
//! The location is compiled in (`CARGO_MANIFEST_DIR`), so a binary built from one checkout
//! and run from another (a target directory shared between worktrees) would check the wrong
//! tree: [`preflight`] refuses that, and a `CLIPPY_CONF_DIR` that would point Clippy past the
//! pure crates' `clippy.toml`.

use std::ffi::OsStr;
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

/// Why `cargo xtask` must not run in `cwd` with `clippy_conf_dir` set as given, if it must
/// not: `cwd` lies outside `root` (the checkout this binary was built from), or
/// `CLIPPY_CONF_DIR` is set. Pinned by `should_refuse_when_run_outside_its_checkout` and
/// `should_refuse_when_clippy_conf_dir_is_set`.
pub fn preflight(root: &Path, cwd: &Path, clippy_conf_dir: Option<&OsStr>) -> Result<(), String> {
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (root, cwd) = (canonical(root), canonical(cwd));
    if !cwd.starts_with(&root) {
        return Err(format!(
            "this xtask was built from {} but runs in {}, outside that checkout: it would check the other tree; run `cargo xtask` from inside the checkout to check (a shared CARGO_TARGET_DIR does not make one binary serve two checkouts)",
            root.display(),
            cwd.display()
        ));
    }
    if let Some(dir) = clippy_conf_dir {
        return Err(format!(
            "CLIPPY_CONF_DIR is set ({}): Clippy would read that configuration instead of each pure crate's clippy.toml, switching off the semantic purity layer (foundation.md section 2.3); unset it",
            dir.to_string_lossy()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_refuse_when_run_outside_its_checkout() {
        // given
        let elsewhere = tempfile::TempDir::new().unwrap();
        let root = root();

        // when
        let outside = preflight(&root, elsewhere.path(), None);
        let inside = preflight(&root, &root.join("tools/xtask"), None);

        // then
        let err = outside.unwrap_err();
        assert!(
            err.contains(&root.canonicalize().unwrap().display().to_string()),
            "{err}"
        );
        assert!(
            err.contains(
                &elsewhere
                    .path()
                    .canonicalize()
                    .unwrap()
                    .display()
                    .to_string()
            ),
            "{err}"
        );
        assert_eq!(inside, Ok(()));
    }

    #[test]
    fn should_refuse_when_clippy_conf_dir_is_set() {
        // given
        let root = root();

        // when
        let got = preflight(&root, &root, Some(OsStr::new("/elsewhere")));

        // then
        assert!(
            got.unwrap_err()
                .contains("CLIPPY_CONF_DIR is set (/elsewhere)")
        );
    }

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
