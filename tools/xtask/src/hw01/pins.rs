//! `tools/hw01/images.toml`: the images `hw01` pulls, each by digest, and the runner release the
//! test launcher downloads. A tag anywhere is refused before anything runs; a value the owner has
//! not filled in (`owner-fills-in`) leaves the checks that need it `not_run`.

use std::path::Path;

/// The marker of a pin nobody has verified yet.
pub const UNPINNED: &str = "owner-fills-in";

/// One image pinned by digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// A bare `host/path`.
    pub repository: String,
    /// `sha256:<64 lowercase hex>`.
    pub digest: String,
    /// What one pull downloads.
    pub size_bytes: u64,
}

impl Image {
    /// `<repository>@<digest>`: the only form `hw01` pulls.
    pub fn reference(&self) -> String {
        format!("{}@{}", self.repository, self.digest)
    }
}

/// The runner package pins, `None` while the owner has not filled one in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Runner {
    /// `X.Y.Z`.
    pub version: Option<String>,
    /// The linux-arm64 package's SHA-256.
    pub sha256_linux: Option<String>,
    /// The osx-arm64 package's SHA-256.
    pub sha256_osx: Option<String>,
}

/// Everything `images.toml` pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pins {
    /// Always pulled.
    pub linux: Image,
    /// Pulled only with `--macos`.
    pub macos: Image,
    /// The runner release.
    pub runner: Runner,
}

impl Runner {
    /// The version and the checksum for `os` (`linux` or `osx`), or why they are missing.
    pub fn for_os(&self, os: &str) -> Result<(&str, &str), String> {
        let sha = if os == "osx" {
            self.sha256_osx.as_deref()
        } else {
            self.sha256_linux.as_deref()
        };
        match (self.version.as_deref(), sha) {
            (Some(version), Some(sha)) => Ok((version, sha)),
            _ => Err(format!(
                "the runner pin ({os}) in tools/hw01/images.toml is still `{UNPINNED}`"
            )),
        }
    }
}

fn is_hex64(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_version(text: &str) -> bool {
    let parts: Vec<_> = text.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// A bare `host/path` with no tag, digest or scheme. Pinned by
/// `should_refuse_a_tag_when_an_image_is_pinned_by_one`.
fn check_repository(name: &str, repository: &str) -> Result<(), String> {
    let (host, path) = repository
        .split_once('/')
        .ok_or_else(|| format!("images.{name}.repository: not host/path: {repository}"))?;
    let path_ok = !path.is_empty()
        && path.split('/').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
        });
    let host_ok = !host.is_empty()
        && host
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b':'));
    if repository.contains('@') || !path_ok || !host_ok || repository.contains("://") {
        return Err(format!(
            "images.{name}.repository must be a bare host/path, never a tag or digest: {repository}"
        ));
    }
    Ok(())
}

fn image(table: &toml::Table, name: &str) -> Result<Image, String> {
    let image = table
        .get("images")
        .and_then(|i| i.get(name))
        .and_then(toml::Value::as_table)
        .ok_or_else(|| format!("images.{name} is missing"))?;
    let text = |key: &str| {
        image
            .get(key)
            .and_then(toml::Value::as_str)
            .ok_or_else(|| format!("images.{name}.{key} is missing"))
    };
    let repository = text("repository")?.to_owned();
    check_repository(name, &repository)?;
    let digest = text("digest")?.to_owned();
    if !digest.strip_prefix("sha256:").is_some_and(is_hex64) {
        return Err(format!(
            "images.{name}.digest must be sha256:<64 lowercase hex>, not a tag: {digest}"
        ));
    }
    text("reason")?;
    let size_bytes = image
        .get("size_bytes")
        .and_then(toml::Value::as_integer)
        .and_then(|n| u64::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("images.{name}.size_bytes must be a positive integer"))?;
    Ok(Image {
        repository,
        digest,
        size_bytes,
    })
}

fn pin(pins: &toml::Table, version: &str, key: &str) -> Result<Option<String>, String> {
    match pins.get(key).and_then(toml::Value::as_str) {
        Some(UNPINNED) => Ok(None),
        Some(value) if is_hex64(value) => Ok(Some(value.to_owned())),
        Some(value) => Err(format!("runners.\"{version}\".{key} is not valid: {value}")),
        None => Err(format!("runners.\"{version}\".{key} is missing")),
    }
}

/// `[runners."X.Y.Z"]` with one checksum per platform (`linux-arm64`, `osx-arm64`), the shape of
/// the image store's pins (`mayfly-adapter-tart`'s `ImageConfig::runners`); exactly one version,
/// which is `owner-fills-in` until the owner pins one.
fn runner(table: &toml::Table) -> Result<Runner, String> {
    let runners = table
        .get("runners")
        .and_then(toml::Value::as_table)
        .ok_or("runners is missing")?;
    let mut versions = runners.iter();
    let (Some((version, pins)), None) = (versions.next(), versions.next()) else {
        return Err("runners must pin exactly one version".to_owned());
    };
    let pins = pins
        .as_table()
        .ok_or_else(|| format!("runners.\"{version}\" is not a table"))?;
    if let Some(extra) = pins
        .keys()
        .find(|k| *k != "linux-arm64" && *k != "osx-arm64")
    {
        return Err(format!(
            "runners.\"{version}\".{extra}: only linux-arm64 and osx-arm64"
        ));
    }
    let version = match version.as_str() {
        UNPINNED => None,
        v if is_version(v) => Some(v.to_owned()),
        v => return Err(format!("runners: `{v}` is not X.Y.Z")),
    };
    let name = version.as_deref().unwrap_or(UNPINNED);
    Ok(Runner {
        sha256_linux: pin(pins, name, "linux-arm64")?,
        sha256_osx: pin(pins, name, "osx-arm64")?,
        version,
    })
}

/// Parses `images.toml`'s text.
///
/// # Errors
/// A tag, a malformed digest or repository, a missing key or an unknown schema.
pub fn parse(text: &str) -> Result<Pins, String> {
    let table: toml::Table = text.parse().map_err(|e| format!("images.toml: {e}"))?;
    if table.get("schema").and_then(toml::Value::as_integer) != Some(1) {
        return Err("images.toml: schema must be 1".to_owned());
    }
    if let Some(images) = table.get("images").and_then(toml::Value::as_table)
        && let Some(extra) = images.keys().find(|k| *k != "linux" && *k != "macos")
    {
        return Err(format!("images.{extra}: only linux and macos are pinned"));
    }
    Ok(Pins {
        linux: image(&table, "linux")?,
        macos: image(&table, "macos")?,
        runner: runner(&table)?,
    })
}

/// Reads and parses `path`.
///
/// # Errors
/// As [`parse`], or the file cannot be read.
pub fn load(path: &Path) -> Result<Pins, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:97166c84ea5f102be33d2959f70e83861a36bd749f1715ab669d93e6c9f31476";

    fn file(linux_digest: &str, macos_repository: &str) -> String {
        format!(
            "schema = 1\n[images.linux]\nrepository = \"ghcr.io/example/linux\"\ndigest = \
             \"{linux_digest}\"\nsize_bytes = 10\nreason = \"r\"\n[images.macos]\nrepository = \
             \"{macos_repository}\"\ndigest = \"{DIGEST}\"\nsize_bytes = 10\nreason = \"r\"\n\
             [runners.owner-fills-in]\nlinux-arm64 = \"owner-fills-in\"\nosx-arm64 = \
             \"owner-fills-in\"\n"
        )
    }

    #[test]
    fn should_hold_digests_only_when_the_committed_file_is_read() {
        // given
        let path = crate::repo::root().join("tools/hw01/images.toml");

        // when
        let pins = load(&path).expect("the committed pins parse");

        // then
        for image in [&pins.linux, &pins.macos] {
            assert!(image.digest.starts_with("sha256:"), "{image:?}");
            assert!(!image.repository.contains(':'), "{image:?}");
            assert!(image.reference().contains('@'), "{image:?}");
        }
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            !text.contains(":latest\"") && !text.contains(":24.04\""),
            "no tag in any value"
        );
    }

    #[test]
    fn should_refuse_a_tag_when_an_image_is_pinned_by_one() {
        // given
        let by_tag_digest = file("latest", "ghcr.io/example/macos");
        let tagged_repository = file(DIGEST, "ghcr.io/example/macos:latest");
        let digest_in_repository = file(DIGEST, &format!("ghcr.io/example/macos@{DIGEST}"));

        // when
        let parsed = [by_tag_digest, tagged_repository, digest_in_repository].map(|t| parse(&t));

        // then
        for result in parsed {
            assert!(result.is_err(), "{result:?}");
        }
        assert!(parse(&file(DIGEST, "ghcr.io/example/macos")).is_ok());
    }

    #[test]
    fn should_pin_each_platform_of_one_version_when_the_runner_is_filled_in() {
        // given
        let sha = "ab".repeat(32);
        let base = file(DIGEST, "ghcr.io/example/macos");
        let pinned = base.replace(
            "[runners.owner-fills-in]\nlinux-arm64 = \"owner-fills-in\"",
            &format!("[runners.\"2.330.0\"]\nlinux-arm64 = \"{sha}\""),
        );
        let two = format!("{base}[runners.\"2.331.0\"]\nlinux-arm64 = \"{sha}\"\n");

        // when
        let parsed = parse(&pinned).expect("parse");

        // then
        assert_eq!(parsed.runner.for_os("linux"), Ok(("2.330.0", sha.as_str())));
        assert!(
            parsed.runner.for_os("osx").is_err(),
            "osx-arm64 still unpinned"
        );
        assert!(parse(&two).is_err(), "one version only");
    }

    #[test]
    fn should_report_the_missing_runner_pin_when_the_owner_has_not_filled_it_in() {
        // given
        let pins = parse(&file(DIGEST, "ghcr.io/example/macos")).expect("parse");

        // when
        let linux = pins.runner.for_os("linux");

        // then
        assert!(linux.unwrap_err().contains(UNPINNED));
    }
}
