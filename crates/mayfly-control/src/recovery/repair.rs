//! The application of an approved recovery plan (P20, `mayfly recover --apply <plan-id>`): each
//! action as intent, the attempt command, then the outcome, the plan bound throughout (I31,
//! ADR 0041; `formal/recovery.qnt`'s `planIntentWith`, `planActWith`, `planOutcomeWith`,
//! `refusePlan`, `completePlan`).
//!
//! The two halves read the journal differently, as the CP-04 card (corrected by KER-10G) states:
//!
//! - **The plan's own units** (`BeginAction`, `CompletePlan`) carry the bound streams as they
//!   are **now** ([`Bindings::read_now`], so the set is exactly the bound one) and are appended
//!   with those **current** revisions as their read conditions
//!   ([`HeadsRead::read_conditions`]), inside their own `with_revision_retry` closure: a stream
//!   that moves after the read refuses the unit and the retry reads it again, and the aggregate,
//!   comparing the current heads with the bound ones, records `RecoveryPlanRefused` when they
//!   differ. Conditioning these units on the bound revisions would refuse forever the very unit
//!   that must record the refusal (pinned by
//!   `tests/store/repair.rs::should_record_the_plan_refused_when_a_bound_stream_moved_before_the_action`).
//! - **An action's attempt command** is appended with the **bound** revisions
//!   ([`Bindings::read_conditions`]): every other bound stream as a read, the attempt's own as
//!   the revision its stream must be at ([`SaveRequest::with_expected`]). It is never retried:
//!   a stream that moved since the plan last recorded it refuses the command at commit, it never
//!   runs on a moved world, and the next `BeginAction` records the plan refused (the model's
//!   `planActWith`; pinned by
//!   `should_refuse_the_attempt_command_and_then_the_plan_when_another_writer_moves_a_bound_stream`).
//! - **The outcome** carries the attempt unit's own write as its receipt gives it (stream,
//!   revision before and after), never a recomputed value. An application resumed with an
//!   action's intent open re-runs its attempt command, answered by the same receipt when it
//!   committed, rather than recording a new intent that would read the plan's own write as a
//!   moved head (pinned by
//!   `should_complete_the_plan_when_an_application_resumes_after_its_outcome_failed`).
//!
//! A command under another run epoch than the plan's refuses the plan (`StaleBinding::Epoch`,
//! pinned by `should_refuse_the_plan_when_it_is_applied_under_a_new_run_epoch`). A completed plan
//! leaves safe mode with the plan's epoch and a verification taken after the completion; the
//! run then restarts before recovery completes ([`Applied::Completed`]).
//!
//! An action whose kind has no attempt command (`report`, `ignore`) cannot record a write, so a
//! plan holding one stops at it ([`Applied::NoCommand`]): the plan's drawer lists such names in
//! the plan's view, not as actions. An attempt command accepted with no event (nothing to do)
//! is recorded as failed: the plan stops, and a new one is drawn against the world as it is.

use std::collections::BTreeMap;

use mayfly_control_domain::attempt::{
    Attempt, AttemptCommand, AttemptRejection, Fact, Observation as AttemptObservation,
};
use mayfly_control_domain::operations::hostrun::{HostRun, HostRunCommand, SafeModeExit};
use mayfly_control_domain::operations::plan::{
    ActionKind, ActionOutcome, Bindings, GithubBinding, HeadsRead, Observed, OwnWrite, PlanAction,
    PlanPhase, RecoveryPlan, RecoveryPlanCommand, RecoveryPlanState, StaleBinding, attempt_stream,
};
use mayfly_control_domain::{Actor, Aggregate as _, Command};
use mayfly_domain::{CommandId, Epoch, EventId, HostId, ObservationId, PlanId, StateDigest};

use super::{InventorySeen, StructureCheck, inventory_ref, verify};
use crate::ids::derive;
use crate::journal::{
    ExpectedRevision, Journal, ReadCondition, StorageError, StreamId, StreamRevision,
};
use crate::store::{AggregateStore, Clock, Outcome, Rng, SaveError, SaveRequest, StoreNotifier};

/// P20's name: the actor of the plan's units.
pub const REPAIR: &str = "recovery_plan";

const ID_DOMAIN: &[u8] = b"mayfly-recovery-plan-apply-v1\n";

/// What the application observes besides the journal: the complete inventory's digest and
/// GitHub's runner listing (or that it is unreachable) now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct World {
    /// The digest of the complete inventory now.
    pub inventory: StateDigest,
    /// GitHub's runner listing now.
    pub github: GithubBinding,
}

/// How an application ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// Every action applied and the plan completed; safe mode was left with its epoch, and the
    /// daemon restarts before recovery completes.
    Completed {
        /// The plan's `RecoveryPlanCompleted`.
        completed_ref: EventId,
    },
    /// The world moved: the plan is refused and a new one is drawn.
    Refused(StaleBinding),
    /// An action's command was refused: the plan stopped there, resumable.
    Failed {
        /// The action.
        index: u32,
    },
    /// The action at `index` has no attempt command (`report`, `ignore`).
    NoCommand {
        /// The action.
        index: u32,
    },
    /// The plan is not approved, or is expired.
    NotApplicable,
}

/// Why an application stopped without an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairError {
    /// The store failed or refused a unit outside the plan's rules.
    Store(SaveError),
    /// A stream the plan bound has no event: it cannot have been read.
    UnboundStream(String),
    /// A plan unit was refused, by its code.
    Refused(&'static str),
}

impl From<SaveError> for RepairError {
    fn from(e: SaveError) -> Self {
        Self::Store(e)
    }
}

impl std::fmt::Display for RepairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "the store failed: {e}"),
            Self::UnboundStream(s) => write!(f, "the bound stream {s} has no event"),
            Self::Refused(code) => write!(f, "a plan unit was refused: {code}"),
        }
    }
}

impl std::error::Error for RepairError {}

/// The application of plans on one host under the run's epoch.
pub struct Repair<'s, J, C, R, N> {
    store: &'s AggregateStore<J, C, R, N>,
    host: HostId,
    run_epoch: Epoch,
}

impl<J, C, R, N> std::fmt::Debug for Repair<'_, J, C, R, N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repair")
            .field("host", &self.host)
            .field("run_epoch", &self.run_epoch)
            .finish_non_exhaustive()
    }
}

impl<'s, J: Journal, C: Clock, R: Rng, N: StoreNotifier> Repair<'s, J, C, R, N> {
    /// Applications on `host` over `store`, every unit issued under `run_epoch`.
    pub const fn new(
        store: &'s AggregateStore<J, C, R, N>,
        host: HostId,
        run_epoch: Epoch,
    ) -> Self {
        Self {
            store,
            host,
            run_epoch,
        }
    }

    /// Applies plan `plan_id` from where it stands (see the module docs); `world` is what the
    /// application observes of the inventory and GitHub, and `structure` verifies the store
    /// after the plan completes.
    ///
    /// # Errors
    /// [`RepairError`].
    pub async fn apply<S: StructureCheck>(
        &self,
        plan_id: PlanId,
        world: World,
        structure: &S,
    ) -> Result<Applied, RepairError> {
        let instance = plan_id.to_string();
        // a conflict refuses the attempt command; the next intent then reads the heads afresh
        let mut conflicted = false;
        loop {
            let plan = self.store.load::<RecoveryPlan>(&instance).await?;
            match plan.phase {
                PlanPhase::Approved | PlanPhase::Applying | PlanPhase::Failed => {}
                PlanPhase::Refused(stale) => return Ok(Applied::Refused(stale)),
                PlanPhase::Completed => return self.leave(plan_id, &plan, structure).await,
                PlanPhase::Unproposed | PlanPhase::Proposed | PlanPhase::Expired => {
                    return Ok(Applied::NotApplicable);
                }
            }
            let Some(bindings) = plan.bindings.clone() else {
                return Ok(Applied::Refused(StaleBinding::JournalHeads));
            };
            let index = u32::try_from(plan.next).unwrap_or(u32::MAX);
            let Some(action) = plan.actions.get(plan.next).cloned() else {
                self.plan_unit(&instance, &bindings, world, b"complete", index, |current| {
                    RecoveryPlanCommand::CompletePlan { current }
                })
                .await?;
                continue;
            };
            // A resumed application whose intent is open re-runs the attempt command, answered by
            // its receipt if it committed: a new intent would read the plan's own write as a moved
            // head and refuse the plan.
            if !plan.intent_open || conflicted {
                self.plan_unit(&instance, &bindings, world, b"begin", index, |current| {
                    RecoveryPlanCommand::BeginAction { index, current }
                })
                .await?;
                conflicted = false;
                let plan = self.store.load::<RecoveryPlan>(&instance).await?;
                if !plan.intent_open {
                    continue;
                }
            }
            let Some(body) = self.command_of(&plan_id, index, &action, &bindings) else {
                return Ok(Applied::NoCommand { index });
            };
            let outcome = match self.act(&plan_id, index, &action, &bindings, body).await {
                Err(SaveError::Conflict { .. }) => {
                    conflicted = true;
                    continue;
                }
                other => other?,
            };
            let (outcome, write) = match outcome {
                Outcome::Accepted { appended, .. } => {
                    let own = attempt_stream(action.attempt_id);
                    let bound = bindings.heads.get(&own).copied();
                    match (
                        appended.streams.iter().find(|s| s.stream.as_str() == own),
                        bound,
                    ) {
                        (Some(written), Some(before)) => (
                            ActionOutcome::Applied,
                            Some(OwnWrite {
                                stream: own,
                                before,
                                after: written.last.get(),
                            }),
                        ),
                        _ => (ActionOutcome::Failed, None),
                    }
                }
                Outcome::Refused { .. } => (ActionOutcome::Failed, None),
            };
            self.save_plan(
                &instance,
                self.id(&plan_id, &[b"outcome", &index.to_be_bytes()]),
                RecoveryPlanCommand::RecordOutcome {
                    index,
                    outcome,
                    write,
                },
                Vec::new(),
            )
            .await?;
            if outcome == ActionOutcome::Failed {
                return Ok(Applied::Failed { index });
            }
        }
    }

    /// A plan-stream unit carrying the bound streams as read now, conditioned on those current
    /// revisions, read again on each retry.
    async fn plan_unit(
        &self,
        instance: &str,
        bindings: &Bindings,
        world: World,
        step: &[u8],
        index: u32,
        body: impl Fn(Observed) -> RecoveryPlanCommand,
    ) -> Result<(), RepairError> {
        let plan_key = instance.as_bytes();
        let outcome = self
            .store
            .with_revision_retry(|| async {
                let heads = self.read_heads(bindings).await?;
                let current: HeadsRead = bindings.read_now(|s| heads.get(s).copied().unwrap_or(0));
                let reads = conditions(current.read_conditions())?;
                let digest: Vec<u8> = current
                    .read_conditions()
                    .iter()
                    .flat_map(|(s, r)| [s.as_bytes(), &r.to_be_bytes()].concat())
                    .collect();
                let id = CommandId::from_u128(derive(
                    ID_DOMAIN,
                    &[plan_key, step, &index.to_be_bytes(), &digest],
                ));
                let mut request = SaveRequest::new(
                    instance,
                    self.command(
                        id,
                        body(Observed {
                            heads: current,
                            inventory: world.inventory,
                            github: world.github,
                        }),
                    ),
                );
                request.reads = reads;
                self.store.save::<RecoveryPlan>(request).await
            })
            .await?;
        match outcome {
            Outcome::Accepted { .. } => Ok(()),
            Outcome::Refused { rejection, .. } => Err(RepairError::Refused(
                mayfly_control_domain::Rejection::code(&rejection),
            )),
        }
    }

    async fn read_heads(&self, bindings: &Bindings) -> Result<BTreeMap<String, u64>, SaveError> {
        let mut heads = BTreeMap::new();
        for stream in bindings.heads.keys() {
            let id = StreamId::new(stream).map_err(SaveError::InvalidName)?;
            let head = self
                .store
                .journal()
                .head(&id)
                .await
                .map_err(SaveError::Storage)?
                .ok_or_else(|| {
                    SaveError::Storage(StorageError::new("bound_stream_without_event"))
                })?;
            heads.insert(stream.clone(), head.revision.get());
        }
        Ok(heads)
    }

    /// The attempt command of `action`: an adoption (also for `keep`, which adopts a
    /// quarantined attempt under the run without changing its case), the complete listing's
    /// absence for `mark_lost`, or the quarantine; `None` for `report` and `ignore`.
    fn command_of(
        &self,
        plan_id: &PlanId,
        index: u32,
        action: &PlanAction,
        bindings: &Bindings,
    ) -> Option<AttemptCommand> {
        match action.kind {
            ActionKind::Adopt | ActionKind::Keep => Some(AttemptCommand::Adopt {
                inventory_ref: inventory_ref(&self.host, InventorySeen::Digest(bindings.inventory)),
            }),
            ActionKind::MarkLost => Some(AttemptCommand::RecordObservation(AttemptObservation {
                observation_id: ObservationId::from_u128(
                    self.id(plan_id, &[b"lost", &index.to_be_bytes()]).as_u128(),
                ),
                epoch: self.run_epoch,
                generation: mayfly_domain::Generation::FIRST,
                fact: Fact::VmAbsent,
            })),
            ActionKind::Quarantine(reason) => Some(AttemptCommand::Quarantine { reason }),
            ActionKind::Report | ActionKind::Ignore => None,
        }
    }

    /// The attempt command, conditioned on the bound revisions: every other bound stream as a
    /// read, the attempt's own as its expected revision. Never retried. A quarantine is preceded
    /// by the attempt's adoption under the run (the attempt takes no other command under a newer
    /// epoch, `not_adopted`): two units on its stream, the second expecting it where the first
    /// left it, so the plan's own write runs from the bound revision to the second's; an attempt
    /// already adopted under the run refuses the adoption (`already_current`) and is quarantined
    /// from the bound revision. Pinned by
    /// `tests/store/repair.rs::should_mark_lost_and_quarantine_through_ordinary_commands_when_the_plan_says_so`.
    async fn act(
        &self,
        plan_id: &PlanId,
        index: u32,
        action: &PlanAction,
        bindings: &Bindings,
        body: AttemptCommand,
    ) -> Result<Outcome<AttemptRejection>, SaveError> {
        let own = attempt_stream(action.attempt_id);
        let mut expected = bindings.heads.get(&own).copied();
        let mut body = body;
        if let AttemptCommand::RecordObservation(observation) = &mut body {
            let state = self
                .store
                .load::<Attempt>(&action.attempt_id.to_string())
                .await?;
            if let Some(identity) = state.identity {
                observation.generation = identity.generation;
            }
        }
        if matches!(body, AttemptCommand::Quarantine { .. }) {
            let adopt = AttemptCommand::Adopt {
                inventory_ref: inventory_ref(&self.host, InventorySeen::Digest(bindings.inventory)),
            };
            if let Some(rejection) = self
                .refused_after_adoption(plan_id, index, action, &adopt, &body)
                .await?
            {
                return Ok(Outcome::Refused {
                    rejection,
                    replayed: false,
                });
            }
            match self
                .act_once(
                    plan_id,
                    index,
                    action,
                    bindings,
                    (b"adopt", adopt, expected),
                )
                .await?
            {
                Outcome::Accepted { appended, .. } => {
                    if let Some(written) =
                        appended.streams.iter().find(|s| s.stream.as_str() == own)
                    {
                        expected = Some(written.last.get());
                    }
                }
                Outcome::Refused {
                    rejection: AttemptRejection::AlreadyCurrent,
                    ..
                } => {}
                refused @ Outcome::Refused { .. } => return Ok(refused),
            }
        }
        self.act_once(plan_id, index, action, bindings, (b"act", body, expected))
            .await
    }

    /// Whether the quarantine would be refused once the attempt is adopted (a directive in
    /// flight, `directive_pending`), decided purely on the state the adoption would leave, so a
    /// refusal leaves the attempt as it was rather than adopted under a failed plan. Not asked
    /// once the quarantine has a receipt: a resumed application is answered by it. Pinned by
    /// `tests/store/repair.rs::should_leave_the_attempt_unadopted_when_its_quarantine_would_be_refused`.
    async fn refused_after_adoption(
        &self,
        plan_id: &PlanId,
        index: u32,
        action: &PlanAction,
        adopt: &AttemptCommand,
        quarantine: &AttemptCommand,
    ) -> Result<Option<AttemptRejection>, SaveError> {
        let act_id = self.id(plan_id, &[b"act", &index.to_be_bytes()]);
        if self
            .store
            .journal()
            .receipt(act_id)
            .await
            .map_err(SaveError::Storage)?
            .is_some()
        {
            return Ok(None);
        }
        let state = self
            .store
            .load::<Attempt>(&action.attempt_id.to_string())
            .await?;
        let adopt_id = self.id(plan_id, &[b"adopt", &index.to_be_bytes()]);
        let adopted = match Attempt::decide(&state, self.command(adopt_id, adopt.clone())) {
            Ok(events) => events.iter().fold(state.clone(), Attempt::evolve),
            Err(_) => state,
        };
        Ok(Attempt::decide(&adopted, self.command(act_id, quarantine.clone())).err())
    }

    /// One unit of an action on its attempt: every bound stream but its own as a read at its
    /// bound revision, its own expected at `own_at`.
    async fn act_once(
        &self,
        plan_id: &PlanId,
        index: u32,
        action: &PlanAction,
        bindings: &Bindings,
        (step, body, own_at): (&[u8], AttemptCommand, Option<u64>),
    ) -> Result<Outcome<AttemptRejection>, SaveError> {
        let own = attempt_stream(action.attempt_id);
        let mut request = SaveRequest::new(
            action.attempt_id,
            self.command(self.id(plan_id, &[step, &index.to_be_bytes()]), body),
        );
        if let Some(revision) = own_at {
            request =
                request.with_expected(ExpectedRevision::Exactly(StreamRevision::new(revision)));
        }
        let mut reads = Vec::new();
        for (stream, revision) in bindings.read_conditions() {
            if stream != own {
                reads.push(ReadCondition {
                    stream: StreamId::new(&stream).map_err(SaveError::InvalidName)?,
                    expected: ExpectedRevision::Exactly(StreamRevision::new(revision)),
                });
            }
        }
        request.reads = reads;
        self.store.save::<Attempt>(request).await
    }

    /// Safe mode left by the completed plan, with a verification taken now, after it.
    async fn leave<S: StructureCheck>(
        &self,
        plan_id: PlanId,
        plan: &RecoveryPlanState,
        structure: &S,
    ) -> Result<Applied, RepairError> {
        let instance = plan_id.to_string();
        let stream =
            crate::store::stream_of::<RecoveryPlan>(&instance).map_err(SaveError::InvalidName)?;
        let completed_ref = self
            .store
            .journal()
            .read_stream(&stream, None, None)
            .await
            .map_err(SaveError::Storage)?
            .last()
            .map(|e| e.event_id);
        let verification = verify(self.store.journal(), structure)
            .await
            .map_err(SaveError::Storage)?;
        let plan_epoch = plan.bindings.as_ref().map_or(plan.epoch, |b| b.epoch);
        let command = self.command(
            self.id(&plan_id, &[b"leave"]),
            HostRunCommand::LeaveSafeMode {
                exit: SafeModeExit::PlanCompleted {
                    plan_id,
                    completed_ref,
                    plan_epoch,
                    verification,
                },
            },
        );
        match self
            .store
            .execute::<HostRun>(SaveRequest::new(self.host.as_str(), command))
            .await?
        {
            Outcome::Accepted { .. } => {}
            Outcome::Refused { rejection, .. } => {
                return Err(RepairError::Refused(
                    mayfly_control_domain::Rejection::code(&rejection),
                ));
            }
        }
        Ok(Applied::Completed {
            completed_ref: completed_ref.unwrap_or(EventId::from_u128(0)),
        })
    }

    async fn save_plan(
        &self,
        instance: &str,
        id: CommandId,
        body: RecoveryPlanCommand,
        reads: Vec<ReadCondition>,
    ) -> Result<(), RepairError> {
        let mut request = SaveRequest::new(instance, self.command(id, body));
        request.reads = reads;
        match self
            .store
            .with_revision_retry(|| self.store.save::<RecoveryPlan>(request.clone()))
            .await?
        {
            Outcome::Accepted { .. } => Ok(()),
            Outcome::Refused { rejection, .. } => Err(RepairError::Refused(
                mayfly_control_domain::Rejection::code(&rejection),
            )),
        }
    }

    fn command<B>(&self, id: CommandId, body: B) -> Command<B> {
        Command::new(
            id,
            self.store.clock().now(),
            self.run_epoch,
            Actor::Automation { policy: REPAIR },
            body,
        )
    }

    fn id(&self, plan_id: &PlanId, parts: &[&[u8]]) -> CommandId {
        let plan = plan_id.to_string();
        let mut all: Vec<&[u8]> = vec![plan.as_bytes()];
        all.extend_from_slice(parts);
        CommandId::from_u128(derive(ID_DOMAIN, &all))
    }
}

fn conditions(read: Vec<(String, u64)>) -> Result<Vec<ReadCondition>, SaveError> {
    read.into_iter()
        .map(|(stream, revision)| {
            Ok(ReadCondition {
                stream: StreamId::new(&stream).map_err(SaveError::InvalidName)?,
                expected: ExpectedRevision::Exactly(StreamRevision::new(revision)),
            })
        })
        .collect()
}
