//! The operator socket's wire format, API 1 (plan §11, RT-03): one JSON document per line
//! (NDJSON) each way over the `0600` Unix socket.
//!
//! A request is one line, `{"api":1,"command":"<command>", ...}` ([`Request`]); the daemon
//! answers with one line ([`Answer`]), or for `events` with one line per event until either side
//! closes. An answer is `{"api":1,"ok":true,"result":{...}}` or
//! `{"api":1,"ok":false,"error":{"code":"<snake_case>","message":"..."}}`.
//!
//! **A compatibility surface** (`doc/event-store.md` §6, "is an older build that ignores this
//! still correct?"). Fields are only ever added, optional on the side that reads them, and
//! never renamed: both ends ignore fields they do not know (no `deny_unknown_fields`), so an
//! older CLI reads a newer daemon's status and an older daemon serves a newer CLI's request.
//! A command an older daemon does not know is answered `unknown_command`, never guessed at; a
//! request of a higher `api` than the daemon serves is answered `unsupported_api`. Nothing a
//! client sends names an actor: the daemon decides every mutating command under the peer's uid
//! read from the connection (KER-10F). Pinned by
//! `tests/operator.rs::should_ignore_an_unknown_field_when_a_newer_client_sends_one` and
//! `should_answer_unknown_command_when_the_daemon_does_not_know_it`.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
pub use serde_json::Value;

/// The API version this build speaks.
pub const API: u32 = 1;

/// A request, tagged by `command`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    /// `mayfly status`: [`Status`].
    Status,
    /// `mayfly vm list`: [`VmList`].
    VmList,
    /// `mayfly pool list`: [`PoolList`].
    PoolList,
    /// `mayfly pool pause <pool>`.
    PoolPause {
        /// The pool.
        pool: String,
    },
    /// `mayfly pool resume <pool>`.
    PoolResume {
        /// The pool.
        pool: String,
    },
    /// `mayfly pool drain <pool>`.
    PoolDrain {
        /// The pool.
        pool: String,
    },
    /// `mayfly pool reset-breaker <pool> [--digest <digest>]`: [`Reset`].
    PoolResetBreaker {
        /// The pool.
        pool: String,
        /// The digest whose breaker to reset; absent, every tripped one.
        #[serde(default)]
        digest: Option<String>,
    },
    /// `mayfly host drain [--deadline <duration>]`: this daemon's host.
    HostDrain {
        /// Seconds busy attempts are given; absent, [`DEFAULT_DRAIN_SECS`].
        #[serde(default)]
        deadline_secs: Option<u64>,
    },
    /// `mayfly host resume`: this daemon's host.
    HostResume,
    /// `mayfly events [--follow] [--since <duration>]`: [`EventLine`]s, history first; with
    /// `follow`, a [`Following`] line comes first.
    Events {
        /// Keep the connection open and send each newly committed event.
        #[serde(default)]
        follow: bool,
        /// History: the events recorded in the last this many seconds (at most
        /// [`MAX_SINCE_SECS`]), first.
        #[serde(default)]
        since_secs: Option<u64>,
    },
}

/// The longest history `events --since` reads: seven days. Reading history enumerates every
/// stream (there is no journal query by time yet, RT-09), so the window is bounded.
pub const MAX_SINCE_SECS: u64 = 7 * 86_400;

/// The drain deadline when the operator names none: six hours, GitHub-hosted runners' own job
/// limit, so a typical job finishes; past it the drain alerts and never forces (P23).
pub const DEFAULT_DRAIN_SECS: u64 = 6 * 3600;

impl Request {
    /// The request as its line's document: the command's fields and `"api"`.
    #[must_use]
    pub fn to_document(&self) -> Value {
        let mut document = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Value::Object(map) = &mut document {
            map.insert("api".to_owned(), Value::from(API));
        }
        document
    }
}

/// Every command this build serves, as `command` names it.
pub const COMMANDS: &[&str] = &[
    "status",
    "vm_list",
    "pool_list",
    "pool_pause",
    "pool_resume",
    "pool_drain",
    "pool_reset_breaker",
    "host_drain",
    "host_resume",
    "events",
];

/// Why a request was not served.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    /// A fixed `snake_case` code: `unauthorised`, `busy` (every connection slot is taken: try
    /// again; the CLI exits 75), `bad_request`, `unknown_command`, `unsupported_api`,
    /// `not_running`, `query_failed`, `save_failed`, or an aggregate's refusal code
    /// (`already_paused`, `not_tripped`, `operator_only`, ...).
    pub code: String,
    /// What to do about it, for a person.
    pub message: String,
}

/// One answer line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    /// The API version of the daemon that answered.
    pub api: u32,
    /// Whether the request was served.
    pub ok: bool,
    /// What it returned, when served.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Why not, when not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
}

impl Answer {
    /// A served request's answer.
    #[must_use]
    pub fn served<T: Serialize>(result: &T) -> Self {
        Self {
            api: API,
            ok: true,
            result: Some(serde_json::to_value(result).unwrap_or(Value::Null)),
            error: None,
        }
    }

    /// A refused request's answer.
    #[must_use]
    pub fn failed(code: &str, message: impl Into<String>) -> Self {
        Self {
            api: API,
            ok: false,
            result: None,
            error: Some(Failure {
                code: code.to_owned(),
                message: message.into(),
            }),
        }
    }
}

/// `status`. Every field is read from the journal's committed projection rows when the request
/// is served (each projection page by page, not one snapshot: `mayfly_control::queries`), the
/// run from the `HostRun` stream, the sizing from the daemon's start-up reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// This daemon's host id.
    pub host: String,
    /// This daemon's build version.
    pub version: String,
    /// The current run.
    pub run: RunStatus,
    /// The host's sizing in effect at start-up, each value with its source (RT-06): what
    /// `mayflyd` wrote to stderr until RT-03.
    pub sizing: Vec<SizingValue>,
    /// Listed images whose disk did not measure at start-up (their disk is unknown).
    pub unmeasured_images: Vec<String>,
    /// Every configured pool, by name.
    pub pools: Vec<PoolStatus>,
    /// Every registered host, by id.
    pub hosts: Vec<HostStatus>,
    /// The attempts.
    pub attempts: AttemptStatus,
}

/// The run of a [`Status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStatus {
    /// The run's number, from 1.
    pub number: u64,
    /// The run's epoch (0 before it is minted).
    pub epoch: u64,
    /// `starting`, `recovering`, `operating`, `shutting_down` or `ended` (the `HostRun`
    /// aggregate's phase).
    pub phase: String,
    /// **The run's ready signal**: `true` once this run reported its ready line, with exactly
    /// the ready line's promise (`mayfly_composition::daemon::Ready`: recovery finished, every
    /// durable consumer caught up once, one terms pass completed); `false` before it and from
    /// the start of every later run until that run's line. It is not derived from `phase`:
    /// `phase` reads `operating` as soon as recovery completed, before the consumers and the
    /// terms pass that `ready` also waits for, so wait on `ready`, never on `phase`.
    pub ready: bool,
    /// Why the host is in safe mode (`epoch_below_floor`, `unknown_event_kind`,
    /// `verification_failed`, `crash_loop`), or `null` when it is not.
    pub safe_mode: Option<String>,
}

/// One sizing value of a [`Status`], exactly as the start-up report had it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizingValue {
    /// The `[host]` key.
    pub key: String,
    /// The value in the key's unit, `per_image`, or `unknown`.
    pub value: String,
    /// `configured`, `configured_capped_at_apple_limit`, `measured`, `manifest`,
    /// `apple_limit` or `default`.
    pub source: String,
    /// Why it is unknown (`cpus_unreadable`, `probe_unreadable`), when it is.
    pub reason: Option<String>,
}

/// One pool of a [`Status`] or [`PoolList`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolStatus {
    /// The pool.
    pub name: String,
    /// `active`, `paused`, `draining`, `drained`, `decommissioned` or `unconfigured`.
    pub mode: String,
    /// The digest the pool is pinned to.
    pub image: Option<String>,
    /// Its `max_running`.
    pub max_running: Option<u32>,
    /// Its `min_prepared`.
    pub min_prepared: Option<u32>,
    /// Its tripped breakers.
    pub tripped: Vec<Tripped>,
    /// **Where the pool can run** (VER-09's signal): one entry per registered host. The
    /// pool is admitted on a host once `terms_open` is `true` and `placement` is
    /// `materialised`; `leases` counts the slots the host has reserved for it.
    pub hosts: Vec<PoolOnHost>,
}

/// A tripped breaker of a [`PoolStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tripped {
    /// The image.
    pub digest: String,
    /// When it tripped, Unix-epoch milliseconds.
    pub at: u64,
}

/// A pool on a host, in [`PoolStatus::hosts`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolOnHost {
    /// The host.
    pub host: String,
    /// Whether the host holds the pool's terms open (P4, CP-08): the `Host` stream's committed
    /// terms for the pool as its projection row reads now.
    pub terms_open: bool,
    /// The phase of the placement of the pool's pinned image on the host, from its committed
    /// projection row (`wanted`, `pulling`, `pulled`, `materialised`, `evicting`, `evicted`,
    /// `failed`), or `null` when the pool pins nothing or nothing was requested there.
    /// `failed` is final for that pin on that host (a refused pull or first materialisation).
    pub placement: Option<String>,
    /// The live leases the host holds for the pool (reserved, activated or quarantined), from
    /// the committed `slots` rows: at least 1 once the reconciler reserved for the pool there.
    pub leases: u32,
}

/// One host of a [`Status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatus {
    /// The host.
    pub host: String,
    /// Whether its admission gate is open.
    pub admission_open: bool,
    /// `open`, `draining` or `drained`.
    pub drain: String,
    /// Its live leases.
    pub leases: u32,
}

/// The attempts of a [`Status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptStatus {
    /// How many attempts are in each phase.
    pub by_phase: BTreeMap<String, u32>,
    /// Directives in flight whose outcome is unknown (I10).
    pub unknown_effects: u32,
    /// Attempts whose VM vanished outside mayfly (DEC-07, ADR 0066 amended).
    pub desync: Vec<Desync>,
}

/// A desync: an attempt whose VM vanished outside mayfly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Desync {
    /// The attempt.
    pub attempt: String,
    /// Its pool.
    pub pool: String,
    /// Its VM.
    pub vm: String,
    /// How it was found: `inventory`, `stop_vm` or `start_vm`.
    pub discovered_by: String,
    /// The same in plain words.
    pub note: String,
}

/// `pool list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolList {
    /// Every configured pool, by name.
    pub pools: Vec<PoolStatus>,
}

/// `vm list`: every attempt whose VM may exist or vanished (all but `not_started`, `deleted`
/// and `closed`), by attempt id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmList {
    /// The VMs.
    pub vms: Vec<Vm>,
}

/// One VM of a [`VmList`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vm {
    /// The attempt.
    pub attempt: String,
    /// Its pool.
    pub pool: String,
    /// Its host.
    pub host: String,
    /// Its VM's name.
    pub vm: String,
    /// The attempt's phase.
    pub phase: String,
    /// The directive it waits on.
    pub pending: Option<String>,
    /// Whether that directive's outcome is unknown.
    pub pending_unknown: bool,
    /// The job (`runnerRequestId`) bound to it.
    pub job: Option<u64>,
    /// The desync, when its VM vanished outside mayfly (`inventory`, `stop_vm`, `start_vm`).
    pub desync: Option<String>,
    /// The desync in plain words.
    pub note: Option<String>,
}

/// `pool reset-breaker`'s result: the digests reset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reset {
    /// The digests whose breaker was reset.
    pub reset: Vec<String>,
}

/// One event of `events`, labelled with its stream and ordered for display by `recorded_at`
/// (`doc/event-store.md` §5.6: there is no global order; nothing decides from this).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventLine {
    /// When the journal recorded it, Unix-epoch milliseconds.
    pub recorded_at: u64,
    /// Its stream.
    pub stream: String,
    /// Its revision in the stream.
    pub revision: u64,
    /// Its kind.
    pub kind: String,
    /// Its schema version.
    pub schema_version: u32,
    /// Its payload, as stored.
    pub payload: Value,
}

/// **The follow started**: the first line of `events --follow`, sent once the daemon subscribed
/// to the store's after-commit notifications. Every unit committed after the client read it
/// is sent (while the follower keeps up, else a [`Lagged`] line). The history of `--since`, if
/// asked for, comes after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Following {
    /// Always `true`.
    pub following: bool,
}

/// A follower fell behind: this many committed units were not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lagged {
    /// The units lost.
    pub lagged: u64,
}

/// The plain words for a desync found by `discovered_by` (DEC-05, DEC-07).
#[must_use]
pub fn desync_note(discovered_by: &str) -> String {
    format!(
        "the VM vanished outside mayfly (found by {discovered_by}): a desync, not a failure of the job; send the diagnostics bundle"
    )
}

/// `value` read as `T`, or `None` when it is not one (an answer of a much newer daemon).
#[must_use]
pub fn read<T: DeserializeOwned>(value: &Value) -> Option<T> {
    serde_json::from_value(value.clone()).ok()
}

/// `value` as pretty-printed JSON, how `--json` prints an answer.
#[must_use]
pub fn pretty<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// `value` as one line of compact JSON, how a document goes on the wire.
#[must_use]
pub fn compact<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}
