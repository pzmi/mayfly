//! The Apalache release `formal-verify` runs, fetched and verified by the xtask instead of by
//! Quint (D15, foundation.md section 8.4).
//!
//! Quint downloads `apalache.tgz` on first use without checking it and runs whatever it finds
//! in `apalache-dist-<version>/apalache` under `$QUINT_HOME` (default `~/.quint`). Before
//! Quint runs, [`ensure_pinned`] therefore:
//!
//! 1. reuses the tarball in `target/apalache/` only when its SHA-256 equals the digest pinned
//!    in `tools/versions.toml`, and otherwise downloads it again with `curl --proto =https`;
//!    a download whose digest differs is deleted and the run fails, naming both digests;
//! 2. unpacks the verified tarball into a fresh directory beside the distribution and moves
//!    it into place with one rename, after removing whatever was there, so what Quint runs is
//!    always the content of a verified tarball and Quint never downloads anything itself.
//!
//! An unpacked tree is never trusted on its own: a cache or an earlier run could have left a
//! different one. Hashing uses `sha256sum`, or `shasum -a 256` on macOS; unpacking uses `tar`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{repo, tools};

/// The release asset Quint fetches for `version`.
pub fn release_url(version: &str) -> String {
    format!("https://github.com/apalache-mc/apalache/releases/download/v{version}/apalache.tgz")
}

/// Quint's configuration directory: `$QUINT_HOME`, else `~/.quint` (Quint's `config.js`).
pub fn quint_home() -> Option<PathBuf> {
    std::env::var_os("QUINT_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".quint")))
}

/// Where Quint looks for the release of `version`.
pub fn dist_dir(version: &str) -> Option<PathBuf> {
    quint_home().map(|home| home.join(format!("apalache-dist-{version}")))
}

/// The pinned version and digest from `tools/versions.toml`.
fn pin() -> Result<(String, String), String> {
    let tool = tools::manifest()?
        .into_iter()
        .find(|t| t.name == "apalache")
        .ok_or("tools/versions.toml has no apalache entry")?;
    let digest = tool.sha256.ok_or(
        "tools/versions.toml pins no sha256 for apalache; compute it from the release download \
         (D15)",
    )?;
    Ok((tool.version, digest))
}

/// Fetches, verifies and unpacks the pinned release where Quint looks for it; returns the
/// binary. Fails, never skips, when the download, the digest or the unpacking fails.
pub fn ensure_pinned(version: &str) -> Result<PathBuf, String> {
    let (pinned, digest) = pin()?;
    if pinned != version {
        return Err(format!(
            "Apalache {version} is not the pinned {pinned} of tools/versions.toml"
        ));
    }
    let dist = dist_dir(version).ok_or("neither QUINT_HOME nor HOME is set")?;
    let tarball = repo::root()
        .join("target")
        .join("apalache")
        .join(format!("apalache-{version}.tgz"));
    let url = release_url(version);
    install(&digest, &tarball, &dist, |dest| download(&url, dest))
}

/// [`ensure_pinned`] with the paths and the download given: `fetch` writes the release to the
/// path it is passed.
pub fn install(
    digest: &str,
    tarball: &Path,
    dist: &Path,
    fetch: impl Fn(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let cached = tarball.is_file() && sha256_of(tarball)? == digest;
    if !cached {
        if let Some(parent) = tarball.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let partial = tarball.with_extension("tgz.part");
        remove_file(&partial)?;
        fetch(&partial)?;
        let found = sha256_of(&partial)?;
        if found != digest {
            remove_file(&partial)?;
            return Err(format!(
                "the downloaded Apalache release has SHA-256 {found}, but tools/versions.toml \
                 pins {digest}; refusing to run it (D15)"
            ));
        }
        std::fs::rename(&partial, tarball)
            .map_err(|e| format!("cannot move the release to {}: {e}", tarball.display()))?;
    }
    unpack(tarball, dist)
}

/// Unpacks `tarball` into `dist/apalache`, replacing what was there.
fn unpack(tarball: &Path, dist: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dist).map_err(|e| format!("cannot create {}: {e}", dist.display()))?;
    let staging = dist.join(format!("verified-{}", std::process::id()));
    remove_dir(&staging)?;
    std::fs::create_dir(&staging)
        .map_err(|e| format!("cannot create {}: {e}", staging.display()))?;
    let unpacked = Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(&staging)
        .status()
        .map_err(|e| format!("cannot run tar: {e}"))?;
    let tree = staging.join("apalache");
    let binary = Path::new("bin").join("apalache-mc");
    if !unpacked.success() || !tree.join(&binary).is_file() {
        remove_dir(&staging)?;
        return Err(format!(
            "{} did not unpack to apalache/bin/apalache-mc",
            tarball.display()
        ));
    }
    let target = dist.join("apalache");
    remove_dir(&target)?;
    std::fs::rename(&tree, &target)
        .map_err(|e| format!("cannot move Apalache into {}: {e}", target.display()))?;
    remove_dir(&staging)?;
    Ok(target.join(binary))
}

/// Downloads `url` to `dest` over HTTPS only.
fn download(url: &str, dest: &Path) -> Result<(), String> {
    println!("formal-verify: fetching {url}");
    let status = Command::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location"])
        .args([
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--retry",
            "3",
        ])
        .arg("--output")
        .arg(dest)
        .arg(url)
        .status()
        .map_err(|e| format!("cannot run curl: {e}; install curl to fetch {url}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "cannot download {url} ({status}); allow github.com, or put the release at {}",
            dest.display()
        ))
    }
}

/// The SHA-256 of a file, as lowercase hex.
pub fn sha256_of(path: &Path) -> Result<String, String> {
    let attempts: [(&str, &[&str]); 2] = [("sha256sum", &[]), ("shasum", &["-a", "256"])];
    for (program, args) in attempts {
        let Ok(output) = Command::new(program).args(args).arg(path).output() else {
            continue;
        };
        if !output.status.success() {
            return Err(format!(
                "{program} failed on {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        return parse_digest(&text)
            .ok_or_else(|| format!("cannot read the output of {program}: {text:?}"));
    }
    Err("cannot hash a file: neither sha256sum nor shasum is installed".to_owned())
}

/// The digest at the start of `sha256sum`'s or `shasum`'s output line.
fn parse_digest(text: &str) -> Option<String> {
    let digest = text.split_whitespace().next()?.to_ascii_lowercase();
    tools::is_sha256(&digest).then_some(digest)
}

fn remove_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("cannot remove {}: {e}", path.display()))
        }
        _ => Ok(()),
    }
}

fn remove_dir(path: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("cannot remove {}: {e}", path.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A release tarball holding `apalache/bin/apalache-mc` with `body`, and its digest.
    fn release(dir: &Path, body: &str) -> (PathBuf, String) {
        let tree = dir.join("tree");
        std::fs::create_dir_all(tree.join("apalache/bin")).unwrap();
        std::fs::write(tree.join("apalache/bin/apalache-mc"), body).unwrap();
        let tarball = dir.join(format!("release-{}.tgz", body.len()));
        let ok = Command::new("tar")
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(&tree)
            .arg("apalache")
            .status()
            .unwrap()
            .success();
        assert!(ok, "tar could not build the test release");
        std::fs::remove_dir_all(&tree).unwrap();
        let digest = sha256_of(&tarball).unwrap();
        (tarball, digest)
    }

    fn copy_from(source: &Path) -> impl Fn(&Path) -> Result<(), String> + '_ {
        move |dest| {
            std::fs::copy(source, dest)
                .map(drop)
                .map_err(|e| e.to_string())
        }
    }

    #[test]
    fn should_hash_a_file_when_its_digest_is_known() {
        // given
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("abc");
        std::fs::write(&file, "abc").unwrap();

        // when
        let digest = sha256_of(&file).unwrap();

        // then
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn should_unpack_the_release_when_its_digest_matches() {
        // given
        let dir = tempfile::tempdir().unwrap();
        let (source, digest) = release(dir.path(), "#!/bin/sh\n");
        let dist = dir.path().join("quint/apalache-dist-1");
        std::fs::create_dir_all(dist.join("apalache/bin")).unwrap();
        std::fs::write(dist.join("apalache/bin/apalache-mc"), "planted").unwrap();
        std::fs::write(dist.join("apalache/extra"), "planted").unwrap();

        // when
        let binary = install(
            &digest,
            &dir.path().join("cache/a.tgz"),
            &dist,
            copy_from(&source),
        )
        .unwrap();

        // then
        assert_eq!(binary, dist.join("apalache/bin/apalache-mc"));
        assert_eq!(std::fs::read_to_string(&binary).unwrap(), "#!/bin/sh\n");
        assert!(
            !dist.join("apalache/extra").exists(),
            "the old tree was kept"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dist)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["apalache"]);
    }

    #[test]
    fn should_refuse_the_release_when_its_digest_differs() {
        // given
        let dir = tempfile::tempdir().unwrap();
        let (source, actual) = release(dir.path(), "tampered");
        let pinned = "0".repeat(64);
        let dist = dir.path().join("quint/apalache-dist-1");
        let tarball = dir.path().join("cache/a.tgz");

        // when
        let result = install(&pinned, &tarball, &dist, copy_from(&source));

        // then
        let err = result.unwrap_err();
        assert!(err.contains(&actual) && err.contains(&pinned), "{err}");
        assert!(!tarball.exists() && !tarball.with_extension("tgz.part").exists());
        assert!(
            !dist.join("apalache").exists(),
            "an unverified tree was unpacked"
        );
    }

    #[test]
    fn should_download_again_when_the_cached_tarball_differs() {
        // given
        let dir = tempfile::tempdir().unwrap();
        let (source, digest) = release(dir.path(), "good");
        let tarball = dir.path().join("cache/a.tgz");
        std::fs::create_dir_all(tarball.parent().unwrap()).unwrap();
        std::fs::write(&tarball, "corrupted").unwrap();
        let fetched = Cell::new(0);
        let fetch = |dest: &Path| {
            fetched.set(fetched.get() + 1);
            copy_from(&source)(dest)
        };

        // when
        let first = install(&digest, &tarball, &dir.path().join("d"), fetch);
        let second = install(&digest, &tarball, &dir.path().join("d"), fetch);

        // then
        assert!(first.is_ok() && second.is_ok(), "{first:?} {second:?}");
        assert_eq!(
            fetched.get(),
            1,
            "a verified tarball is reused, a bad one replaced"
        );
    }

    #[test]
    fn should_fail_when_the_download_fails() {
        // given
        let dir = tempfile::tempdir().unwrap();

        // when
        let result = install(
            &"0".repeat(64),
            &dir.path().join("a.tgz"),
            dir.path(),
            |_| Err("offline".to_owned()),
        );

        // then
        assert_eq!(result.unwrap_err(), "offline");
    }

    #[test]
    fn should_place_the_release_where_quint_looks_when_paths_are_built() {
        // given
        let version = "0.62.1";

        // when
        let url = release_url(version);
        let dist = dist_dir(version);

        // then
        assert_eq!(
            url,
            "https://github.com/apalache-mc/apalache/releases/download/v0.62.1/apalache.tgz"
        );
        if let Some(dist) = dist {
            assert!(dist.ends_with("apalache-dist-0.62.1"), "{}", dist.display());
        }
    }

    #[test]
    fn should_read_the_digest_when_a_hash_tool_prints_it() {
        // given
        let line = format!("{}  file\n", "AB".repeat(32));

        // when
        let digest = parse_digest(&line);

        // then
        assert_eq!(digest, Some("ab".repeat(32)));
        assert_eq!(parse_digest("nonsense"), None);
    }
}
