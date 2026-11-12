//! The daemon's configuration: TOML, `schema_version = 1` (plan §11).
//!
//! The file is read as a raw TOML table and walked key by key, not deserialised through
//! `serde`: a derive reports the first problem without its path and cannot say how to fix it,
//! while this reader names every problem's field (`pools[1].tier`), says what to change and
//! reports all of them at once. Rejected alternative: `deny_unknown_fields` plus a path
//! tracker crate, which still reports one error and echoes the offending source line.
//!
//! Rules this module holds:
//!
//! - **Unknown keys are refused, never ignored**, at every level; the message lists the valid
//!   keys. Pinned by `tests/config.rs::should_refuse_the_input_when_a_fixture_is_invalid`
//!   (`unknown_key.toml`).
//! - **Refused keys get their reason.** `allow_untrusted_prs` (ADR 0027) and a classic `mode`
//!   (ADR 0028) are refused with a message naming what replaced them. Pinned by the same test
//!   (`refused_allow_untrusted_prs.toml`, `refused_classic_mode.toml`).
//! - **Secrets are references, never values** (ADR 0033): a secret field holds a
//!   [`SecretRef`], and neither a refused reference nor a syntax error ever repeats the text it
//!   refused. Pinned by `tests/config.rs::should_not_echo_a_pasted_secret_when_the_reference_is_refused`.
//! - **Keys are never renamed** (compatibility rule); a breaking shape raises
//!   [`SCHEMA_VERSION`]. Pinned by `tests/config.rs::should_accept_every_key_when_the_full_fixture_is_parsed`.
//!
//! The config file itself holds no secret, so it carries no permission requirement.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::RangeInclusive;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use mayfly_control_domain::capacity;
use mayfly_control_domain::reconcile::PolicyOf;
use mayfly_domain::{PoolName, Resources, SecretRef};
use toml::{Table, Value};

use crate::exit_code;

/// The schema this build reads.
pub const SCHEMA_VERSION: i64 = 1;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// The whole configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The state directory: journal, host id, base registry, shims (plan §13). `None` when the
    /// file names none: `mayflyd` takes the platform's standard place (`crate::paths`, ADR 0091
    /// decision 5).
    pub state_dir: Option<PathBuf>,
    /// The GitHub side.
    pub github: GithubConfig,
    /// The host's operator reserves.
    pub host: HostConfig,
    /// The VM provider.
    pub provider: ProviderConfig,
    /// Where images come from (`[images]`, ADR 0091): optional in the schema, so a file without
    /// it still validates, and required by `mayflyd`, which cannot build its image store
    /// without a repository.
    pub images: Option<ImagesConfig>,
    /// The pools, in file order; one pool is one scale set (ADR 0040). Never empty.
    pub pools: Vec<PoolConfig>,
}

/// `[images]`: the OCI repository the data plane pulls images from by digest (ADR 0039,
/// ADR 0087) and the runner packages it may install (ADR 0030). One repository per deployment,
/// as the image store is built (`mayfly_adapter_tart::ImageConfig`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagesConfig {
    /// `host/path`, no scheme, tag or digest (`ghcr.io/acme/runner-macos`).
    pub repository: String,
    /// Pinned runner packages: version `X.Y.Z`, then the platform of each package
    /// ([`RUNNER_PLATFORMS`]), to its SHA-256, 64 lowercase hex digits; a version or platform not
    /// listed is never installed. `"X.Y.Z" = "<sha>"` (the first form) pins that checksum for
    /// every platform, as it always meant; `"X.Y.Z" = { linux-arm64 = "<sha>", osx-arm64 =
    /// "<sha>" }` pins each package (HW-01X: the runner ships one per platform, and the base's
    /// guest OS selects the pin). Pinned by `tests/config.rs::should_read_runner_pins_per_platform_when_a_version_names_them`.
    pub runners: BTreeMap<String, BTreeMap<String, String>>,
    /// Hosts besides the registry's own a token realm may be on (lowercase host names).
    pub realm_hosts: Vec<String>,
}

/// `[github]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubConfig {
    /// Organisation scope (ADR 0040), or repository scope for testing.
    pub scope: GithubScope,
    /// The GitHub App's client id.
    pub app_client_id: String,
    /// The App's installation id.
    pub installation_id: u64,
    /// Where the App's private key comes from (resolved by the secret provider, ADR 0033).
    pub private_key: SecretRef,
}

/// Where the scale sets live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubScope {
    /// Organisation scope: the supported scope in v0.x.
    Organisation {
        /// The organisation's login.
        organisation: String,
    },
    /// Repository scope: allowed for testing.
    Repository {
        /// The owning organisation or user.
        organisation: String,
        /// The repository name.
        repository: String,
    },
}

/// `[host]`: what the operator keeps for the host itself (ADR 0024), and what the host offers
/// and its VMs take on disk. The sizing keys have no default (ADR 0052: the data plane's ledger
/// is sized by the operator, never by a guess): optional in the schema, so a file without them
/// still validates, and required by `mayflyd` (`crate::daemon::load`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostConfig {
    /// Memory left unallocated to VMs, in MiB.
    pub reserve_memory_mib: u64,
    /// Disk left unallocated to VMs, in GiB.
    pub reserve_disk_gib: u64,
    /// The VMs this host runs at once, of any guest OS.
    pub vm_slots: Option<u32>,
    /// The macOS guests the hypervisor runs at once, foreign ones included: at most 2, Apple's
    /// limit.
    pub macos_guests: Option<u32>,
    /// The disk ceiling of one pulled image, and of one base made from it, in GiB.
    pub image_disk_gib: Option<u64>,
    /// The disk ceiling of one clone, in GiB.
    pub clone_disk_gib: Option<u64>,
}

impl HostConfig {
    /// The reserves as a resource amount in bytes.
    #[must_use]
    pub const fn reserve(&self) -> Resources {
        Resources::new(self.reserve_memory_mib * MIB, self.reserve_disk_gib * GIB)
    }
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            reserve_memory_mib: 4096,
            reserve_disk_gib: 50,
            vm_slots: None,
            macos_guests: None,
            image_disk_gib: None,
            clone_disk_gib: None,
        }
    }
}

/// `[provider]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    /// The provider kind; `tart` is the only one.
    pub kind: ProviderKind,
    /// The Tart home directory (`TART_HOME`), dedicated to mayfly (plan §13, ADR 0087). `None`
    /// when the file names none: `mayflyd` takes the platform's standard place (`crate::paths`).
    pub tart_home: Option<PathBuf>,
}

impl Default for ProviderConfig {
    /// Tart, in the platform's standard home.
    fn default() -> Self {
        Self {
            kind: ProviderKind::Tart,
            tart_home: None,
        }
    }
}

/// The VM providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// Tart, driven by argv (ADR 0013).
    Tart,
}

/// `[[pools]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    /// The pool's name; also its scale set's name.
    pub name: PoolName,
    /// The trust tier (ADR 0027).
    pub tier: Tier,
    /// The image manifest, an absolute path (read by `mayfly image verify`, OPS-02).
    pub image_manifest: PathBuf,
    /// The guest OS.
    pub guest_os: GuestOs,
    /// CPUs per VM.
    pub cpu: u32,
    /// Memory per VM, in MiB.
    pub memory_mib: u64,
    /// Prepared, unregistered VMs held ahead of demand (ADR 0028); 0 by default.
    pub min_prepared: u32,
    /// A Prepared VM older than this is retired (ADR 0028).
    pub max_prepared_age: Duration,
    /// The most VMs the pool runs at once.
    pub max_running: u32,
    /// The pool's circuit breaker (dm I27).
    pub breaker: BreakerConfig,
    /// A daily runner-only refresh, host local time (ADR 0039); none by default.
    pub runner_refresh_at: Option<TimeOfDay>,
    /// What the pool keeps ahead of the pools after it (ADR 0069); `exact` by default.
    pub capacity_policy: CapacityPolicy,
}

/// `capacity_policy`: what a pool keeps of the host's free capacity ahead of the pools after it
/// in name order (ADR 0069, DEC-06).
///
/// Optional, `"exact"` when absent. A build older than this key refuses a file that has it
/// (unknown keys are refused, never ignored), so a rollback fails loudly instead of running
/// another policy than the operator chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CapacityPolicy {
    /// `"exact"`: keep the pool's whole `max_running` headroom, whether jobs wait or not. Safe
    /// whatever GitHub does with the `maxCapacity` mayfly sends.
    #[default]
    Exact,
    /// `"demand"`: **experimental**, an explicit opt-in (ADR 0069, amended 2026-11-11). Keep room
    /// only for the VMs the pool waits for, so later pools run beside an idle one. GitHub
    /// assigns nothing to a scale set advertising `maxCapacity` 0 (`doc/github-scaleset.md` §5),
    /// so a pool carved nothing never learns of its job and can starve; HW-02 measures what lies
    /// beyond 0 (§12 item 3).
    Demand,
}

/// The capacity policy a reconciler pass hands `capacity::plan` for a pool configured with
/// `policy` (ADR 0069, DEC-06F): `exact` is `Exact` whatever waits; `demand` is
/// `Demand { waiting }`, where `waiting` is what the pass derives for the pool (CP-03): its
/// scale set's latest `totalAssignedJobs` beyond its live runners, plus its `min_prepared`
/// deficit, never capped by the pool's carve-out (a capped count would feed back: carved
/// nothing, told nothing). Without this mapping the key would be parsed and ignored:
/// [`reconciler_policies`] hands it to the reconciler (CP-03), which RT-02 wires. Pinned by
/// `tests/config.rs::should_map_the_capacity_policy_when_a_pool_is_planned`.
#[must_use]
pub const fn plan_policy(policy: CapacityPolicy, waiting: u32) -> capacity::CapacityPolicy {
    match policy {
        CapacityPolicy::Exact => capacity::CapacityPolicy::Exact,
        CapacityPolicy::Demand => capacity::CapacityPolicy::Demand { waiting },
    }
}

/// The reconciler's policy of a pool configured with `policy` (CP-03): [`plan_policy`] as a
/// plain function of the VMs the pool waits for, which each pass derives and hands it.
#[must_use]
pub fn policy_of(policy: CapacityPolicy) -> PolicyOf {
    match policy {
        CapacityPolicy::Exact => |waiting| plan_policy(CapacityPolicy::Exact, waiting),
        CapacityPolicy::Demand => |waiting| plan_policy(CapacityPolicy::Demand, waiting),
    }
}

/// Every configured pool's reconciler policy, by pool: what composition hands the reconciler
/// (`mayfly_control::reconciler::ReconcilerConfig::policies`). Pinned by
/// `tests/reconciler.rs::should_carve_a_demand_pool_only_for_its_assigned_jobs_when_configured`.
#[must_use]
pub fn reconciler_policies(config: &Config) -> BTreeMap<PoolName, PolicyOf> {
    config
        .pools
        .iter()
        .map(|pool| (pool.name.clone(), policy_of(pool.capacity_policy)))
        .collect()
}

/// The trust tier a pool serves (ADR 0027).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Trusted code only: the only tier in v0.x.
    Trusted,
}

/// The guest operating system of a pool's image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestOs {
    /// A macOS guest.
    Macos,
    /// A Linux guest.
    Linux,
}

/// A pool's breaker: `failures` stage failures within `window` trip it (dm I27).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerConfig {
    /// N: failures that trip the breaker.
    pub failures: u32,
    /// W: the window the failures are counted in.
    pub window: Duration,
}

/// A wall-clock time of day, `HH:MM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeOfDay {
    /// 0 to 23.
    pub hour: u8,
    /// 0 to 59.
    pub minute: u8,
}

/// What kind of problem a [`ConfigError`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigErrorKind {
    /// The text is not TOML.
    Syntax,
    /// `schema_version` is missing or not the one this build reads.
    SchemaVersion,
    /// A required key or table is absent.
    Missing,
    /// A key this schema does not have.
    Unknown,
    /// A key that existed in a plan and was refused by a decision, with its reason.
    Refused,
    /// A value of the wrong TOML type.
    WrongType,
    /// A value of the right type outside its allowed range or shape.
    Invalid,
    /// A value that repeats another's.
    Duplicate,
}

/// One problem: where, what, and how to fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// The field, as a path (`pools[1].tier`), or `line L, column C` for a syntax error.
    pub field: String,
    /// What kind of problem it is.
    pub kind: ConfigErrorKind,
    /// What is wrong. Never repeats a secret or the text of a refused reference.
    pub problem: String,
    /// What to change.
    pub fix: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} (fix: {})", self.field, self.problem, self.fix)
    }
}

/// Every problem found in one configuration, in the order the schema is read. Never empty.
///
/// `Display` is the operator-facing report: a count line, then one line per problem.
/// Pinned by `tests/config.rs::should_refuse_the_input_when_a_fixture_is_invalid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigErrors(pub Vec<ConfigError>);

impl ConfigErrors {
    /// The exit code a daemon or `mayfly config validate` ends with (ADR 0031).
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        exit_code::CONFIG_INVALID
    }
}

impl fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.0.len();
        write!(
            f,
            "invalid configuration: {count} {}",
            if count == 1 { "error" } else { "errors" }
        )?;
        for error in &self.0 {
            write!(f, "\n  {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigErrors {}

/// Why a configuration file did not load.
#[derive(Debug)]
pub enum LoadError {
    /// The file could not be read.
    Read {
        /// The path asked for.
        path: PathBuf,
        /// The operating system's reason.
        source: std::io::Error,
    },
    /// The file was read and is not valid.
    Invalid(ConfigErrors),
}

impl LoadError {
    /// The exit code a daemon or `mayfly config validate` ends with (ADR 0031).
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        exit_code::CONFIG_INVALID
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "cannot read configuration {}: {source}", path.display())
            }
            Self::Invalid(errors) => errors.fmt(f),
        }
    }
}

impl std::error::Error for LoadError {}

impl Config {
    /// Reads and validates the file at `path`.
    ///
    /// # Errors
    ///
    /// [`LoadError::Read`] when the file cannot be read, [`LoadError::Invalid`] with every
    /// problem when it is not a valid configuration.
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        let text = std::fs::read_to_string(path).map_err(|source| LoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text).map_err(LoadError::Invalid)
    }

    /// Validates configuration text. Pure: reads no file and no environment; a path in the
    /// text is checked for shape, never for existence (`doctor` checks the host, OPS-01).
    ///
    /// # Errors
    ///
    /// Every problem found, never empty.
    pub fn parse(text: &str) -> Result<Self, ConfigErrors> {
        let table: Table = match toml::from_str(text) {
            Ok(table) => table,
            Err(error) => return Err(syntax_error(text, &error)),
        };
        if let Some(error) = schema_version_error(&table) {
            return Err(ConfigErrors(vec![error]));
        }
        let mut root = Reader::new(
            String::new(),
            table,
            &[
                "schema_version",
                "state_dir",
                "github",
                "host",
                "provider",
                "images",
                "pools",
            ],
        );
        root.table.remove("schema_version");
        let state_dir = root.path("state_dir", "state_dir = \"/var/lib/mayfly\"", false);
        let github = root.read_section("github", true, read_github);
        let host = root.read_section("host", false, |reader| Some(read_host(reader)));
        let provider = root.read_section("provider", false, read_provider);
        let images = root.read_section("images", false, read_images);
        let pools = read_pools(&mut root);
        let errors = root.finish();
        match (github, pools) {
            (Some(github), Some(pools)) if errors.is_empty() => Ok(Self {
                state_dir,
                github,
                host: host.unwrap_or_default(),
                provider: provider.unwrap_or_default(),
                images,
                pools,
            }),
            _ => Err(ConfigErrors(errors)),
        }
    }
}

fn syntax_error(text: &str, error: &toml::de::Error) -> ConfigErrors {
    let field = error.span().map_or_else(
        || "line 1, column 1".to_owned(),
        |span| {
            let before = &text[..span.start.min(text.len())];
            let line = before.matches('\n').count() + 1;
            let column = before
                .rsplit('\n')
                .next()
                .map_or(0, |last| last.chars().count())
                + 1;
            format!("line {line}, column {column}")
        },
    );
    // Only the parser's own message, cut short: its source snippet would repeat the line, and
    // a line may hold a secret pasted where a reference belongs.
    let message: String = error.message().chars().take(80).collect();
    ConfigErrors(vec![ConfigError {
        field,
        kind: ConfigErrorKind::Syntax,
        problem: format!("not valid TOML: {message}"),
        fix: "correct the TOML syntax at that position".to_owned(),
    }])
}

fn schema_version_error(table: &Table) -> Option<ConfigError> {
    let fix = format!("start the file with `schema_version = {SCHEMA_VERSION}`");
    let problem = match table.get("schema_version") {
        None => "missing".to_owned(),
        Some(Value::Integer(found)) if *found == SCHEMA_VERSION => return None,
        Some(Value::Integer(found)) => format!(
            "schema_version {found} is not readable by this build, which reads {SCHEMA_VERSION}"
        ),
        Some(other) => format!("expected an integer, found {}", describe(other)),
    };
    Some(ConfigError {
        field: "schema_version".to_owned(),
        kind: ConfigErrorKind::SchemaVersion,
        problem,
        fix,
    })
}

fn describe(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "a string",
        Value::Integer(_) => "an integer",
        Value::Float(_) => "a float",
        Value::Boolean(_) => "a boolean",
        Value::Datetime(_) => "a date or time",
        Value::Array(_) => "an array",
        Value::Table(_) => "a table",
    }
}

fn enum_text(text: &str) -> String {
    let shown: String = text.chars().take(32).collect();
    format!("{shown:?}")
}

/// One table being read: the keys not yet taken, and the problems found so far.
struct Reader {
    path: String,
    table: Table,
    known: &'static [&'static str],
    errors: Vec<ConfigError>,
}

impl Reader {
    fn new(path: String, table: Table, known: &'static [&'static str]) -> Self {
        Self {
            path,
            table,
            known,
            errors: Vec::new(),
        }
    }

    fn field(&self, key: &str) -> String {
        if self.path.is_empty() {
            key.to_owned()
        } else {
            format!("{}.{key}", self.path)
        }
    }

    fn push(&mut self, key: &str, kind: ConfigErrorKind, problem: String, fix: &str) {
        let field = self.field(key);
        self.errors.push(ConfigError {
            field,
            kind,
            problem,
            fix: fix.to_owned(),
        });
    }

    fn invalid(&mut self, key: &str, problem: &str, fix: &str) {
        self.push(key, ConfigErrorKind::Invalid, problem.to_owned(), fix);
    }

    fn value(&mut self, key: &str, example: &str, required: bool) -> Option<Value> {
        let value = self.table.remove(key);
        if value.is_none() && required {
            self.push(
                key,
                ConfigErrorKind::Missing,
                "required, but absent".to_owned(),
                &format!("add `{example}`"),
            );
        }
        value
    }

    fn string(&mut self, key: &str, example: &str, required: bool) -> Option<String> {
        match self.value(key, example, required)? {
            Value::String(text) => Some(text),
            other => {
                self.wrong_type(key, "a string", &other, example);
                None
            }
        }
    }

    fn wrong_type(&mut self, key: &str, expected: &str, found: &Value, example: &str) {
        self.push(
            key,
            ConfigErrorKind::WrongType,
            format!("expected {expected}, found {}", describe(found)),
            &format!("write it as `{example}`"),
        );
    }

    fn uint(
        &mut self,
        key: &str,
        range: &RangeInclusive<u64>,
        example: &str,
        required: bool,
    ) -> Option<u64> {
        let found = match self.value(key, example, required)? {
            Value::Integer(found) => found,
            other => {
                self.wrong_type(key, "an integer", &other, example);
                return None;
            }
        };
        match u64::try_from(found) {
            Ok(number) if range.contains(&number) => Some(number),
            _ => {
                let (problem, fix) = if *range.end() >= i64::MAX.cast_unsigned() {
                    (
                        format!("{found} is below {}", range.start()),
                        format!("use {} or more", range.start()),
                    )
                } else {
                    (
                        format!("{found} is outside {} to {}", range.start(), range.end()),
                        format!("use a value from {} to {}", range.start(), range.end()),
                    )
                };
                self.push(key, ConfigErrorKind::Invalid, problem, &fix);
                None
            }
        }
    }

    fn narrow(&mut self, key: &str, range: RangeInclusive<u64>, example: &str) -> Option<u32> {
        self.uint(key, &range, example, false)
            .and_then(|number| u32::try_from(number).ok())
    }

    fn path(&mut self, key: &str, example: &str, required: bool) -> Option<PathBuf> {
        let text = self.string(key, example, required)?;
        let fix = format!("use an absolute path, `{example}`; `~` and `..` are not expanded");
        if text.is_empty() || text.contains('\0') {
            self.invalid(key, "is empty or contains a NUL byte", &fix);
            return None;
        }
        let path = PathBuf::from(&text);
        if !path.is_absolute() {
            self.invalid(key, "is not an absolute path", &fix);
            return None;
        }
        if path.components().any(|part| part == Component::ParentDir) {
            self.invalid(key, "has a `..` component", &fix);
            return None;
        }
        Some(path)
    }

    fn duration(
        &mut self,
        key: &str,
        range: &RangeInclusive<Duration>,
        example: &str,
    ) -> Option<Duration> {
        let text = self.string(key, example, false)?;
        let fix = format!("write a whole number and one unit s, m, h or d, like `{example}`");
        let Some(duration) = parse_duration(&text) else {
            self.invalid(key, "is not a duration", &fix);
            return None;
        };
        if !range.contains(&duration) {
            self.push(
                key,
                ConfigErrorKind::Invalid,
                format!(
                    "is outside {} to {}",
                    show_duration(*range.start()),
                    show_duration(*range.end())
                ),
                &format!(
                    "use a duration from {} to {}",
                    show_duration(*range.start()),
                    show_duration(*range.end())
                ),
            );
            return None;
        }
        Some(duration)
    }

    fn section(&mut self, key: &str, required: bool) -> Option<Self> {
        let example = format!("[{key}]");
        match self.value(key, &example, required)? {
            Value::Table(table) => Some(Self::new(self.field(key), table, section_keys(key))),
            other => {
                self.wrong_type(key, "a table", &other, &example);
                None
            }
        }
    }

    fn absorb(&mut self, child: Self) {
        let errors = child.finish();
        self.errors.extend(errors);
    }

    fn read_section<T>(
        &mut self,
        key: &str,
        required: bool,
        read: impl FnOnce(&mut Self) -> Option<T>,
    ) -> Option<T> {
        let mut child = self.section(key, required)?;
        let value = read(&mut child);
        self.absorb(child);
        value
    }

    /// The problems found, with every key not taken reported as refused or unknown.
    fn finish(mut self) -> Vec<ConfigError> {
        let leftover: Vec<String> = self.table.keys().cloned().collect();
        for key in leftover {
            let known = self.known.join(", ");
            let (kind, problem, fix) = match refusal(&key) {
                Some((problem, fix)) => {
                    (ConfigErrorKind::Refused, problem.to_owned(), fix.to_owned())
                }
                None => (
                    ConfigErrorKind::Unknown,
                    "unknown key; unknown keys are refused, never ignored".to_owned(),
                    format!("remove it, or use one of: {known}"),
                ),
            };
            self.push(&key, kind, problem, &fix);
        }
        self.errors
    }
}

/// Keys the plans once had and a decision removed, each with its reason (never renamed back).
fn refusal(key: &str) -> Option<(&'static str, &'static str)> {
    match key {
        "allow_untrusted_prs" => Some((
            "untrusted pull-request code is not supported in v0.x (ADR 0027), and this switch is never honoured",
            "remove it and give each pool `tier = \"trusted\"`",
        )),
        "mode" => Some((
            "classic runner registration is not supported (ADR 0028): mayfly serves GitHub Runner Scale Sets only",
            "remove `mode`",
        )),
        _ => None,
    }
}

fn section_keys(key: &str) -> &'static [&'static str] {
    match key {
        "github" => &[
            "organisation",
            "repository",
            "app_client_id",
            "installation_id",
            "private_key",
        ],
        "host" => &[
            "reserve_memory_mib",
            "reserve_disk_gib",
            "vm_slots",
            "macos_guests",
            "image_disk_gib",
            "clone_disk_gib",
        ],
        "images" => &["repository", "runners", "realm_hosts"],
        _ => &["kind", "tart_home"],
    }
}

const POOL_KEYS: &[&str] = &[
    "name",
    "tier",
    "image_manifest",
    "guest_os",
    "cpu",
    "memory_mib",
    "min_prepared",
    "max_prepared_age",
    "max_running",
    "breaker_failures",
    "breaker_window",
    "runner_refresh_at",
    "capacity_policy",
];

fn parse_duration(text: &str) -> Option<Duration> {
    let unit = text.chars().last()?;
    let number = text.get(..text.len() - unit.len_utf8())?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seconds = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return None,
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|count| count.checked_mul(seconds))
        .map(Duration::from_secs)
}

fn show_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds.is_multiple_of(86_400) {
        format!("{}d", seconds / 86_400)
    } else if seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}m", seconds / 60)
    }
}

fn parse_time_of_day(text: &str) -> Option<TimeOfDay> {
    let (hour, minute) = text.split_once(':')?;
    let two_digits = |part: &str| part.len() == 2 && part.bytes().all(|b| b.is_ascii_digit());
    if !two_digits(hour) || !two_digits(minute) {
        return None;
    }
    let (hour, minute) = (hour.parse::<u8>().ok()?, minute.parse::<u8>().ok()?);
    (hour < 24 && minute < 60).then_some(TimeOfDay { hour, minute })
}

fn organisation_ok(text: &str) -> bool {
    (1..=39).contains(&text.len())
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !text.starts_with('-')
        && !text.ends_with('-')
}

fn repository_ok(text: &str) -> bool {
    (1..=100).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && text != "."
        && text != ".."
}

fn client_id_ok(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn read_github(reader: &mut Reader) -> Option<GithubConfig> {
    let organisation = reader.string("organisation", "organisation = \"example-org\"", true);
    let repository = reader.string("repository", "repository = \"example-repo\"", false);
    let app_client_id = reader.string(
        "app_client_id",
        "app_client_id = \"Iv1.0123456789abcdef\"",
        true,
    );
    let installation_id = reader.uint(
        "installation_id",
        &(1..=i64::MAX.cast_unsigned()),
        "installation_id = 12345678",
        true,
    );
    let private_key = reader.secret(
        "private_key",
        "private_key = \"file:/etc/mayfly/app-key.pem\"",
    );

    let organisation = organisation.filter(|text| {
        let ok = organisation_ok(text);
        if !ok {
            reader.invalid(
                "organisation",
                "is not a GitHub organisation login",
                "use 1 to 39 letters, digits and hyphens, not starting or ending with a hyphen",
            );
        }
        ok
    });
    let repository = repository.filter(|text| {
        let ok = repository_ok(text);
        if !ok {
            reader.invalid(
                "repository",
                "is not a GitHub repository name",
                "use 1 to 100 letters, digits, `-`, `_` and `.`; give the name only, the owner is `organisation`",
            );
        }
        ok
    });
    let app_client_id = app_client_id.filter(|text| {
        let ok = client_id_ok(text);
        if !ok {
            reader.invalid(
                "app_client_id",
                "is not a GitHub App client id",
                "copy the client id from the App's settings page: letters, digits, `-`, `_` and `.`",
            );
        }
        ok
    });
    let organisation = organisation?;
    let scope = match repository {
        Some(repository) => GithubScope::Repository {
            organisation,
            repository,
        },
        None => GithubScope::Organisation { organisation },
    };
    Some(GithubConfig {
        scope,
        app_client_id: app_client_id?,
        installation_id: installation_id?,
        private_key: private_key?,
    })
}

fn read_host(reader: &mut Reader) -> HostConfig {
    let default = HostConfig::default();
    HostConfig {
        reserve_memory_mib: reader
            .uint(
                "reserve_memory_mib",
                &(0..=4_194_304),
                "reserve_memory_mib = 4096",
                false,
            )
            .unwrap_or(default.reserve_memory_mib),
        reserve_disk_gib: reader
            .uint(
                "reserve_disk_gib",
                &(0..=1_048_576),
                "reserve_disk_gib = 50",
                false,
            )
            .unwrap_or(default.reserve_disk_gib),
        vm_slots: reader.narrow("vm_slots", 1..=1024, "vm_slots = 2"),
        macos_guests: reader.narrow("macos_guests", 0..=2, "macos_guests = 2"),
        image_disk_gib: reader.uint(
            "image_disk_gib",
            &(1..=1_048_576),
            "image_disk_gib = 100",
            false,
        ),
        clone_disk_gib: reader.uint(
            "clone_disk_gib",
            &(1..=1_048_576),
            "clone_disk_gib = 100",
            false,
        ),
    }
}

fn image_repository_ok(text: &str) -> bool {
    let mut parts = text.split('/');
    let host = parts.next().unwrap_or("");
    let host_ok = !host.is_empty()
        && !host.starts_with(['.', ':'])
        && !host.contains("..")
        && host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-:".contains(&b));
    let path: Vec<&str> = parts.collect();
    host_ok
        && !path.is_empty()
        && path.iter().all(|p| {
            !p.is_empty()
                && *p != "."
                && *p != ".."
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        })
}

fn runner_version_ok(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_digit())
                && (*part == "0" || !part.starts_with('0'))
        })
}

/// The runner package platforms a pin may name (the runner's release names).
pub const RUNNER_PLATFORMS: [&str; 2] = ["linux-arm64", "osx-arm64"];

/// One `[images.runners]` entry: a checksum for every platform, or a table of them.
fn runner_pins(value: &Value) -> Option<BTreeMap<String, String>> {
    match value {
        Value::String(sha) if sha256_ok(sha) => Some(
            RUNNER_PLATFORMS
                .iter()
                .map(|p| ((*p).to_owned(), sha.clone()))
                .collect(),
        ),
        Value::Table(table) if !table.is_empty() => table
            .iter()
            .map(|(platform, sha)| match sha {
                Value::String(sha)
                    if RUNNER_PLATFORMS.contains(&platform.as_str()) && sha256_ok(sha) =>
                {
                    Some((platform.clone(), sha.clone()))
                }
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

fn sha256_ok(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn realm_host_ok(text: &str) -> bool {
    (1..=253).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-".contains(&b))
        && !text.starts_with(['.', '-'])
        && !text.contains("..")
}

fn read_images(reader: &mut Reader) -> Option<ImagesConfig> {
    let repository = reader
        .string(
            "repository",
            "repository = \"ghcr.io/acme/runner-macos\"",
            true,
        )
        .filter(|text| {
            let ok = image_repository_ok(text);
            if !ok {
                reader.invalid(
                    "repository",
                    "is not a bare OCI repository",
                    "write `host/path` in lowercase, with no scheme, tag or digest, like `ghcr.io/acme/runner-macos`",
                );
            }
            ok
        });
    let runners = match reader.value("runners", "[images.runners]", false) {
        None => Some(BTreeMap::new()),
        Some(Value::Table(table)) => {
            let mut runners = BTreeMap::new();
            let mut ok = true;
            for (version, pins) in table {
                match runner_pins(&pins) {
                    Some(pins) if runner_version_ok(&version) => {
                        runners.insert(version, pins);
                    }
                    _ => {
                        ok = false;
                        reader.invalid(
                            "runners",
                            "holds an entry that is not a runner version and its checksum",
                            "write `\"X.Y.Z\" = \"<64 lowercase hex digits>\"`, the SHA-256 of that runner package",
                        );
                    }
                }
            }
            ok.then_some(runners)
        }
        Some(other) => {
            reader.wrong_type("runners", "a table", &other, "[images.runners]");
            None
        }
    };
    let realm_hosts =
        match reader.value("realm_hosts", "realm_hosts = [\"auth.example.com\"]", false) {
            None => Some(Vec::new()),
            Some(Value::Array(items)) => {
                let count = items.len();
                let hosts: Vec<String> = items
                    .into_iter()
                    .filter_map(|item| match item {
                        Value::String(host) if realm_host_ok(&host) => Some(host),
                        _ => None,
                    })
                    .collect();
                if hosts.len() == count {
                    Some(hosts)
                } else {
                    reader.invalid(
                        "realm_hosts",
                        "holds an entry that is not a lowercase host name",
                        "list host names only, like `realm_hosts = [\"auth.example.com\"]`",
                    );
                    None
                }
            }
            Some(other) => {
                reader.wrong_type(
                    "realm_hosts",
                    "an array of host names",
                    &other,
                    "realm_hosts = [\"auth.example.com\"]",
                );
                None
            }
        };
    Some(ImagesConfig {
        repository: repository?,
        runners: runners?,
        realm_hosts: realm_hosts?,
    })
}

fn read_provider(reader: &mut Reader) -> Option<ProviderConfig> {
    let kind = match reader.string("kind", "kind = \"tart\"", false) {
        None => Some(ProviderKind::Tart),
        Some(text) if text == "tart" => Some(ProviderKind::Tart),
        Some(text) => {
            reader.push(
                "kind",
                ConfigErrorKind::Invalid,
                format!("{} is not a provider", enum_text(&text)),
                "use `kind = \"tart\"`, the only provider",
            );
            None
        }
    };
    let tart_home = reader.path("tart_home", "tart_home = \"/var/lib/mayfly/tart\"", false);
    kind.map(|kind| ProviderConfig { kind, tart_home })
}

fn read_pools(root: &mut Reader) -> Option<Vec<PoolConfig>> {
    let items = match root.value("pools", "[[pools]]", true)? {
        Value::Array(items) => items,
        other => {
            root.wrong_type("pools", "an array of tables", &other, "[[pools]]");
            return None;
        }
    };
    if items.is_empty() {
        root.invalid(
            "pools",
            "is empty: a daemon with no pool serves nothing",
            "add a `[[pools]]` table",
        );
        return None;
    }
    let mut pools = Vec::new();
    let mut complete = true;
    for (index, item) in items.into_iter().enumerate() {
        let path = format!("pools[{index}]");
        let Value::Table(table) = item else {
            root.errors.push(ConfigError {
                field: path,
                kind: ConfigErrorKind::WrongType,
                problem: format!("expected a table, found {}", describe(&item)),
                fix: "write each pool as a `[[pools]]` table".to_owned(),
            });
            complete = false;
            continue;
        };
        let mut child = Reader::new(path, table, POOL_KEYS);
        let pool = read_pool(&mut child);
        root.absorb(child);
        match pool {
            Some(pool) => pools.push((index, pool)),
            None => complete = false,
        }
    }
    for (position, (index, pool)) in pools.iter().enumerate() {
        if let Some((first, _)) = pools[..position]
            .iter()
            .find(|(_, other)| other.name == pool.name)
        {
            root.errors.push(ConfigError {
                field: format!("pools[{index}].name"),
                kind: ConfigErrorKind::Duplicate,
                problem: format!(
                    "repeats the name of pools[{first}]; one pool is one scale set (ADR 0040)"
                ),
                fix: "give each pool its own name".to_owned(),
            });
            complete = false;
        }
    }
    complete.then(|| pools.into_iter().map(|(_, pool)| pool).collect())
}

fn read_pool(reader: &mut Reader) -> Option<PoolConfig> {
    let name = reader.string("name", "name = \"mac\"", true).and_then(|text| {
        PoolName::new(&text)
            .inspect_err(|_| {
                reader.invalid(
                    "name",
                    "is not a pool name",
                    "use 1 to 16 lowercase letters and digits: the name becomes the scale set label and part of every VM name",
                );
            })
            .ok()
    });
    let tier = reader
        .string("tier", "tier = \"trusted\"", true)
        .and_then(|text| {
            if text == "trusted" {
                Some(Tier::Trusted)
            } else {
                reader.push(
                    "tier",
                    ConfigErrorKind::Invalid,
                    format!(
                        "{} is not supported: v0.x serves the trusted tier only (ADR 0027)",
                        enum_text(&text)
                    ),
                    "use `tier = \"trusted\"`",
                );
                None
            }
        });
    let image_manifest = reader.path(
        "image_manifest",
        "image_manifest = \"/etc/mayfly/images/mac.toml\"",
        true,
    );
    let guest_os = reader
        .string("guest_os", "guest_os = \"macos\"", true)
        .and_then(|text| match text.as_str() {
            "macos" => Some(GuestOs::Macos),
            "linux" => Some(GuestOs::Linux),
            _ => {
                reader.push(
                    "guest_os",
                    ConfigErrorKind::Invalid,
                    format!("{} is not a guest OS", enum_text(&text)),
                    "use `guest_os = \"macos\"` or `guest_os = \"linux\"`",
                );
                None
            }
        });
    let cpu = reader
        .uint("cpu", &(1..=256), "cpu = 4", true)
        .and_then(|number| u32::try_from(number).ok());
    let memory_mib = reader.uint("memory_mib", &(1024..=4_194_304), "memory_mib = 8192", true);
    let max_running = reader
        .uint("max_running", &(1..=1024), "max_running = 2", true)
        .and_then(|number| u32::try_from(number).ok());
    let min_prepared = reader.narrow("min_prepared", 0..=1024, "min_prepared = 0");
    let day = Duration::from_secs(86_400);
    let max_prepared_age = reader.duration(
        "max_prepared_age",
        &(Duration::from_secs(60)..=7 * day),
        "1h",
    );
    let breaker_failures = reader.narrow("breaker_failures", 1..=1000, "breaker_failures = 3");
    let breaker_window = reader.duration("breaker_window", &(Duration::from_secs(60)..=day), "30m");
    let runner_refresh_at = reader
        .string("runner_refresh_at", "runner_refresh_at = \"03:30\"", false)
        .and_then(|text| {
            let parsed = parse_time_of_day(&text);
            if parsed.is_none() {
                reader.invalid(
                    "runner_refresh_at",
                    "is not a time of day",
                    "write `HH:MM` in 24-hour host local time, like `runner_refresh_at = \"03:30\"`",
                );
            }
            parsed
        });

    let capacity_policy = reader
        .string("capacity_policy", "capacity_policy = \"exact\"", false)
        .and_then(|text| match text.as_str() {
            "exact" => Some(CapacityPolicy::Exact),
            "demand" => Some(CapacityPolicy::Demand),
            _ => {
                reader.push(
                    "capacity_policy",
                    ConfigErrorKind::Invalid,
                    format!("{} is not a capacity policy", enum_text(&text)),
                    "use `capacity_policy = \"exact\"` (the default) or `capacity_policy = \"demand\"`",
                );
                None
            }
        });

    let min_prepared_value = min_prepared.unwrap_or(0);
    if let Some(running) = max_running
        && min_prepared_value > running
    {
        reader.invalid(
            "min_prepared",
            "is larger than max_running: prepared VMs count against the pool's limit",
            "lower `min_prepared` or raise `max_running`",
        );
    }
    Some(PoolConfig {
        name: name?,
        tier: tier?,
        image_manifest: image_manifest?,
        guest_os: guest_os?,
        cpu: cpu?,
        memory_mib: memory_mib?,
        min_prepared: min_prepared_value,
        max_prepared_age: max_prepared_age.unwrap_or(Duration::from_secs(3600)),
        max_running: max_running?,
        breaker: BreakerConfig {
            failures: breaker_failures.unwrap_or(3),
            window: breaker_window.unwrap_or(Duration::from_secs(1800)),
        },
        runner_refresh_at,
        capacity_policy: capacity_policy.unwrap_or_default(),
    })
}

impl Reader {
    fn secret(&mut self, key: &str, example: &str) -> Option<SecretRef> {
        let text = self.string(key, example, true)?;
        match text.parse::<SecretRef>() {
            Ok(reference) => Some(reference),
            Err(error) => {
                // The refused text is never repeated: it may be the secret itself.
                self.push(
                    key,
                    ConfigErrorKind::Invalid,
                    error.to_string(),
                    &format!("write a reference like `{example}`"),
                );
                None
            }
        }
    }
}
