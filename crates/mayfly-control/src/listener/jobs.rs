//! [`JobFacts`]: policy P12's first half (`doc/domain-model.md` §7), the job facts a recorded
//! message carries turned into commands on the runner's attempt and registration, as a durable
//! consumer of the `scaleset-*` streams' `MessageRecorded` events (so a fact recorded before a
//! crash is still applied: the event is delivered until handled, ES-I15).
//!
//! - **`JobStarted`** names the runner: its registration is found by the runner's name (the
//!   attempt's VM name, which the adapter keeps only when it is one of mayfly's) or else by the
//!   runner's id, among the registrations of the stream's scale set; then
//!   `RecordConsumed{request_id}` on the registration and `BindJob{request_id}` on the attempt
//!   (Listening → Busy, I14).
//! - **`JobCompleted`** names the runner's id: `RecordConsumed` (a `JobStarted` missed or not
//!   recorded yet) and `RecordCompleted{result}`.
//! - A runner no registration of the scale set names is reported (`unrecognised`, WARN
//!   `job_fact_runner_unrecognised`), never acted on; a refusal (a job already bound, a
//!   registration already consumed by another job, a terminal one) is the aggregate's answer and
//!   is reported, never retried, and never blocks the stream. A runner can take its job before its
//!   attempt journaled `RunnerLaunched` (still `Activating`): `BindJob` is refused
//!   `not_listening`, the registration is consumed all the same, and the registration manager
//!   binds the job once the attempt listens (its level-triggered step: a consumed registration
//!   whose listening attempt is unbound), so no other runner's facts wait on that attempt.
//!   Pinned by
//!   `tests/store/registration.rs::should_apply_a_second_runners_fact_and_bind_the_first_once_it_listens_when_its_launch_lags`.
//!
//! Limitations: a `JobCompleted` without a `runnerId` cannot be matched and is reported as
//! unrecognised, so its registration completes only through GH-12's probe; and the scale set is
//! read as it is when the event is handled, so an event recorded before the set was re-created
//! matches no registration of the new set (those runners went with the old set).
//!
//! P12's second half, `RequestRetirement{job_done}` once the runner exited, is the registration
//! manager's (`RegistrationManager`: a finished attempt whose registration completed is retired,
//! from the attempt's and the registration's own events). Command ids derive from the event and
//! the fact's position in its batch, not from `runnerRequestId` (0 on github.com, never a key:
//! `doc/github-scaleset.md` §4.3), so a redelivered event is answered by its receipts. Pinned by
//! `tests/store/registration.rs::should_bind_complete_and_retire_the_attempt_when_its_runner_takes_a_job`.
//!
//! The registrations are found by reading every `registration-*` stream, as the registration
//! manager's pass does; RT-07's note asks for a projection of the open ones before production.

use mayfly_control_domain::attempt::{Attempt, AttemptCommand};
use mayfly_control_domain::registration::{
    RegistrationCommand, RegistrationState, RunnerRegistration,
};
use mayfly_control_domain::scaleset::{JobFact, ScaleSet, ScaleSetEvent};
use mayfly_control_domain::{Actor, Aggregate, Command, Rejection};
use mayfly_domain::{AttemptId, CommandId, Epoch, EventId};

use crate::delivery::{Consumer, ConsumerError, Delivery};
use crate::github::{RunnerId, ScaleSetId};
use crate::ids::derive;
use crate::journal::{Journal, MAX_PAGE, RecordedEvent, StreamId};
use crate::store::{
    AggregateStore, Clock, Outcome, Rng, SaveError, SaveRequest, StoreNotifier, decode_event,
};

/// The consumer's name, and the actor of its commands.
pub const JOB_FACTS: &str = "job_facts";

const ID_DOMAIN: &[u8] = b"mayfly-job-facts-v1\n";

/// What one event's job facts did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobFactsReport {
    /// Commands accepted (or answered by their receipts).
    pub accepted: u32,
    /// Commands refused, by code.
    pub refused: Vec<&'static str>,
    /// Facts whose runner no registration of the scale set names.
    pub unrecognised: u32,
}

/// Why an event's job facts were not all applied: the event is delivered again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobFactsError {
    /// The store could not read or commit.
    Store(SaveError),
}

impl std::fmt::Display for JobFactsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "store: {e}"),
        }
    }
}

impl std::error::Error for JobFactsError {}

impl From<SaveError> for JobFactsError {
    fn from(error: SaveError) -> Self {
        Self::Store(error)
    }
}

/// One registration of the scale set, as the facts are matched against it.
struct Known {
    attempt: AttemptId,
    state: RegistrationState,
}

/// P12's first half over `store`, issuing under `run_epoch`.
pub struct JobFacts<'s, J, C, R, N> {
    store: &'s AggregateStore<J, C, R, N>,
    run_epoch: Epoch,
}

impl<J, C, R, N> std::fmt::Debug for JobFacts<'_, J, C, R, N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobFacts")
            .field("run_epoch", &self.run_epoch)
            .finish_non_exhaustive()
    }
}

impl<'s, J: Journal, C: Clock, R: Rng, N: StoreNotifier> JobFacts<'s, J, C, R, N> {
    /// The consumer over `store`, issuing under `run_epoch`.
    pub const fn new(store: &'s AggregateStore<J, C, R, N>, run_epoch: Epoch) -> Self {
        Self { store, run_epoch }
    }

    /// Applies the job facts of `event`, when it is a scale set's `MessageRecorded`; any other
    /// event does nothing.
    ///
    /// # Errors
    /// [`JobFactsError`] when the journal cannot be read or a command was not committed: the
    /// event is delivered again, and the commands already committed answer from their receipts.
    pub async fn apply(&self, event: &RecordedEvent) -> Result<JobFactsReport, JobFactsError> {
        let mut report = JobFactsReport::default();
        let Some(pool) = event.stream.as_str().strip_prefix("scaleset-") else {
            return Ok(report);
        };
        let decoded = decode_event::<ScaleSetEvent>(event).map_err(|_| SaveError::Codec {
            stream: event.stream.clone(),
            what: "event_payload",
        })?;
        let ScaleSetEvent::MessageRecorded(recorded) = decoded else {
            return Ok(report);
        };
        let runner_facts = recorded
            .job_facts
            .iter()
            .any(|f| !matches!(f, JobFact::Assigned { .. }));
        if !runner_facts {
            return Ok(report);
        }
        let Some(set) = self.store.load::<ScaleSet>(pool).await?.github_id else {
            return Ok(report);
        };
        let known = self.registrations(set).await?;
        for (n, fact) in (0_u32..).zip(&recorded.job_facts) {
            match fact {
                JobFact::Assigned { .. } => {}
                JobFact::Started {
                    request_id,
                    runner_id,
                    runner_name,
                    ..
                } => {
                    let by_name = runner_name.as_ref().and_then(|name| {
                        known
                            .iter()
                            .find(|k| k.state.runner_name.as_ref() == Some(name))
                    });
                    let Some(k) = by_name.or_else(|| by_id(&known, *runner_id)) else {
                        unrecognised(&mut report, *runner_id);
                        continue;
                    };
                    let consumed = RegistrationCommand::RecordConsumed {
                        request_id: *request_id,
                    };
                    let id = command_id(event.event_id, n, b"consumed");
                    let saved = self
                        .save::<RunnerRegistration>(k.attempt, id, consumed)
                        .await;
                    tally(&mut report, saved)?;
                    let bind = AttemptCommand::BindJob {
                        request_id: *request_id,
                    };
                    let id = command_id(event.event_id, n, b"bind");
                    let saved = self.save::<Attempt>(k.attempt, id, bind).await;
                    tally(&mut report, saved)?;
                }
                JobFact::Completed {
                    request_id,
                    result,
                    runner_id,
                    ..
                } => {
                    let Some(k) = by_id(&known, *runner_id) else {
                        unrecognised(&mut report, *runner_id);
                        continue;
                    };
                    let consumed = RegistrationCommand::RecordConsumed {
                        request_id: *request_id,
                    };
                    let id = command_id(event.event_id, n, b"consumed");
                    let saved = self
                        .save::<RunnerRegistration>(k.attempt, id, consumed)
                        .await;
                    tally(&mut report, saved)?;
                    let completed = RegistrationCommand::RecordCompleted { result: *result };
                    let id = command_id(event.event_id, n, b"completed");
                    let saved = self
                        .save::<RunnerRegistration>(k.attempt, id, completed)
                        .await;
                    tally(&mut report, saved)?;
                }
            }
        }
        Ok(report)
    }

    async fn save<A: Aggregate>(
        &self,
        attempt: AttemptId,
        id: CommandId,
        body: A::Body,
    ) -> Result<Outcome<A::Rejection>, SaveError>
    where
        A::Body: Clone,
    {
        let command = Command::new(
            id,
            self.store.clock().now(),
            self.run_epoch,
            Actor::Automation { policy: JOB_FACTS },
            body,
        );
        self.store
            .execute::<A>(SaveRequest::new(attempt, command))
            .await
    }

    /// Every registration of scale set `set`, read from the `registration-*` streams.
    async fn registrations(&self, set: ScaleSetId) -> Result<Vec<Known>, SaveError> {
        const PREFIX: &str = "registration-";
        let mut after = StreamId::new("registration").map_err(SaveError::InvalidName)?;
        let mut known = Vec::new();
        loop {
            let page = self
                .store
                .journal()
                .streams(Some(&after), MAX_PAGE)
                .await
                .map_err(SaveError::Storage)?;
            let Some(last) = page.last() else {
                return Ok(known);
            };
            for head in &page {
                let stream = head.stream.as_str();
                let Some(id) = stream.strip_prefix(PREFIX) else {
                    if stream > PREFIX {
                        return Ok(known);
                    }
                    continue;
                };
                let Ok(attempt) = id.parse::<AttemptId>() else {
                    continue;
                };
                let state = self.store.load::<RunnerRegistration>(id).await?;
                if state.scale_set == Some(set) {
                    known.push(Known { attempt, state });
                }
            }
            after = last.stream.clone();
        }
    }
}

fn by_id(known: &[Known], runner: Option<RunnerId>) -> Option<&Known> {
    let runner = runner?;
    known.iter().find(|k| k.state.runner_id == Some(runner))
}

fn unrecognised(report: &mut JobFactsReport, runner: Option<RunnerId>) {
    report.unrecognised = report.unrecognised.saturating_add(1);
    tracing::warn!(
        runner_id = runner.map_or(0, |r| r.0),
        "job_fact_runner_unrecognised"
    );
}

fn tally<R: Rejection>(
    report: &mut JobFactsReport,
    saved: Result<Outcome<R>, SaveError>,
) -> Result<(), SaveError> {
    match saved? {
        Outcome::Accepted { .. } => report.accepted = report.accepted.saturating_add(1),
        Outcome::Refused { rejection, .. } => report.refused.push(rejection.code()),
    }
    Ok(())
}

fn command_id(event: EventId, n: u32, tag: &[u8]) -> CommandId {
    CommandId::from_u128(derive(
        ID_DOMAIN,
        &[&event.as_u128().to_be_bytes(), &n.to_be_bytes(), tag],
    ))
}

impl<J: Journal, C: Clock, R: Rng, N: StoreNotifier> Consumer for JobFacts<'_, J, C, R, N> {
    const NAME: &'static str = JOB_FACTS;

    /// Applies a `scaleset-*` event's job facts; events of other streams pass. A store failure
    /// answers an error, so the event comes again after a backoff.
    async fn handle(&self, delivery: Delivery<'_>) -> Result<(), ConsumerError> {
        self.apply(delivery.event).await.map(|_| ()).map_err(|e| {
            ConsumerError::new(match e {
                JobFactsError::Store(_) => "job_facts_not_applied",
            })
        })
    }
}
