//! The reaper's listing of every runner (policy P14, ADR 0029): at run start, after a long
//! GitHub outage, daily and on `mayfly doctor`, at most one listing per
//! [`RegistrationConfig::listing_interval`](super::RegistrationConfig::listing_interval).
//!
//! A listed runner is judged by the journal alone, never by its name:
//!
//! - its id is journaled by a registration whose attempt's VM is gone (retiring, deleting,
//!   deleted, lost, closed, or no attempt at all): `RequestDeregistration{NotLive}`, and the
//!   manager removes it by that id;
//! - its id is journaled and its VM lives: left to the manager (P11, P13);
//! - its id is journaled by no registration and its name has mayfly's shape (`mf-…`): reported
//!   (`unrecognised_runner_reported` at WARN, and in [`SweepReport::unrecognised`]), never
//!   removed (I19: only journaled ids are removed); a name an unknown issue waits for is P10's;
//! - any other name, or another scale set's runner: left alone.

use std::collections::{BTreeMap, BTreeSet};

use mayfly_control_domain::attempt::{Attempt, AttemptState, Phase as AttemptPhase};
use mayfly_control_domain::registration::{
    DeregistrationReason, Phase, RegistrationCommand, RunnerId, RunnerRegistration, ScaleSetId,
};
use mayfly_domain::{AttemptId, Deadline, Millis, VmNameClass, classify_vm_name};

use super::ScaleSetDirectory;
use super::manager::{RegistrationManager, holds_runner};
use crate::github::{Failure, RunnerRegistry, ScaleSetApi};
use crate::journal::{Journal, StreamRevision};
use crate::store::{Clock, Outcome, Rng, StoreNotifier};

/// What started a sweep (dm P14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepTrigger {
    /// The run started.
    RunStarted,
    /// A session opened again after a long GitHub outage.
    AfterOutage,
    /// The daily timer.
    Daily,
    /// `mayfly doctor`.
    Doctor,
}

/// A runner of mayfly's name shape that no registration journaled: reported, never removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnrecognisedRunner {
    /// Its id.
    pub id: RunnerId,
    /// Its name, as GitHub listed it (bounded by the adapter).
    pub name: String,
    /// Its scale set, when the listing said.
    pub scale_set: Option<ScaleSetId>,
}

/// What one sweep found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepReport {
    /// What started it.
    pub trigger: SweepTrigger,
    /// When its listing was taken.
    pub at: Millis,
    /// The runners listed.
    pub listed: usize,
    /// Journaled runners whose VM is gone, removed by their ids.
    pub deregistered: Vec<RunnerId>,
    /// Journaled runners whose VM is gone, whose removal GitHub did not settle yet (busy, or
    /// no answer): the manager asks again.
    pub pending: Vec<(RunnerId, &'static str)>,
    /// Runners of mayfly's name shape that no registration journaled.
    pub unrecognised: Vec<UnrecognisedRunner>,
    /// Runners named for an unknown issue, left to its listing by name (P10).
    pub awaiting_issue_unknown: usize,
    /// Runners of another name shape, or of a scale set mayfly does not serve.
    pub foreign: usize,
}

/// Why a sweep did not list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepError {
    /// The last listing is younger than the listing interval: the next is allowed at `next`.
    BudgetSpent {
        /// When the next listing is allowed.
        next: Millis,
    },
    /// GitHub did not answer the listing; it counts against the budget all the same.
    GitHub(Failure),
    /// The journal could not be read.
    Journal(&'static str),
}

/// Whether the attempt's VM is gone, or going, so that its runner cannot be live in it.
const fn vm_gone(a: &AttemptState) -> bool {
    a.identity.is_none()
        || matches!(
            a.phase,
            AttemptPhase::Retiring
                | AttemptPhase::Deleting
                | AttemptPhase::Deleted
                | AttemptPhase::Lost
                | AttemptPhase::Closed
        )
}

impl<J, C, R, N, A, G, D> RegistrationManager<'_, J, C, R, N, A, G, D>
where
    J: Journal,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
    A: ScaleSetApi,
    G: RunnerRegistry,
    D: ScaleSetDirectory,
{
    /// P14: lists every runner once (within the budget) and judges each by the journal (module
    /// docs). Pinned by
    /// `tests/store/registration.rs::should_remove_only_journaled_ids_and_report_the_rest_when_the_reaper_sweeps`
    /// and `should_list_at_most_once_an_hour_when_sweeps_follow_each_other`.
    ///
    /// # Errors
    /// [`SweepError`]: nothing was judged.
    pub async fn sweep(&self, trigger: SweepTrigger) -> Result<SweepReport, SweepError> {
        let now = self.store.clock().now();
        {
            if let Some(last) = self.local().last_listing {
                let next = Deadline::after(last, self.config.listing_interval);
                if !next.is_expired(now) {
                    return Err(SweepError::BudgetSpent {
                        next: next.instant(),
                    });
                }
            }
        }
        let attempts = self
            .registration_attempts()
            .await
            .map_err(SweepError::Journal)?;
        let mut journaled: BTreeMap<RunnerId, AttemptId> = BTreeMap::new();
        let mut awaited: BTreeSet<String> = BTreeSet::new();
        let mut ours = self.directory.scale_sets();
        for attempt in attempts {
            let r = self
                .store
                .load::<RunnerRegistration>(&attempt.to_string())
                .await
                .map_err(|_| SweepError::Journal("registration_unreadable"))?;
            ours.extend(r.scale_set);
            if let Some(id) = r.runner_id {
                journaled.insert(id, attempt);
            }
            if r.phase == Phase::IssueUnknown
                && let Some(name) = &r.runner_name
            {
                awaited.insert(name.to_string());
            }
        }
        // the budget is spent by the call to GitHub, answered or not, never by a journal fault
        self.local().last_listing = Some(now);
        let listed = self
            .registry
            .list_runners()
            .await
            .map_err(|e| SweepError::GitHub(e.failure))?;
        let mut report = SweepReport {
            trigger,
            at: now,
            listed: listed.len(),
            deregistered: Vec::new(),
            pending: Vec::new(),
            unrecognised: Vec::new(),
            awaiting_issue_unknown: 0,
            foreign: 0,
        };
        for runner in listed {
            if runner.scale_set.is_some_and(|s| !ours.contains(&s)) {
                report.foreign += 1;
                continue;
            }
            if let Some(attempt) = journaled.get(&runner.id).copied() {
                match self.retire_not_live(attempt).await {
                    Ok(Some(true)) => report.deregistered.push(runner.id),
                    Ok(Some(false)) => report.pending.push((runner.id, "not_removed_yet")),
                    Ok(None) => {}
                    Err(code) => report.pending.push((runner.id, code)),
                }
                continue;
            }
            match classify_vm_name(&runner.name) {
                VmNameClass::Foreign => report.foreign += 1,
                VmNameClass::WellFormed(name) if awaited.contains(&name.to_string()) => {
                    report.awaiting_issue_unknown += 1;
                }
                VmNameClass::WellFormed(..) | VmNameClass::MalformedManaged => {
                    tracing::warn!(
                        runner_id = runner.id.0,
                        runner_name = runner.name.as_str(),
                        "unrecognised_runner_reported"
                    );
                    report.unrecognised.push(UnrecognisedRunner {
                        id: runner.id,
                        name: runner.name,
                        scale_set: runner.scale_set,
                    });
                }
            }
        }
        self.local().last_sweep = Some(report.clone());
        Ok(report)
    }

    /// The last sweep's report, for `doctor` and status.
    #[must_use]
    pub fn last_sweep(&self) -> Option<SweepReport> {
        self.local().last_sweep.clone()
    }

    /// When the last listing of every runner was asked of GitHub (answered or not): what P14's
    /// hourly budget counts from. A daemon keeps it across its runs ([`Self::seed_listing`]).
    #[must_use]
    pub fn last_listing(&self) -> Option<Millis> {
        self.local().last_listing
    }

    /// Starts the budget from `at`, the last listing a previous manager of this process asked
    /// for, so a new run (a link that flapped) does not list again within the hour; a later
    /// listing already known here is kept. Pinned by
    /// `tests/store/registration.rs::should_list_at_most_once_an_hour_when_a_new_manager_is_seeded`.
    pub fn seed_listing(&self, at: Option<Millis>) {
        let mut local = self.local();
        local.last_listing = local.last_listing.max(at);
    }

    /// Asks for the removal of `attempt`'s journaled runner when its VM is gone, and lets the
    /// manager make it: `None` when the runner is live (or already being removed), else whether
    /// it is deregistered now.
    async fn retire_not_live(&self, attempt: AttemptId) -> Result<Option<bool>, &'static str> {
        let instance = attempt.to_string();
        let a = self
            .store
            .load::<Attempt>(&instance)
            .await
            .map_err(|_| "attempt_unreadable")?;
        let (r, r_head) = self
            .store
            .load_with_head::<RunnerRegistration>(&instance)
            .await
            .map_err(|_| "registration_unreadable")?;
        if !holds_runner(&r) || !vm_gone(&a) {
            return Ok(None);
        }
        if r.deregistration.is_none() {
            let Some(epoch) = self.epoch(&a) else {
                return Err("no_session");
            };
            let saved = self
                .registration(
                    attempt,
                    epoch,
                    "not_live",
                    r_head.map(StreamRevision::get),
                    RegistrationCommand::RequestDeregistration {
                        reason: DeregistrationReason::NotLive,
                    },
                )
                .await;
            match saved {
                Ok(Outcome::Accepted { .. } | Outcome::Refused { .. }) => {}
                Err(_) => return Err("save_failed"),
            }
        }
        self.step(attempt).await?;
        let after = self
            .store
            .load::<RunnerRegistration>(&instance)
            .await
            .map_err(|_| "registration_unreadable")?;
        Ok(Some(after.phase == Phase::Deregistered))
    }
}
