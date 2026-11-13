//! mayfly's image manifest (plan §8, §11; OPS-02, ADR 0091): the TOML file a pool's
//! `image_manifest` names, saying what an image is and what it needs. Not the OCI manifest the
//! registry serves, which the data plane fetches by digest and verifies at the pull (ADR 0087
//! decision 7); this one is the operator's, and the control plane registers the image from it.
//!
//! ```toml
//! schema = 1
//! provider = "tart"
//! os = "macos"
//! arch = "arm64"
//! source = "ghcr.io/acme/runner-macos@sha256:<64 lowercase hex digits>"
//! revision = 1
//!
//! [runner]
//! version = "2.330.0"
//! sha256 = "<the runner package's SHA-256>"
//!
//! [launcher]
//! sha256 = "<the in-image launcher's SHA-256>"
//!
//! [capabilities]
//! guest_agent = true
//! ```
//!
//! Rules this module holds, each pinned by `tests/manifest.rs`:
//!
//! - **Digest-pinned only.** `source` is `<repository>@sha256:<64 hex>`: a tag-only reference
//!   is refused (a tag moves; mayfly never pulls by tag, ADR 0039, dm §9.3), and so is a tag
//!   beside the digest, which would read as if the tag mattered
//!   (`tag_only.toml`, `tag_and_digest.toml`).
//! - **The guest agent is required.** `capabilities.guest_agent` must be `true`: mayfly reaches
//!   a guest only through `tart exec` (ADR 0038), so an image without the agent could never be
//!   verified or given its runner (`guest_agent_missing.toml`).
//! - **Read like the configuration** (`crate::config`): unknown keys refused, every problem
//!   reported with its field and its fix, all at once.
//! - **Checked against the deployment by `mayflyd`** ([`resolve`]): the image's repository is
//!   `[images] repository`, its guest is the pool's `guest_os`, its runner pin agrees with every
//!   other pin of that runner package, and a digest has one manifest across the pools.
//!
//! The host requirement of schema 1 is `arch` (Tart runs arm64 guests on Apple silicon, the
//! product's host, ADR 0003); a later requirement is a new key, which an older build refuses.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use mayfly_control_domain::attempt::GuestOs as DomainOs;
use mayfly_control_domain::image::{ImageManifest, RunnerVersion};
use mayfly_domain::{ImageDigest, PoolName};
use toml::{Table, Value};

use crate::config::{
    ConfigError, ConfigErrorKind, GuestOs, ImagesConfig, PoolConfig, ProviderKind, Reader,
    describe, enum_text, image_repository_ok, runner_version_ok, sha256_ok, syntax_error,
};
use crate::exit_code;

/// The manifest schema this build reads.
pub const MANIFEST_SCHEMA: i64 = 1;

const TOP_KEYS: &[&str] = &[
    "schema",
    "provider",
    "os",
    "arch",
    "source",
    "revision",
    "runner",
    "launcher",
    "capabilities",
];

/// The guest and host architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    /// 64-bit Arm: Tart's guests on Apple silicon.
    Arm64,
}

/// Where the image comes from: a repository and the digest that pins it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSource {
    /// `host/path`, no scheme, tag or digest.
    pub repository: String,
    /// The image's digest: its identity.
    pub digest: ImageDigest,
}

impl fmt::Display for ImageSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.repository, self.digest)
    }
}

/// The runner the image's base is given (ADR 0030, ADR 0087 decision 10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerPin {
    /// Its version.
    pub version: RunnerVersion,
    /// The SHA-256 of its package for the image's guest, 64 lowercase hex digits.
    pub sha256: String,
}

/// One image manifest, valid. `capabilities.guest_agent` is not kept: every valid manifest has
/// it (the reader refuses one without).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The VM provider: `tart`, the only one.
    pub provider: ProviderKind,
    /// The guest OS.
    pub os: GuestOs,
    /// The architecture.
    pub arch: Arch,
    /// The image, pinned by digest.
    pub source: ImageSource,
    /// The image revision a VM reports (I14).
    pub revision: u64,
    /// The runner its base is given.
    pub runner: RunnerPin,
    /// The SHA-256 of the fixed in-image launcher (ADR 0082), 64 lowercase hex digits: recorded
    /// and format-checked; comparing it with the image's launcher is planned, no task yet.
    pub launcher_sha256: String,
}

impl Manifest {
    /// Reads and validates the file at `path`.
    ///
    /// # Errors
    /// [`ManifestLoadError`].
    pub fn load(path: &Path) -> Result<Self, ManifestLoadError> {
        let text = std::fs::read_to_string(path).map_err(|source| ManifestLoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text).map_err(ManifestLoadError::Invalid)
    }

    /// Validates manifest text. Pure: reads no file, no environment and no registry.
    ///
    /// # Errors
    /// Every problem found, never empty.
    pub fn parse(text: &str) -> Result<Self, ManifestErrors> {
        let table: Table = match toml::from_str(text) {
            Ok(table) => table,
            Err(error) => return Err(ManifestErrors(syntax_error(text, &error).0)),
        };
        if let Some(error) = schema_error(&table) {
            return Err(ManifestErrors(vec![error]));
        }
        let mut root = Reader::new(String::new(), table, TOP_KEYS).without_refusals();
        let _ = root.value("schema", "schema = 1", false);
        let provider = read_provider(&mut root);
        let os = read_os(&mut root);
        let arch = read_arch(&mut root);
        let source = read_source(&mut root);
        let revision = root.uint(
            "revision",
            &(0..=i64::MAX.cast_unsigned()),
            "revision = 1",
            true,
        );
        let runner = root.read_section_with("runner", true, &["version", "sha256"], read_runner);
        let launcher_sha256 = root.read_section_with("launcher", true, &["sha256"], |reader| {
            read_sha256(
                reader,
                "sha256",
                "the SHA-256 of the image's launcher (ADR 0082)",
            )
        });
        let agent =
            root.read_section_with("capabilities", true, &["guest_agent"], read_capabilities);
        let errors = root.finish();
        match (
            provider,
            os,
            arch,
            source,
            revision,
            runner,
            launcher_sha256,
            agent,
        ) {
            (
                Some(provider),
                Some(os),
                Some(arch),
                Some(source),
                Some(revision),
                Some(runner),
                Some(launcher_sha256),
                Some(()),
            ) if errors.is_empty() => Ok(Self {
                provider,
                os,
                arch,
                source,
                revision,
                runner,
                launcher_sha256,
            }),
            _ => Err(ManifestErrors(errors)),
        }
    }

    /// What the control plane records of it (`ImageRegistered`'s manifest): the guest, the
    /// runner version and the revision. The rest stays in the file: the runner's checksum joins
    /// the data plane's pin table, which checks the package at the materialisation (ADR 0087
    /// decision 10); the architecture is checked here; the launcher's checksum is recorded and
    /// format-checked only, and comparing it with the image's launcher is planned, no task yet
    /// (ADR 0091 decision 9).
    #[must_use]
    pub const fn recorded(&self) -> ImageManifest {
        ImageManifest {
            os: match self.os {
                GuestOs::Macos => DomainOs::Macos,
                GuestOs::Linux => DomainOs::Linux,
            },
            runner_version: self.runner.version,
            revision: self.revision,
        }
    }

    /// One line for an operator: the source, guest, architecture, runner and revision.
    #[must_use]
    pub fn summary(&self) -> String {
        let v = self.runner.version;
        format!(
            "{} os={} arch=arm64 runner={}.{}.{} revision={}",
            self.source,
            self.os.as_str(),
            v.major,
            v.minor,
            v.patch,
            self.revision
        )
    }
}

fn schema_error(table: &Table) -> Option<ConfigError> {
    let fix = format!("start the file with `schema = {MANIFEST_SCHEMA}`");
    let problem = match table.get("schema") {
        None => "missing".to_owned(),
        Some(Value::Integer(found)) if *found == MANIFEST_SCHEMA => return None,
        Some(Value::Integer(found)) => {
            format!("schema {found} is not readable by this build, which reads {MANIFEST_SCHEMA}")
        }
        Some(other) => format!("expected an integer, found {}", describe(other)),
    };
    Some(ConfigError {
        field: "schema".to_owned(),
        kind: ConfigErrorKind::SchemaVersion,
        problem,
        fix,
    })
}

fn read_provider(reader: &mut Reader) -> Option<ProviderKind> {
    let text = reader.string("provider", "provider = \"tart\"", true)?;
    if text == "tart" {
        return Some(ProviderKind::Tart);
    }
    reader.push(
        "provider",
        ConfigErrorKind::Invalid,
        format!("{} is not a provider", enum_text(&text)),
        "use `provider = \"tart\"`, the only provider",
    );
    None
}

fn read_os(reader: &mut Reader) -> Option<GuestOs> {
    let text = reader.string("os", "os = \"macos\"", true)?;
    match text.as_str() {
        "macos" => Some(GuestOs::Macos),
        "linux" => Some(GuestOs::Linux),
        _ => {
            reader.push(
                "os",
                ConfigErrorKind::Invalid,
                format!("{} is not a guest OS", enum_text(&text)),
                "use `os = \"macos\"` or `os = \"linux\"`",
            );
            None
        }
    }
}

fn read_arch(reader: &mut Reader) -> Option<Arch> {
    let text = reader.string("arch", "arch = \"arm64\"", true)?;
    if text == "arm64" {
        return Some(Arch::Arm64);
    }
    reader.push(
        "arch",
        ConfigErrorKind::Invalid,
        format!(
            "{} is not supported: Tart runs arm64 guests on Apple silicon",
            enum_text(&text)
        ),
        "use `arch = \"arm64\"`",
    );
    None
}

/// Why a `source` is refused, and how to fix it.
fn source_refusal(text: &str) -> Option<(&'static str, &'static str)> {
    const PIN: &str = "write `<repository>@sha256:<64 lowercase hex digits>`, the digest the registry gives the image";
    let (name, digest) = match text.split_once('@') {
        Some(parts) => parts,
        None => {
            let last = text.rsplit('/').next().unwrap_or(text);
            return Some(if last.contains(':') {
                (
                    "is a tag-only reference: a tag can move, and mayfly pulls only by digest (ADR 0039)",
                    PIN,
                )
            } else {
                (
                    "names no digest: mayfly pulls only by digest (ADR 0039)",
                    PIN,
                )
            });
        }
    };
    let last = name.rsplit('/').next().unwrap_or(name);
    if last.contains(':') {
        return Some((
            "names a tag beside its digest: the digest alone pins the image",
            "remove the tag and keep `@sha256:<digest>`",
        ));
    }
    if !image_repository_ok(name) {
        return Some((
            "does not name a bare OCI repository before `@`",
            "write `host/path` in lowercase, like `ghcr.io/acme/runner-macos@sha256:<digest>`",
        ));
    }
    if ImageDigest::new(digest).is_err() {
        return Some((
            "does not end in a digest of `sha256:` and 64 lowercase hex digits",
            PIN,
        ));
    }
    None
}

fn read_source(reader: &mut Reader) -> Option<ImageSource> {
    let example = "source = \"ghcr.io/acme/runner-macos@sha256:<digest>\"";
    let text = reader.string("source", example, true)?;
    if let Some((problem, fix)) = source_refusal(&text) {
        reader.invalid("source", problem, fix);
        return None;
    }
    let (repository, digest) = text.split_once('@')?;
    Some(ImageSource {
        repository: repository.to_owned(),
        digest: ImageDigest::new(digest).ok()?,
    })
}

fn read_sha256(reader: &mut Reader, key: &str, what: &str) -> Option<String> {
    let text = reader.string(key, "sha256 = \"<64 lowercase hex digits>\"", true)?;
    if sha256_ok(&text) {
        return Some(text);
    }
    reader.invalid(
        key,
        "is not a SHA-256 of 64 lowercase hex digits",
        &format!("write {what}"),
    );
    None
}

fn read_runner(reader: &mut Reader) -> Option<RunnerPin> {
    let version = reader
        .string("version", "version = \"2.330.0\"", true)
        .and_then(|text| {
            let parsed = runner_version(&text);
            if parsed.is_none() {
                reader.invalid(
                    "version",
                    "is not a runner version",
                    "write `X.Y.Z`, like `version = \"2.330.0\"`",
                );
            }
            parsed
        });
    let sha256 = read_sha256(
        reader,
        "sha256",
        "the SHA-256 of the runner package for the image's guest",
    );
    Some(RunnerPin {
        version: version?,
        sha256: sha256?,
    })
}

/// `X.Y.Z` as three numbers, each fitting `u32`.
fn runner_version(text: &str) -> Option<RunnerVersion> {
    if !runner_version_ok(text) {
        return None;
    }
    let mut parts = text.split('.').map(str::parse::<u32>);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch))) => {
            Some(RunnerVersion::new(major, minor, patch))
        }
        _ => None,
    }
}

fn read_capabilities(reader: &mut Reader) -> Option<()> {
    let example = "guest_agent = true";
    match reader.value("guest_agent", example, true)? {
        Value::Boolean(true) => Some(()),
        Value::Boolean(false) => {
            reader.invalid(
                "guest_agent",
                "is false: mayfly reaches a guest only through `tart exec` (ADR 0038), so an image without the guest agent is refused",
                "bake the Tart guest agent into the image and set `guest_agent = true`",
            );
            None
        }
        other => {
            reader.wrong_type("guest_agent", "a boolean", &other, example);
            None
        }
    }
}

/// Every problem of one manifest, in the order the schema is read. Never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestErrors(pub Vec<ConfigError>);

impl fmt::Display for ManifestErrors {
    /// A count line, then one line per problem. Pinned by
    /// `tests/manifest.rs::should_refuse_the_input_when_a_fixture_is_invalid`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.0.len();
        write!(
            f,
            "invalid image manifest: {count} {}",
            if count == 1 { "error" } else { "errors" }
        )?;
        for error in &self.0 {
            write!(f, "\n  {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ManifestErrors {}

/// Why a manifest file did not load.
#[derive(Debug)]
pub enum ManifestLoadError {
    /// The file could not be read.
    Read {
        /// The path asked for.
        path: PathBuf,
        /// The operating system's reason.
        source: std::io::Error,
    },
    /// The file was read and is not a valid manifest.
    Invalid(ManifestErrors),
}

impl ManifestLoadError {
    /// The exit code `mayfly image verify` ends with: an invalid configuration's (ADR 0046).
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        exit_code::CONFIG_INVALID
    }
}

impl fmt::Display for ManifestLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "cannot read image manifest {}: {source}", path.display())
            }
            Self::Invalid(errors) => errors.fmt(f),
        }
    }
}

impl std::error::Error for ManifestLoadError {}

/// Runner pins: version, then package platform, to SHA-256 (`[images.runners]`'s shape).
pub type RunnerPins = BTreeMap<String, BTreeMap<String, String>>;

/// What `mayflyd` takes from its pools' manifests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Each pool's manifest.
    pub manifests: BTreeMap<PoolName, Manifest>,
    /// `[images.runners]` with every manifest's runner pin added for its guest's platform: the
    /// data plane's pin table (ADR 0091).
    pub runners: RunnerPins,
}

fn mismatch(index: usize, problem: String, fix: String) -> ConfigError {
    ConfigError {
        field: format!("pools[{index}].image_manifest"),
        kind: ConfigErrorKind::Invalid,
        problem,
        fix,
    }
}

/// Reads each pool's manifest through `read` and checks it against the deployment (ADR 0091):
/// the image comes from `[images] repository` (the one repository the data plane pulls from),
/// its guest is the pool's `guest_os`, its runner pin agrees with `[images.runners]` and every
/// other manifest's pin of the same package, and one digest has one manifest across the pools.
/// Every problem is named by its pool (`pools[1].image_manifest`), all at once. Pinned by
/// `tests/manifest.rs::should_name_each_mismatch_when_a_manifest_disagrees_with_the_deployment`
/// and `should_add_each_manifest_runner_pin_when_the_configuration_pins_none`.
///
/// # Errors
/// Every problem found, never empty.
pub fn resolve(
    pools: &[PoolConfig],
    images: &ImagesConfig,
    read: impl Fn(&Path) -> Result<Manifest, ManifestLoadError>,
) -> Result<Resolved, Vec<ConfigError>> {
    let mut errors = Vec::new();
    let mut manifests = BTreeMap::new();
    let mut runners = images.runners.clone();
    let mut by_digest: BTreeMap<ImageDigest, (usize, Manifest)> = BTreeMap::new();
    for (index, pool) in pools.iter().enumerate() {
        let manifest = match read(&pool.image_manifest) {
            Ok(manifest) => manifest,
            Err(ManifestLoadError::Read { source, .. }) => {
                errors.push(mismatch(
                    index,
                    format!("cannot be read: {source}"),
                    "write the image's manifest there; `mayfly image verify <path>` checks one"
                        .to_owned(),
                ));
                continue;
            }
            Err(ManifestLoadError::Invalid(found)) => {
                errors.extend(found.0.into_iter().map(|e| ConfigError {
                    field: format!("pools[{index}].image_manifest ({})", e.field),
                    ..e
                }));
                continue;
            }
        };
        let before = errors.len();
        if manifest.source.repository != images.repository {
            errors.push(mismatch(
                index,
                format!(
                    "the image comes from {:?}, but the data plane pulls from `[images] repository` {:?}",
                    manifest.source.repository, images.repository
                ),
                "push the image to the configured repository and pin its digest there, or change `[images] repository`".to_owned(),
            ));
        }
        if manifest.os != pool.guest_os {
            errors.push(mismatch(
                index,
                format!(
                    "the image's guest is {}, the pool's `guest_os` is {}",
                    manifest.os.as_str(),
                    pool.guest_os.as_str()
                ),
                "give the pool an image of its guest OS, or change its `guest_os`".to_owned(),
            ));
        }
        let v = manifest.runner.version;
        let version = format!("{}.{}.{}", v.major, v.minor, v.patch);
        let platform = manifest.os.runner_platform();
        let pins = runners.entry(version.clone()).or_default();
        match pins.get(platform) {
            Some(pinned) if *pinned != manifest.runner.sha256 => errors.push(mismatch(
                index,
                format!(
                    "pins runner {version}'s {platform} package to another SHA-256 than `[images.runners]` or another pool's manifest"
                ),
                "make every pin of that package agree: it is one file".to_owned(),
            )),
            Some(_) => {}
            None => {
                pins.insert(platform.to_owned(), manifest.runner.sha256.clone());
            }
        }
        if let Some((first, earlier)) = by_digest.get(&manifest.source.digest)
            && *earlier != manifest
        {
            errors.push(mismatch(
                index,
                format!(
                    "describes the image of pools[{first}] otherwise: an image's manifest is fixed by its digest"
                ),
                "give both pools the same manifest for that digest".to_owned(),
            ));
        }
        if errors.len() == before {
            by_digest
                .entry(manifest.source.digest.clone())
                .or_insert((index, manifest.clone()));
            manifests.insert(pool.name.clone(), manifest);
        }
    }
    if errors.is_empty() {
        Ok(Resolved { manifests, runners })
    } else {
        Err(errors)
    }
}
