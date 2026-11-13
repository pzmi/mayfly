//! mayfly's image manifest through the public API (OPS-02, ADR 0091): golden valid and invalid
//! examples under `tests/fixtures/manifest/`, one invalid example per refusal, and the checks
//! `mayflyd` makes of each pool's manifest against its deployment.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use mayfly_composition::config::{Config, ConfigErrorKind, GuestOs};
use mayfly_composition::manifest::{Arch, Manifest, ManifestErrors, ManifestLoadError, resolve};
use mayfly_control_domain::attempt::GuestOs as DomainOs;
use mayfly_control_domain::image::{ImageManifest, RunnerVersion};
use mayfly_domain::PoolName;

fn fixtures(family: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/manifest")
        .join(family)
}

fn read(family: &str, name: &str) -> String {
    fs::read_to_string(fixtures(family).join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn names(family: &str, extension: &str) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(fixtures(family))
        .unwrap_or_else(|e| panic!("{e}"))
        .map(|entry| {
            entry
                .unwrap_or_else(|e| panic!("{e}"))
                .file_name()
                .into_string()
                .unwrap_or_else(|n| panic!("{n:?}"))
        })
        .filter(|name| name.ends_with(extension))
        .collect();
    found.sort();
    found
}

fn macos() -> Manifest {
    Manifest::parse(&read("valid", "macos.toml")).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn should_accept_the_input_when_a_fixture_is_valid() {
    // given
    let valid = names("valid", ".toml");

    // when
    let results: Vec<_> = valid
        .iter()
        .map(|name| (name, Manifest::parse(&read("valid", name))))
        .collect();

    // then
    assert!(valid.len() >= 2);
    for (name, result) in results {
        assert!(result.is_ok(), "{name}: {:?}", result.err());
    }
}

#[test]
fn should_read_every_field_when_the_macos_fixture_is_parsed() {
    // given
    let text = read("valid", "macos.toml");

    // when
    let manifest = Manifest::parse(&text).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert_eq!(manifest.os, GuestOs::Macos);
    assert_eq!(manifest.arch, Arch::Arm64);
    assert_eq!(manifest.source.repository, "ghcr.io/acme/runner-macos");
    assert_eq!(
        manifest.source.digest.as_str(),
        format!("sha256:{}", "a".repeat(64))
    );
    assert_eq!(manifest.runner.sha256, "c".repeat(64));
    assert_eq!(manifest.launcher_sha256, "d".repeat(64));
    assert_eq!(
        manifest.recorded(),
        ImageManifest {
            os: DomainOs::Macos,
            runner_version: RunnerVersion::new(2, 330, 0),
            revision: 1,
        }
    );
    assert_eq!(
        manifest.summary(),
        format!(
            "ghcr.io/acme/runner-macos@sha256:{} os=macos arch=arm64 runner=2.330.0 revision=1",
            "a".repeat(64)
        )
    );
}

#[test]
fn should_refuse_the_input_when_a_fixture_is_invalid() {
    // given
    let invalid = names("invalid", ".toml");

    // when
    let reports: Vec<_> = invalid
        .iter()
        .map(|name| {
            let report = Manifest::parse(&read("invalid", name))
                .err()
                .unwrap_or_else(|| panic!("{name} was accepted"))
                .to_string();
            (name, report)
        })
        .collect();

    // then
    assert!(invalid.len() >= 10);
    for (name, report) in reports {
        let golden = read("invalid", &name.replace(".toml", ".errors.txt"));
        assert_eq!(report, golden.trim_end_matches('\n'), "{name}");
    }
}

#[test]
fn should_pair_every_invalid_fixture_with_its_golden_report_when_the_directory_is_listed() {
    // given
    let inputs = names("invalid", ".toml");
    let goldens = names("invalid", ".errors.txt");

    // when
    let expected: Vec<String> = inputs
        .iter()
        .map(|name| name.replace(".toml", ".errors.txt"))
        .collect();

    // then
    assert_eq!(goldens, expected);
}

#[test]
fn should_refuse_a_tag_when_the_source_names_one_with_or_without_a_digest() {
    // given
    let cases = ["tag_only.toml", "tag_and_digest.toml", "no_digest.toml"];
    for fixture in cases {
        // when
        let ManifestErrors(errors) = Manifest::parse(&read("invalid", fixture))
            .err()
            .unwrap_or_else(|| panic!("{fixture} was accepted"));

        // then
        assert_eq!(errors.len(), 1, "{fixture}");
        assert_eq!(errors[0].field, "source", "{fixture}");
        assert_eq!(errors[0].kind, ConfigErrorKind::Invalid, "{fixture}");
    }
}

#[test]
fn should_refuse_the_image_when_it_has_no_guest_agent() {
    // given
    let text = read("invalid", "guest_agent_missing.toml");

    // when
    let ManifestErrors(errors) = Manifest::parse(&text)
        .err()
        .unwrap_or_else(|| panic!("accepted"));

    // then
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].field, "capabilities.guest_agent");
    assert!(errors[0].problem.contains("ADR 0038"), "{}", errors[0]);
}

/// A configuration of `pools` (name, guest OS, manifest path) with `[images]` naming
/// `repository` and `runners`.
fn config(repository: &str, runners: &str, pools: &[(&str, &str, &str)]) -> Config {
    let mut text = format!(
        "schema_version = 1\n[github]\norganisation = \"example-org\"\napp_client_id = \"Iv1.0\"\n\
         installation_id = 1\nprivate_key = \"env:KEY\"\n[images]\nrepository = \"{repository}\"\n\
         {runners}\n"
    );
    for (name, os, path) in pools {
        text.push_str(&format!(
            "[[pools]]\nname = \"{name}\"\ntier = \"trusted\"\nimage_manifest = \"{path}\"\n\
             guest_os = \"{os}\"\ncpu = 2\nmemory_mib = 4096\nmax_running = 1\n"
        ));
    }
    Config::parse(&text).unwrap_or_else(|e| panic!("{e}"))
}

/// A reader over `files`, by path: what `resolve` reads instead of the file system.
fn files(
    files: BTreeMap<&'static str, Manifest>,
) -> impl Fn(&Path) -> Result<Manifest, ManifestLoadError> {
    move |path: &Path| {
        path.to_str()
            .and_then(|p| files.get(p).cloned())
            .ok_or_else(|| ManifestLoadError::Read {
                path: path.to_path_buf(),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            })
    }
}

#[test]
fn should_add_each_manifest_runner_pin_when_the_configuration_pins_none() {
    // given
    let config = config(
        "ghcr.io/acme/runner-macos",
        "",
        &[
            ("mac", "macos", "/m/mac.toml"),
            ("mac2", "macos", "/m/mac.toml"),
        ],
    );
    let images = config.images.clone().unwrap_or_else(|| panic!("[images]"));

    // when
    let resolved = resolve(
        &config.pools,
        &images,
        files(BTreeMap::from([("/m/mac.toml", macos())])),
    )
    .unwrap_or_else(|e| panic!("{e:?}"));

    // then
    let pool = |name: &str| PoolName::new(name).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(resolved.manifests.get(&pool("mac")), Some(&macos()));
    assert_eq!(resolved.manifests.get(&pool("mac2")), Some(&macos()));
    assert_eq!(
        resolved.runners,
        BTreeMap::from([(
            "2.330.0".to_owned(),
            BTreeMap::from([("osx-arm64".to_owned(), "c".repeat(64))])
        )])
    );
}

#[test]
fn should_name_each_mismatch_when_a_manifest_disagrees_with_the_deployment() {
    // given: pool 0 reads another repository's image, pool 1 a macOS image as Linux with a
    // runner pin other than [images.runners], pool 2 that pin again, pool 3 a good manifest, and
    // pool 4's manifest is missing
    let config = config(
        "ghcr.io/acme/runner-macos",
        &format!("[images.runners]\n\"2.330.0\" = \"{}\"", "f".repeat(64)),
        &[
            ("other", "macos", "/m/other.toml"),
            ("lin", "linux", "/m/mac.toml"),
            ("pin", "macos", "/m/mac.toml"),
            ("good", "macos", "/m/good.toml"),
            ("gone", "macos", "/m/gone.toml"),
        ],
    );
    let images = config.images.clone().unwrap_or_else(|| panic!("[images]"));
    let mut other = macos();
    other.source.repository = "ghcr.io/acme/elsewhere".to_owned();
    other.runner.sha256 = "f".repeat(64);
    let mut good = other.clone();
    good.source.repository = "ghcr.io/acme/runner-macos".to_owned();
    let read = files(BTreeMap::from([
        ("/m/other.toml", other),
        ("/m/mac.toml", macos()),
        ("/m/good.toml", good),
    ]));

    // when
    let errors = resolve(&config.pools, &images, read)
        .err()
        .unwrap_or_else(|| panic!("accepted"));

    // then
    let found: Vec<(&str, bool)> = errors
        .iter()
        .map(|e| (e.field.as_str(), e.kind == ConfigErrorKind::Invalid))
        .collect();
    assert_eq!(
        found,
        [
            ("pools[0].image_manifest", true),
            ("pools[1].image_manifest", true),
            ("pools[1].image_manifest", true),
            ("pools[2].image_manifest", true),
            ("pools[4].image_manifest", true),
        ],
        "{errors:#?}"
    );
    assert!(
        errors[0].problem.contains("[images] repository"),
        "{}",
        errors[0]
    );
    assert!(errors[1].problem.contains("guest_os"), "{}", errors[1]);
    assert!(errors[2].problem.contains("osx-arm64"), "{}", errors[2]);
    assert!(errors[3].problem.contains("osx-arm64"), "{}", errors[3]);
    assert!(
        errors[4].problem.contains("cannot be read"),
        "{}",
        errors[4]
    );
}

#[test]
fn should_refuse_a_second_manifest_when_two_pools_describe_one_digest_otherwise() {
    // given
    let config = config(
        "ghcr.io/acme/runner-macos",
        "",
        &[
            ("mac", "macos", "/m/mac.toml"),
            ("twin", "macos", "/m/twin.toml"),
        ],
    );
    let images = config.images.clone().unwrap_or_else(|| panic!("[images]"));
    let mut twin = macos();
    twin.revision = 9;
    let read = files(BTreeMap::from([
        ("/m/mac.toml", macos()),
        ("/m/twin.toml", twin),
    ]));

    // when
    let errors = resolve(&config.pools, &images, read)
        .err()
        .unwrap_or_else(|| panic!("accepted"));

    // then
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].field, "pools[1].image_manifest");
    assert!(errors[0].problem.contains("pools[0]"), "{}", errors[0]);
}

#[test]
fn should_name_the_manifest_field_under_its_pool_when_a_manifest_is_invalid() {
    // given
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let path = dir.path().join("tag.toml");
    fs::write(&path, read("invalid", "tag_only.toml")).unwrap_or_else(|e| panic!("{e}"));
    let config = config(
        "ghcr.io/acme/runner-macos",
        "",
        &[("mac", "macos", &path.display().to_string())],
    );
    let images = config.images.clone().unwrap_or_else(|| panic!("[images]"));

    // when
    let errors = resolve(&config.pools, &images, Manifest::load)
        .err()
        .unwrap_or_else(|| panic!("accepted"));

    // then
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].field, "pools[0].image_manifest (source)");
    assert!(errors[0].problem.contains("tag-only"), "{}", errors[0]);
}
