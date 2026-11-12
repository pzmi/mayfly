//! `mayfly_github_requests_total{op,status_class}` (plan §12): one count per attempt, labels
//! from two closed enums (foundation.md §5.4), never an id, a URL or upstream text.
//!
//! The adapter only counts; RT-04's endpoint renders the counts and pins the names.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

/// The `op` label: what a request was for. Its value set is this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Operation {
    /// The installation token (spec §1 step 2).
    InstallationToken,
    /// The registration token (spec §1 step 3).
    RegistrationToken,
    /// The runner-registration exchange for the admin token (spec §1 step 4).
    AdminExchange,
    /// A runner group by name (spec §2).
    RunnerGroup,
    /// A scale set by name or id (spec §2).
    ScaleSetRead,
    /// A scale set create (spec §2).
    ScaleSetCreate,
    /// A session create (spec §3.1).
    SessionCreate,
    /// A session refresh (spec §3.1).
    SessionRefresh,
    /// A session delete (spec §3.1).
    SessionDelete,
    /// The long poll (spec §4.1).
    Poll,
    /// A message ack (spec §4.4).
    Ack,
    /// An acquire (GHES, spec §6.1).
    AcquireJobs,
    /// The acquirable jobs (GHES, spec §6.1).
    AcquirableJobs,
    /// A JIT config (spec §6.3).
    JitConfig,
    /// A runner by id or by name (spec §2).
    RunnerRead,
    /// A runner removal (spec §7).
    RunnerRemove,
}

impl Operation {
    /// The label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InstallationToken => "installation_token",
            Self::RegistrationToken => "registration_token",
            Self::AdminExchange => "admin_exchange",
            Self::RunnerGroup => "runner_group",
            Self::ScaleSetRead => "scale_set_read",
            Self::ScaleSetCreate => "scale_set_create",
            Self::SessionCreate => "session_create",
            Self::SessionRefresh => "session_refresh",
            Self::SessionDelete => "session_delete",
            Self::Poll => "poll",
            Self::Ack => "ack",
            Self::AcquireJobs => "acquire_jobs",
            Self::AcquirableJobs => "acquirable_jobs",
            Self::JitConfig => "jit_config",
            Self::RunnerRead => "runner_read",
            Self::RunnerRemove => "runner_remove",
        }
    }
}

/// The `status_class` label: how an attempt ended. Its value set is this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StatusClass {
    /// 2xx.
    Success,
    /// 1xx or 3xx (redirects are not followed).
    Unexpected,
    /// A rate-limited 403 or 429 (spec §8).
    RateLimited,
    /// Another 4xx.
    ClientError,
    /// 5xx.
    ServerError,
    /// No HTTP answer.
    Transport,
    /// The attempt's deadline passed first.
    Timeout,
}

impl StatusClass {
    /// The label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "2xx",
            Self::Unexpected => "other",
            Self::RateLimited => "rate_limited",
            Self::ClientError => "4xx",
            Self::ServerError => "5xx",
            Self::Transport => "transport",
            Self::Timeout => "timeout",
        }
    }
}

/// One counter of the family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestCount {
    /// The `op` label.
    pub op: Operation,
    /// The `status_class` label.
    pub status_class: StatusClass,
    /// Attempts counted.
    pub count: u64,
}

/// The request counters, shared by every call of one client.
#[derive(Debug, Default)]
pub struct RequestMetrics {
    counts: Mutex<BTreeMap<(Operation, StatusClass), u64>>,
}

impl RequestMetrics {
    /// Counts one attempt.
    pub fn record(&self, op: Operation, status_class: StatusClass) {
        *self
            .counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry((op, status_class))
            .or_default() += 1;
    }

    /// Every non-zero counter, ordered by labels.
    #[must_use]
    pub fn snapshot(&self) -> Vec<RequestCount> {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(&(op, status_class), &count)| RequestCount {
                op,
                status_class,
                count,
            })
            .collect()
    }
}
