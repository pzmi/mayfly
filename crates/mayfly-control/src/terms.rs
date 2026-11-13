//! The terms policy (P4, `doc/domain-model.md` §7; CP-08): every pool's terms
//! ([`mayfly_control_domain::pool::terms`]: open, `max_running`, tier, at the pool's
//! `terms_revision`) applied on every registered host as `ApplyPoolTerms`, which is how a pool
//! change (configuration, pin, pause, resume, drain, a breaker trip or reset) reaches admission:
//! `Host` reserves only under terms it has applied, at their revision.
//!
//! **A level-triggered pass, not a durable consumer.** Domain-model §7 describes each policy as
//! a consumer of the events named in "Whenever"; P4 is built like P5 and P17 instead. A pass
//! compares each pool's current terms revision with the revision each host has applied (the
//! `hosts` projection) and applies the newer terms where a host lags. So a host registered after
//! the pool's last event gets its terms, a lost kick costs at most the interval, and nothing
//! leaves the journal (no effect to deliver, so no delivery ledger). Rejected alternative: a
//! consumer of pool events, which would miss a host registered later and needs a checkpoint per
//! pool stream for what a comparison of two revisions already says.
//!
//! **When.** At most once per [`TermsConfig::interval`] by the control plane's clock, and at once
//! when a pool or host stream moved ([`TermsPolicy::tick`] reads its [`Kicks`]).
//!
//! **Idempotent.** A command's id is derived from the pool, the terms revision and the host
//! (dm §7's key `(pool, revision)`, per host), so the same terms saved twice, by a repeated pass
//! or by two policies racing, are recorded once and the later save is answered by the receipt
//! (pinned by `tests/store/terms.rs::should_apply_nothing_twice_when_a_pass_is_repeated` and
//! `should_record_the_terms_once_when_two_passes_apply_the_same_terms`).
//!
//! **No gap opens admission.** Each pool's terms are read from its stream (the authoritative
//! `terms_revision`), and `ApplyPoolTerms` is saved on the host stream with a read of the pool
//! stream at the revision it was read at, under the store's revision retry: if the pool moved in
//! between, the unit is refused (`conflict`) and the retry reads the pool again, so a host never
//! applies terms a newer pool event has superseded (pinned by
//! `tests/store/terms.rs::should_apply_nothing_superseded_when_the_pool_moves_between_the_read_and_the_save`;
//! `formal/scheduling.qnt`'s `applyTerms` is the one atomic step this makes faithful). A host
//! stream that moved meanwhile (a reservation, a report) is retried the same way
//! (`should_apply_the_terms_in_the_same_pass_when_the_host_moves_between_the_read_and_the_save`).
//! An older revision than the host holds is refused by `Host` (`stale_terms`; pinned by
//! `tests/store/terms.rs::should_refuse_a_stale_revision_when_the_host_holds_a_newer_one`). A
//! pass that leaves something undecided (retries spent, a store fault, a pool unread) says so
//! ([`TermsReport::complete`]); the next pass reads again.
//!
//! **Safe mode** ([`TermsPolicy::set_safe_mode`]): a pass applies nothing, as the reconciler and
//! the placement manager decide nothing new; admission is closed in safe mode anyway, and the
//! first normal pass applies the terms as they then are (pinned by
//! `tests/store/terms.rs::should_apply_nothing_when_the_policy_is_in_safe_mode`).

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use mayfly_control_domain::host::{ApplyPoolTerms, Host, HostCommand};
use mayfly_control_domain::pool::{Pool, PoolTerms, terms};
use mayfly_control_domain::{Actor, Command, Rejection};
use mayfly_domain::{CommandId, Epoch, HostId, Millis, PoolName};

use crate::delivery::Kicks;
use crate::ids::derive;
use crate::journal::{ExpectedRevision, Journal};
use crate::projection::ProjectionStore;
use crate::projection::views::{self, HostRow};
use crate::queries::{self, QueryError};
use crate::store::{
    AggregateStore, Clock, Outcome, Rng, SaveError, SaveRequest, StoreNotifier, stream_of,
};

/// The policy's name: the actor of its commands.
pub const TERMS_POLICY: &str = "terms_policy";

const ID_DOMAIN: &[u8] = b"mayfly-terms-policy-v1\n";

/// The policy's configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermsConfig {
    /// The run epoch its commands are issued under.
    pub run_epoch: Epoch,
    /// The most time between two passes without a kick.
    pub interval: Millis,
}

impl TermsConfig {
    /// A one-minute interval under `run_epoch`.
    #[must_use]
    pub const fn new(run_epoch: Epoch) -> Self {
        Self {
            run_epoch,
            interval: Millis::new(60_000),
        }
    }
}

/// One pool's terms due on one host: the host has applied an older revision, or none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermsDue {
    /// The pool.
    pub pool: PoolName,
    /// The host.
    pub host_id: HostId,
    /// The pool's terms now.
    pub terms: PoolTerms,
}

/// The terms due: for every pool with terms (configured), every host in `applied` whose applied
/// revision for it is lower than the pool's, or absent. Pure; pools and hosts in name order.
/// Pinned by `tests/store/terms.rs::should_apply_every_bump_on_every_host_when_the_pool_changes`.
#[must_use]
pub fn due(
    pools: &BTreeMap<PoolName, PoolTerms>,
    applied: &BTreeMap<HostId, BTreeMap<PoolName, u64>>,
) -> Vec<TermsDue> {
    let mut out = Vec::new();
    for (pool, terms) in pools {
        for (host_id, held) in applied {
            if held.get(pool).is_some_and(|rev| *rev >= terms.revision) {
                continue;
            }
            out.push(TermsDue {
                pool: pool.clone(),
                host_id: host_id.clone(),
                terms: *terms,
            });
        }
    }
    out
}

/// The revisions each registered host has applied, by host, from the `hosts` rows.
fn applied_of(hosts: Vec<(String, HostRow)>) -> BTreeMap<HostId, BTreeMap<PoolName, u64>> {
    hosts
        .into_iter()
        .filter_map(|(key, row)| {
            let host: HostId = key.strip_prefix("host-")?.parse().ok()?;
            let held = row
                .terms
                .into_iter()
                .map(|t| (t.pool, t.revision))
                .collect();
            Some((host, held))
        })
        .collect()
}

/// Why a pass did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermsError {
    /// The read side could not be read.
    Query(QueryError),
}

impl std::fmt::Display for TermsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Query(e) => write!(f, "terms pass not run: {e}"),
        }
    }
}

impl std::error::Error for TermsError {}

/// What one pass did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TermsReport {
    /// The terms due.
    pub due: Vec<TermsDue>,
    /// Applied (also from a receipt), with the terms saved (newer than the pass first read when
    /// the pool moved meanwhile).
    pub accepted: Vec<TermsDue>,
    /// Refused by `Host`, with the refusal's code.
    pub refused: Vec<(TermsDue, &'static str)>,
    /// Not decided now (a conflict, a store fault): the next pass reads again.
    pub failed: Vec<(TermsDue, &'static str)>,
    /// Pools whose stream could not be read: the next pass reads again.
    pub unread: Vec<PoolName>,
    /// Due but not applied because the policy is in safe mode.
    pub held: Vec<TermsDue>,
    /// The pass ran in safe mode: it applied nothing, and a pool it could not read (a stored
    /// kind this build does not decode, the usual way into safe mode mid-run) leaves it
    /// complete.
    pub safe_mode: bool,
}

impl TermsReport {
    /// Whether the pass decided everything it had to: in safe mode always (it applies nothing,
    /// and a pool stream this build cannot read is what put the run there); otherwise every pool
    /// was read and every item due was applied or refused by `Host`, nothing failed. The ready
    /// line waits for a complete pass. Pinned by
    /// `tests/store/terms.rs::should_complete_in_safe_mode_when_a_pool_stream_holds_an_unknown_kind`.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.safe_mode || (self.failed.is_empty() && self.unread.is_empty())
    }
}

#[derive(Debug, Default)]
struct Timing {
    last_pass: Option<Millis>,
    safe_mode: bool,
}

/// The terms policy over an [`AggregateStore`] and the [`ProjectionStore`] it reads.
pub struct TermsPolicy<'s, J, C, R, N, P> {
    store: &'s AggregateStore<J, C, R, N>,
    projections: &'s P,
    config: TermsConfig,
    kicks: Kicks,
    timing: Mutex<Timing>,
}

impl<J, C, R, N, P> std::fmt::Debug for TermsPolicy<'_, J, C, R, N, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TermsPolicy")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl<'s, J: Journal, C: Clock, R: Rng, N: StoreNotifier, P: ProjectionStore>
    TermsPolicy<'s, J, C, R, N, P>
{
    /// A policy over `store`, reading `projections`, woken by `kicks` (the composition root makes
    /// them one of the store's notifiers).
    pub fn new(
        store: &'s AggregateStore<J, C, R, N>,
        projections: &'s P,
        config: TermsConfig,
        kicks: Kicks,
    ) -> Self {
        Self {
            store,
            projections,
            config,
            kicks,
            timing: Mutex::new(Timing::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Timing> {
        self.timing.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Enters or leaves safe mode (I29): a pass then applies nothing.
    pub fn set_safe_mode(&self, safe_mode: bool) {
        self.lock().safe_mode = safe_mode;
    }

    /// When the timer next makes a pass due: at once before the first pass, else one interval
    /// after the last.
    #[must_use]
    pub fn next_due(&self) -> Millis {
        self.lock().last_pass.map_or(Millis::new(0), |last| {
            Millis::new(last.get().saturating_add(self.config.interval.get()))
        })
    }

    /// The time from now until [`Self::next_due`] by the control plane's clock (0 when due).
    #[must_use]
    pub fn until_due(&self) -> Millis {
        Millis::new(
            self.next_due()
                .get()
                .saturating_sub(self.store.clock().now().get()),
        )
    }

    /// Runs a pass when one is due: a pool or host stream was kicked since the last look, or the
    /// interval passed. `None` when nothing was due.
    ///
    /// # Errors
    /// [`TermsError`] when the read side cannot be read.
    pub async fn tick(&self) -> Result<Option<TermsReport>, TermsError> {
        let kicked = self.kicks.take().iter().any(|stream| {
            let s = stream.as_str();
            s.starts_with("pool-") || s.starts_with("host-")
        });
        if !kicked && self.store.clock().now() < self.next_due() {
            return Ok(None);
        }
        self.run_pass().await.map(Some)
    }

    /// Reads, compares and applies one pass now.
    ///
    /// # Errors
    /// [`TermsError`] when the read side cannot be read; nothing is applied then.
    pub async fn run_pass(&self) -> Result<TermsReport, TermsError> {
        let now = self.store.clock().now();
        let hosts = queries::all_rows::<views::Hosts, P>(self.projections)
            .await
            .map_err(TermsError::Query)?;
        let pool_keys = queries::all_rows::<views::Pools, P>(self.projections)
            .await
            .map_err(TermsError::Query)?;
        let safe_mode = self.lock().safe_mode;
        let mut report = TermsReport {
            safe_mode,
            ..TermsReport::default()
        };
        let mut pools = BTreeMap::new();
        for (key, _) in pool_keys {
            let Some(pool) = key
                .strip_prefix("pool-")
                .and_then(|p| p.parse::<PoolName>().ok())
            else {
                continue;
            };
            match self.store.load_with_head::<Pool>(pool.as_str()).await {
                Ok((state, _)) => {
                    if let Some(t) = terms(&state) {
                        pools.insert(pool, t);
                    }
                }
                Err(_) => report.unread.push(pool),
            }
        }
        report.due = due(&pools, &applied_of(hosts));
        for item in report.due.clone() {
            if safe_mode {
                report.held.push(item);
                continue;
            }
            match self.apply(&item).await {
                Answer::Accepted(terms) => report.accepted.push(TermsDue { terms, ..item }),
                Answer::Refused(terms, code) => {
                    report.refused.push((TermsDue { terms, ..item }, code));
                }
                Answer::Failed(code) => report.failed.push((item, code)),
            }
        }
        self.lock().last_pass = Some(now);
        Ok(report)
    }

    /// `ApplyPoolTerms` of `item`'s pool on its host, under the store's revision retry: each try
    /// reads the pool's terms and head afresh and saves on the host stream with a read of the
    /// pool stream at that head. A conflict on either stream (the host's moves with every
    /// reservation and report, the pool's with every pool event) is retried with the store's
    /// backoff, and a pool that moved is applied at its new revision, never at the one the pass
    /// first read (pinned by
    /// `tests/store/terms.rs::should_apply_the_terms_in_the_same_pass_when_the_host_moves_between_the_read_and_the_save`
    /// and `should_apply_nothing_superseded_when_the_pool_moves_between_the_read_and_the_save`).
    async fn apply(&self, item: &TermsDue) -> Answer {
        let saved = self
            .store
            .with_revision_retry(|| async move {
                let pool_stream =
                    stream_of::<Pool>(item.pool.as_str()).map_err(SaveError::InvalidName)?;
                let (state, head) = self
                    .store
                    .load_with_head::<Pool>(item.pool.as_str())
                    .await?;
                let Some(t) = terms(&state) else {
                    return Ok(None);
                };
                let id = CommandId::from_u128(derive(
                    ID_DOMAIN,
                    &[
                        item.pool.as_str().as_bytes(),
                        &t.revision.to_be_bytes(),
                        item.host_id.as_str().as_bytes(),
                    ],
                ));
                let command = Command::new(
                    id,
                    self.store.clock().now(),
                    self.config.run_epoch,
                    Actor::Automation {
                        policy: TERMS_POLICY,
                    },
                    HostCommand::ApplyPoolTerms(ApplyPoolTerms {
                        pool: item.pool.clone(),
                        revision: t.revision,
                        open: t.open,
                        max_running: t.max_running,
                        tier: t.tier,
                    }),
                );
                let request = SaveRequest::new(item.host_id.as_str(), command)
                    .with_read(pool_stream, ExpectedRevision::of(head));
                Ok(Some((t, self.store.save::<Host>(request).await?)))
            })
            .await;
        match saved {
            Ok(Some((t, Outcome::Accepted { .. }))) => Answer::Accepted(t),
            Ok(Some((t, Outcome::Refused { rejection, .. }))) => {
                Answer::Refused(t, rejection.code())
            }
            Ok(None) => Answer::Failed("not_configured"),
            Err(SaveError::CommandIdReused { .. }) => Answer::Failed("command_id_reused"),
            Err(error) if error.is_retryable() => Answer::Failed("conflict"),
            Err(_) => Answer::Failed("save_failed"),
        }
    }
}

enum Answer {
    Accepted(PoolTerms),
    Refused(PoolTerms, &'static str),
    Failed(&'static str),
}
