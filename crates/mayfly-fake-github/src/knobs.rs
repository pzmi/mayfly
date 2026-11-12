//! The fake's named assumptions (ADR 0048, amended 2026-11-11).
//!
//! Every behaviour `doc/github-scaleset.md` marks client-assumed or `[open: HW-02]` is a field
//! here, with the default the spec takes, so a test that relies on it names it and HW-02's
//! capture can flip it. Settled, server-observed behaviour (the 409 on a second session, the
//! 202 hold, the cap at 0) is not a knob.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Which assignment flow the fake serves (spec §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flow {
    /// github.com: `JobAssigned` with `runnerRequestId` 0, no offer, no acquire, nothing assigned
    /// beyond the last poll's `maxCapacity`.
    Direct,
    /// GHES: `JobAvailable` offers, `acquirejobs` and `acquirablejobs`.
    Acquire,
}

/// What removing a runner that does not exist answers (spec §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbsentRemoval {
    /// 204, as for a removed runner (maintainer-stated).
    NoContent,
    /// 404 `AgentNotFoundException` (client-assumed).
    NotFound,
}

/// When a message's statistics are taken (A6, `[open: HW-02, items 1 and 4]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatisticsAt {
    /// When the message is built for its first delivery: the newest counts, the case where A6
    /// goes away.
    Build,
    /// When its last job message was emitted (A6's reading, the default): a message emitted before a session
    /// opened and delivered after it carries older counts than the opening's.
    Event,
}

/// What an acquire that names an id no longer available answers (`[open: HW-02, item 2]`, A4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartialAcquire {
    /// The answer lists only the ids acquired (A4, client-assumed).
    OmitUnavailable,
    /// Any unavailable id fails the whole request with 400.
    FailRequest,
}

/// A runner group the service knows, for the group lookup and the REST group read (spec §6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerGroup {
    /// Its id.
    pub id: u64,
    /// Its name.
    pub name: String,
    /// Whether it is the organisation's default group.
    pub default: bool,
    /// `allows_public_repositories` of the REST read.
    pub allows_public_repositories: bool,
}

/// The organisation's fork-PR setting for private repositories (spec §2, REST).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkPrSetting {
    /// `run_workflows_from_fork_pull_requests`.
    pub run_workflows_from_fork_pull_requests: bool,
    /// `send_write_tokens_to_workflows`.
    pub send_write_tokens_to_workflows: bool,
    /// `send_secrets_and_variables`.
    pub send_secrets_and_variables: bool,
    /// `require_approval_for_fork_pr_workflows`.
    pub require_approval_for_fork_pr_workflows: bool,
}

/// The fake's assumptions and modes. [`Knobs::default`] is the spec's current reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Knobs {
    /// The assignment flow (spec §6.1). Default: github.com's direct flow.
    pub flow: Flow,
    /// How long a poll holds before answering 202 (about 50 s, server-observed, spec §4.1).
    #[serde(with = "millis")]
    pub poll_hold: Duration,
    /// The most job messages one message holds; a backlog is truncated into later messages
    /// (spec §5; 50, client-assumed from the reference's README).
    pub max_jobs_per_message: usize,
    /// The id of a scale set's first message (about `100000001` in 2026, server-observed,
    /// spec §4.1).
    pub first_message_id: u64,
    /// How long a message-queue token lives (about an hour, server-observed, spec §1).
    pub queue_token_lifetime_secs: u64,
    /// How long a session lives on its own, if at all (`[open: HW-02, item 5]`; default: until
    /// deleted or expired by the control API).
    pub session_lifetime_secs: Option<u64>,
    /// Whether a refresh answers a new `messageQueueUrl`, the old one then answering 404
    /// (`[open: HW-02, item 5]`; default no).
    pub refresh_moves_queue_url: bool,
    /// Whether a poll's `lastMessageId` also acknowledges every message at or below it
    /// (`[open: HW-02, item 4]`; default no: only a delete acks).
    pub last_message_id_acks: bool,
    /// The status of an ack for a message that is not queued, already acked or never sent
    /// (`[open: HW-02, item 4]`; default 404).
    pub ack_of_unknown_status: u16,
    /// The bytes of a 202 (`[open: HW-02, item 6]`; default empty).
    pub accepted_body: String,
    /// What removing an absent runner answers (spec §7, `[open: HW-02, item 9]`; default 204,
    /// maintainer-stated).
    pub absent_removal: AbsentRemoval,
    /// The status of a busy runner's removal, `JobStillRunningException` either way: 400
    /// (maintainer-stated) or 409 (client-assumed) (spec §7, `[open: HW-02, item 9]`; default 400).
    pub busy_removal_status: u16,
    /// The status a request about a deleted scale set answers (`[open: HW-02, item 9]`;
    /// default 404, client-assumed).
    pub gone_scale_set_status: u16,
    /// A partial acquire (`[open: HW-02, item 2]`, A4).
    pub partial_acquire: PartialAcquire,
    /// Whether re-acquiring a job this scale set already acquired lists it again (client-assumed
    /// no-op, spec §6.1, `[open: HW-02, item 2]`; default yes).
    pub reacquire_lists_again: bool,
    /// Whether a `JobCompleted` names no job: no `jobId` and no `workflowRunId` (`[open: HW-02,
    /// item 1]`, spec §10).
    pub completed_names_no_job: bool,
    /// How many times an assigned job no runner takes is released and requeued before it fails
    /// (three, client-assumed from the reference's README, spec §4.3, `[open: HW-02, item 2]`).
    pub max_releases: u32,
    /// Whether service calls must carry `api-version` in the query (the reference always sends
    /// it, client-assumed, spec §2; default yes).
    pub require_api_version: bool,
    /// Whether a second scale set created under a taken name answers the existing one instead of
    /// 409 (`[open: HW-02, item 9]`; default 409).
    pub duplicate_create_returns_existing: bool,
    /// Whether the messages of a scale set's queue that no ack removed survive the session and
    /// reach the next one (A1, `[open: HW-02, item 4]`; default yes). When not, a new session
    /// starts with an empty queue.
    pub queue_survives_session: bool,
    /// When a message's statistics are taken (A6, `[open: HW-02, items 1 and 4]`; default
    /// [`StatisticsAt::Event`], the spec's reading: the scale set's counts when the message was
    /// emitted).
    pub statistics_at: StatisticsAt,
    /// The base of every `messageQueueUrl`; `None` means the service URL. Its path prefixes the
    /// queue's routes; on github.com the queue lives on another host (spec §1).
    pub queue_url_base: Option<String>,
    /// The runner groups the service knows.
    pub runner_groups: Vec<RunnerGroup>,
    /// The organisation's fork-PR setting.
    pub fork_pr_setting: ForkPrSetting,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            flow: Flow::Direct,
            poll_hold: Duration::from_secs(50),
            max_jobs_per_message: 50,
            first_message_id: 100_000_001,
            queue_token_lifetime_secs: 3600,
            session_lifetime_secs: None,
            refresh_moves_queue_url: false,
            last_message_id_acks: false,
            ack_of_unknown_status: 404,
            accepted_body: String::new(),
            absent_removal: AbsentRemoval::NoContent,
            busy_removal_status: 400,
            gone_scale_set_status: 404,
            partial_acquire: PartialAcquire::OmitUnavailable,
            reacquire_lists_again: true,
            completed_names_no_job: false,
            max_releases: 3,
            require_api_version: true,
            duplicate_create_returns_existing: false,
            queue_survives_session: true,
            statistics_at: StatisticsAt::Event,
            queue_url_base: None,
            runner_groups: vec![RunnerGroup {
                id: 1,
                name: "Default".to_owned(),
                default: true,
                allows_public_repositories: false,
            }],
            fork_pr_setting: ForkPrSetting {
                run_workflows_from_fork_pull_requests: false,
                send_write_tokens_to_workflows: false,
                send_secrets_and_variables: false,
                require_approval_for_fork_pr_workflows: true,
            },
        }
    }
}

mod millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(u64::try_from(value.as_millis()).unwrap_or(u64::MAX))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_millis)
    }
}
