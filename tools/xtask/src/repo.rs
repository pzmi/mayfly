//! The repository root, resolved from this crate's own location rather than the current
//! directory, so a stage run from a subdirectory checks the same files as one run from the
//! root (site-preview ADR 0042).
//!
//! The location is compiled in (`CARGO_MANIFEST_DIR`), so a binary built from one checkout
//! and run from another (a target directory shared between worktrees) would check the wrong
//! tree: [`preflight`] refuses that, and every environment variable that would steer the
//! compiler, its flags or Clippy's configuration past what the gate checks ([`steering`]).

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

/// The value of `RUSTFLAGS` the gate accepts: it only denies warnings, which CI's native
/// macOS job sets (`.github/workflows/ci.yml`). Any other value is refused.
pub const BENIGN_RUSTFLAGS: &[&str] = &["-D warnings", "-Dwarnings"];

/// Whether the environment variable `name`, set to `value`, would change what the gate's
/// builds compile or lint: the compiler or a wrapper around it (`RUSTC`, `RUSTC_WRAPPER`,
/// `RUSTC_WORKSPACE_WRAPPER`, `RUSTDOC`), its flags (`RUSTFLAGS` other than
/// [`BENIGN_RUSTFLAGS`], `RUSTDOCFLAGS`, the encoded and `CARGO_BUILD_*` forms of these, per-target
/// rustflags and runners), nightly features (`RUSTC_BOOTSTRAP`, `CARGO_UNSTABLE_*`), Clippy's
/// configuration and arguments (`CLIPPY_*`), or a Cargo alias that could shadow a subcommand
/// (`CARGO_ALIAS_*`). Fail closed: refused, never cleared and warned about. Pinned by
/// `should_refuse_when_the_environment_steers_the_build`.
pub fn steering(name: &str, value: &OsStr) -> bool {
    if name == "RUSTFLAGS" {
        let flags = value.to_string_lossy();
        let flags = flags.split_whitespace().collect::<Vec<_>>().join(" ");
        return !BENIGN_RUSTFLAGS.contains(&flags.as_str());
    }
    name.starts_with("CLIPPY_")
        || name.starts_with("CARGO_ALIAS_")
        || name.starts_with("CARGO_UNSTABLE_")
        || (name.starts_with("CARGO_TARGET_")
            && (name.ends_with("_RUNNER") || name.ends_with("_RUSTFLAGS")))
        || matches!(
            name,
            "RUSTC"
                | "RUSTC_WRAPPER"
                | "RUSTC_WORKSPACE_WRAPPER"
                | "RUSTC_BOOTSTRAP"
                | "RUSTDOC"
                | "RUSTDOCFLAGS"
                | "CARGO_ENCODED_RUSTFLAGS"
                | "CARGO_ENCODED_RUSTDOCFLAGS"
                | "CARGO_BUILD_RUSTC"
                | "CARGO_BUILD_RUSTC_WRAPPER"
                | "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"
                | "CARGO_BUILD_RUSTDOC"
                | "CARGO_BUILD_RUSTFLAGS"
                | "CARGO_BUILD_RUSTDOCFLAGS"
        )
}

/// Why `cargo xtask` must not run in `cwd` with the environment `vars`, if it must not:
/// `cwd` lies outside `root` (the checkout this binary was built from), or a variable steers
/// the build ([`steering`]; `CLIPPY_CONF_DIR` would point Clippy past each pure crate's
/// clippy.toml). Pinned by `should_refuse_when_run_outside_its_checkout`,
/// `should_refuse_when_clippy_conf_dir_is_set` and
/// `should_refuse_when_the_environment_steers_the_build`.
pub fn preflight<'a>(
    root: &Path,
    cwd: &Path,
    vars: impl IntoIterator<Item = (&'a OsStr, &'a OsStr)>,
) -> Result<(), String> {
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (root, cwd) = (canonical(root), canonical(cwd));
    if !cwd.starts_with(&root) {
        return Err(format!(
            "this xtask was built from {} but runs in {}, outside that checkout: it would check the other tree; run `cargo xtask` from inside the checkout to check (a shared CARGO_TARGET_DIR does not make one binary serve two checkouts)",
            root.display(),
            cwd.display()
        ));
    }
    let mut refused: Vec<String> = Vec::new();
    for (name, value) in vars {
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == "CLIPPY_CONF_DIR" {
            return Err(format!(
                "CLIPPY_CONF_DIR is set ({}): Clippy would read that configuration instead of each pure crate's clippy.toml, switching off the semantic purity layer (foundation.md section 2.3); unset it",
                value.to_string_lossy()
            ));
        }
        if steering(name, value) {
            refused.push(name.to_owned());
        }
    }
    if !refused.is_empty() {
        refused.sort();
        return Err(format!(
            "{} set: each changes the compiler, its flags, Clippy's configuration or a cargo subcommand, so the gate would pass over code it never checked (foundation.md section 3.6); unset {} (RUSTFLAGS may only be `-D warnings`)",
            refused.join(", "),
            if refused.len() == 1 { "it" } else { "them" }
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
        let outside = preflight(&root, elsewhere.path(), []);
        let inside = preflight(&root, &root.join("tools/xtask"), []);

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
        let got = preflight(
            &root,
            &root,
            [(OsStr::new("CLIPPY_CONF_DIR"), OsStr::new("/elsewhere"))],
        );

        // then
        assert!(
            got.unwrap_err()
                .contains("CLIPPY_CONF_DIR is set (/elsewhere)")
        );
    }

    #[test]
    fn should_refuse_when_the_environment_steers_the_build() {
        // given: what a shell, a CI step or cargo-llvm-cov can put in front of the gate
        let root = root();
        let steering = [
            ("RUSTFLAGS", "-A warnings"),
            ("RUSTFLAGS", "-Zcrate-attr=allow(clippy::all)"),
            ("RUSTC_WRAPPER", "/bin/true"),
            ("RUSTC_WORKSPACE_WRAPPER", "w"),
            ("RUSTC", "r"),
            ("RUSTC_BOOTSTRAP", "1"),
            ("CLIPPY_ARGS", "-Aclippy::all"),
            ("CARGO_ENCODED_RUSTFLAGS", "-Awarnings"),
            ("CARGO_BUILD_RUSTFLAGS", "-Awarnings"),
            ("CARGO_BUILD_RUSTC_WRAPPER", "w"),
            ("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER", "true"),
            ("CARGO_ALIAS_CLIPPY", "check"),
        ];
        let benign = [
            ("RUSTFLAGS", "-D  warnings"),
            ("CARGO_TARGET_DIR", "/t"),
            ("CARGO_TERM_COLOR", "always"),
            ("RUSTUP_TOOLCHAIN", "1.97.0"),
            ("CARGO_BUILD_JOBS", "2"),
        ];

        // when
        let refused: Vec<bool> = steering
            .iter()
            .map(|(k, v)| preflight(&root, &root, [(OsStr::new(k), OsStr::new(v))]).is_err())
            .collect();
        let accepted = preflight(
            &root,
            &root,
            benign.iter().map(|(k, v)| (OsStr::new(*k), OsStr::new(*v))),
        );

        // then
        assert!(refused.iter().all(|r| *r), "{refused:?}");
        assert_eq!(accepted, Ok(()));
        let both = preflight(
            &root,
            &root,
            [
                (OsStr::new("RUSTC_WRAPPER"), OsStr::new("w")),
                (OsStr::new("CLIPPY_ARGS"), OsStr::new("a")),
            ],
        )
        .unwrap_err();
        assert!(both.starts_with("CLIPPY_ARGS, RUSTC_WRAPPER set"), "{both}");
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
