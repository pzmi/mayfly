//! The lifecycle driver (ver §6.2 item 3): `formal/lifecycle.qnt` against the `Attempt`
//! aggregate, every stimulus a command saved through the control plane's one write path,
//! [`AggregateStore::save`], over the in-memory journal, and every observation read back from
//! the journal (the stream folded with the aggregate's `evolve`), never from the model.
//!
//! # What is compared
//!
//! [`expected`] projects the model's attempt record `a`; [`LifecycleDriver::observe`] projects
//! the folded `AttemptState`. The fields, by name:
//!
//! | Field | Model (`a`) | Implementation (`AttemptState`) |
//! |:--|:--|:--|
//! | `phase` | `phase` | `phase`'s variant (`Backoff`, `Quarantined` without their data) |
//! | `stage` | `stage` while `Backoff`, else `-` | the `Backoff` stage, else `-` |
//! | `tries` | `tries` | `tries` |
//! | `pending` | `pending` | the pending directive's tag, or `none` |
//! | `unknown` | `unknown` | the pending outcome is `Unknown` |
//! | `name` | `name` | `identity.slot` (the driver starts slot `name`) |
//! | `generation` | `generation` | `identity.generation` |
//! | `instance` | `instance` (0: none) | `instance_id`, mapped back to the model's number |
//! | `vm_running` | `vmRunning` | `vm_running` |
//! | `epoch` | `epoch` | `epoch` |
//! | `granted` | `granted` | `grant_ref` is set |
//! | `fresh_verification` | `freshEpoch == epoch` | `fresh_verification` |
//! | `jit` | `jit` | `jit`'s variant |
//! | `job` | `job` (0: none) | `job_binding` |
//! | `retire_requested` | `retireRequested` | `retirement_requested` is set |
//! | `diagnostics` | `diagnostics` | `diagnostics` |
//! | `runner_exited` | `runnerExited` | `runner_exited` |
//! | `vanished` | `vanished` and `phase == Quarantined` | quarantined `vm_vanished_externally` |
//! | `refused` | `-`: every model step is an accepted transition | the last command's refusal code, else `-` |
//!
//! # What is abstracted
//!
//! - The ghost `g` (history for the invariants) and the provider's world `w` are not compared:
//!   `w` is the environment, read only where [`LifecycleDriver::MODEL_READS`] declares it (the
//!   instance id a clone got and the inventory's listing), and the invariants are checked by
//!   Quint while the trace is made.
//! - `jitLeaked` has no field in the aggregate: a launch of unknown outcome is `jit = taken`,
//!   an orphaned handle `jit = orphaned`, and the consequence (no job is ever bound) shows as a
//!   refusal only if the model offers `bindJob`, which it never does then.
//! - `freshEpoch` is compared as "fresh in the current epoch"; the aggregate keeps no number.
//! - `vanished` is compared only while quarantined: the model keeps the flag after an
//!   operator's resolution, the aggregate keeps the reason only inside `Quarantined`.
//! - Ids, deadlines, refusal and quarantine reasons, the retirement reason, guest facts and
//!   diagnostics digests are minted by the driver; the model has none of them.
//! - Rust-only rows (`NotStarted`, stale observations, the run-epoch fence of I18, actor
//!   checks other than `ResolveQuarantine`) are never driven: the model starts at `Reserved` and
//!   every command is issued in the driver's current run epoch.
//!
//! # Stimuli
//!
//! The saga's `IssueNext` is issued by the driver itself: the attempt saga (CP-02) does not
//! exist yet, so the trace drives the aggregate directly, and the saga's own choices (when to
//! issue) are the model's. Observations carry the driver's current run epoch, as answers
//! re-delivered in the session after an adoption would. `prepareAtOnce` is the sequence of
//! stimuli it stands for, compared once at its end.
//!
//! The aggregate is a type parameter: the negative controls swap in a wrapper that answers one
//! row differently (`tests/lifecycle.rs`).

use std::marker::PhantomData;

use mayfly_control::journal::{Journal, StreamId};
use mayfly_control::store::{
    AggregateStore, NoNotifier, Outcome, SaveError, SaveRequest, decode_event, stream_of,
};
use mayfly_control_domain::attempt::{
    AttemptCommand, AttemptEvent, AttemptState, Diagnostics, DirectiveTag, Fact, GuestFacts,
    GuestOs, GuestRejection, Jit, Observation as Seen, Outcome as PendingOutcome, Phase,
    QuarantineReason, RefusalReason, RetireReason, RunnerRequestId, Stage, StartAttempt,
};
use mayfly_control_domain::{Actor, Aggregate, Command, ControlAggregates, Rejection};
use mayfly_domain::{
    AttemptId, CommandId, Deadline, DirectiveId, Epoch, EventId, Generation, HostId, ImageDigest,
    Millis, ObservationId, PoolName, ProviderInstanceId, Sample, SecretHandleId, Slot,
};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::journal::MemoryJournal;

use crate::itf::State;
use crate::replay::{DriverError, ModelView, Observation, Step};

/// The model's retry budget (`BUDGET`), given to `StartAttempt`.
pub const RETRY_BUDGET: u32 = 2;

/// The model's "no value" (`NONE`).
const NONE: i128 = 0;

/// The instance id the provider gives the model's instance `n`.
#[must_use]
pub fn instance_id(n: i128) -> ProviderInstanceId {
    ProviderInstanceId::new(&format!("0a:00:00:00:00:{n:02x}"))
        .unwrap_or_else(|_| ProviderInstanceId::sample())
}

/// The model's number of an instance id [`instance_id`] made, or the id itself.
fn instance_number(id: &ProviderInstanceId) -> String {
    id.as_str()
        .strip_prefix("0a:00:00:00:00:")
        .and_then(|hex| i128::from_str_radix(hex, 16).ok())
        .map_or_else(|| format!("?{id}"), |n| n.to_string())
}

/// The aggregates a lifecycle driver can run: `Attempt`, or a wrapper with its types.
pub trait AttemptLike:
    Aggregate<State = AttemptState, Body = AttemptCommand, Event = AttemptEvent>
{
}

impl<A> AttemptLike for A where
    A: Aggregate<State = AttemptState, Body = AttemptCommand, Event = AttemptEvent>
{
}

/// The canonical observation of a model state (the table in the module docs).
///
/// # Errors
/// [`DriverError::BadRead`] when `a` lacks a field.
pub fn expected(state: &State) -> Result<Observation, DriverError> {
    let a = state
        .vars
        .get("a")
        .ok_or_else(|| DriverError::BadRead("no variable a".to_owned()))?;
    let get = |name: &str| {
        a.field(name)
            .ok_or_else(|| DriverError::BadRead(format!("no field a.{name}")))
    };
    let int = |name: &str| {
        get(name)?
            .as_int()
            .ok_or_else(|| DriverError::BadRead(format!("a.{name} is not an integer")))
    };
    let flag = |name: &str| {
        get(name)?
            .as_bool()
            .ok_or_else(|| DriverError::BadRead(format!("a.{name} is not a boolean")))
    };
    let text = |name: &str| {
        get(name)?
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| DriverError::BadRead(format!("a.{name} is not a string")))
    };
    let phase = text("phase")?;
    let none_or = |n: i128| {
        if n == NONE {
            "-".to_owned()
        } else {
            n.to_string()
        }
    };
    let mut o = Observation::default();
    o.set(
        "stage",
        if phase == "Backoff" {
            text("stage")?
        } else {
            "-".to_owned()
        },
    );
    o.set("vanished", flag("vanished")? && phase == "Quarantined");
    o.set("phase", phase);
    o.set("tries", int("tries")?);
    o.set("pending", text("pending")?);
    o.set("unknown", flag("unknown")?);
    o.set("name", int("name")?);
    o.set("generation", int("generation")?);
    o.set("instance", none_or(int("instance")?));
    o.set("vm_running", flag("vmRunning")?);
    o.set("epoch", int("epoch")?);
    o.set("granted", flag("granted")?);
    o.set("fresh_verification", int("freshEpoch")? == int("epoch")?);
    o.set("jit", text("jit")?);
    o.set("job", none_or(int("job")?));
    o.set("retire_requested", flag("retireRequested")?);
    o.set("diagnostics", text("diagnostics")?);
    o.set("runner_exited", flag("runnerExited")?);
    o.set("refused", "-");
    Ok(o)
}

/// The canonical observation of a folded attempt (the table in the module docs).
#[must_use]
pub fn observe_state(state: &AttemptState, refused: Option<&str>) -> Observation {
    let mut o = Observation::default();
    let (phase, stage) = match state.phase {
        Phase::NotStarted => ("NotStarted", "-"),
        Phase::Reserved => ("Reserved", "-"),
        Phase::Cloning => ("Cloning", "-"),
        Phase::Booting => ("Booting", "-"),
        Phase::Verifying => ("Verifying", "-"),
        Phase::Prepared => ("Prepared", "-"),
        Phase::Activating => ("Activating", "-"),
        Phase::Listening => ("Listening", "-"),
        Phase::Busy => ("Busy", "-"),
        Phase::Finished => ("Finished", "-"),
        Phase::Retiring => ("Retiring", "-"),
        Phase::Deleting => ("Deleting", "-"),
        Phase::Deleted => ("Deleted", "-"),
        Phase::Lost => ("Lost", "-"),
        Phase::Backoff { stage, .. } => (
            "Backoff",
            match stage {
                Stage::Clone => "clone",
                Stage::Boot => "boot",
                Stage::Verify => "verify",
                Stage::Activate => "activate",
            },
        ),
        Phase::Quarantined { .. } => ("Quarantined", "-"),
        Phase::Closed => ("Closed", "-"),
    };
    o.set("phase", phase);
    o.set("stage", stage);
    o.set("tries", state.tries);
    o.set(
        "pending",
        state.pending.map_or("none", |p| match p.tag {
            DirectiveTag::CloneVm => "CloneVm",
            DirectiveTag::StartVm => "StartVm",
            DirectiveTag::VerifyGuest => "VerifyGuest",
            DirectiveTag::LaunchRunner => "LaunchRunner",
            DirectiveTag::ExportDiagnostics => "ExportDiagnostics",
            DirectiveTag::StopVm => "StopVm",
            DirectiveTag::DeleteVm => "DeleteVm",
        }),
    );
    o.set(
        "unknown",
        state
            .pending
            .is_some_and(|p| matches!(p.outcome, PendingOutcome::Unknown { .. })),
    );
    let (name, generation) = state.identity.as_ref().map_or_else(
        || ("-".to_owned(), "-".to_owned()),
        |i| (i.slot.get().to_string(), i.generation.get().to_string()),
    );
    o.set("name", name);
    o.set("generation", generation);
    o.set(
        "instance",
        state
            .instance_id
            .as_ref()
            .map_or_else(|| "-".to_owned(), instance_number),
    );
    o.set("vm_running", state.vm_running);
    o.set("epoch", state.epoch.get());
    o.set("granted", state.grant_ref.is_some());
    o.set("fresh_verification", state.fresh_verification);
    o.set(
        "jit",
        match state.jit {
            Jit::None => "none",
            Jit::Issued(..) => "issued",
            Jit::Taken(..) => "taken",
            Jit::Orphaned(..) => "orphaned",
        },
    );
    o.set(
        "job",
        state
            .job_binding
            .map_or_else(|| "-".to_owned(), |RunnerRequestId(n)| n.to_string()),
    );
    o.set("retire_requested", state.retirement_requested.is_some());
    o.set(
        "diagnostics",
        match state.diagnostics {
            Diagnostics::NotTaken => "none",
            Diagnostics::Exported => "exported",
            Diagnostics::Lost => "lost",
        },
    );
    o.set("runner_exited", state.runner_exited);
    o.set(
        "vanished",
        matches!(
            state.phase,
            Phase::Quarantined {
                reason: QuarantineReason::VmVanishedExternally,
                ..
            }
        ),
    );
    o.set("refused", refused.unwrap_or("-"));
    o
}

type Store = AggregateStore<MemoryJournal, FakeClock, SeededRng, NoNotifier>;

/// The policy the driver's automation commands are recorded under.
const SAGA: Actor = Actor::Automation {
    policy: "attempt_saga",
};
const POLICY: Actor = Actor::Automation {
    policy: "conformance_driver",
};
const OPERATOR: Actor = Actor::Operator { uid: 501 };

/// Drives one attempt stream of aggregate `A` (normally `Attempt`) through `AggregateStore`
/// over a fresh `MemoryJournal`.
pub struct LifecycleDriver<A: AttemptLike> {
    store: Store,
    journal: MemoryJournal,
    stream: StreamId,
    instance: String,
    run_epoch: u64,
    next_id: u128,
    refused: Option<&'static str>,
    aggregate: PhantomData<A>,
}

impl<A: AttemptLike> std::fmt::Debug for LifecycleDriver<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LifecycleDriver")
            .field("stream", &self.stream)
            .field("run_epoch", &self.run_epoch)
            .finish_non_exhaustive()
    }
}

/// The attempt the driver starts (one stream per replay).
const ATTEMPT: u128 = 0xA77E;

impl<A: AttemptLike> LifecycleDriver<A> {
    /// A driver over an empty journal.
    ///
    /// # Errors
    /// [`DriverError::Harness`] when the store cannot be built.
    pub fn new() -> Result<Self, DriverError> {
        let journal = MemoryJournal::new();
        let store = AggregateStore::new::<ControlAggregates>(
            journal.clone(),
            FakeClock::at(Millis::new(1_700_000_000_000)),
            SeededRng::new(1),
            NoNotifier,
        )
        .map_err(|e| DriverError::Harness(e.to_string()))?;
        let instance = format!("{:032x}", ATTEMPT);
        let stream = stream_of::<A>(&instance).map_err(|e| DriverError::Harness(e.to_string()))?;
        Ok(Self {
            store,
            journal,
            stream,
            instance,
            run_epoch: 1,
            next_id: 0,
            refused: None,
            aggregate: PhantomData,
        })
    }

    fn mint(&mut self) -> u128 {
        self.next_id += 1;
        self.next_id
    }

    /// The attempt as the journal records it: its stream folded with `A::evolve`.
    ///
    /// # Errors
    /// [`DriverError::Harness`] when the stream cannot be read or decoded.
    pub fn state(&self) -> Result<AttemptState, DriverError> {
        let recorded = block_on(self.journal.read_stream(&self.stream, None, None))
            .map_err(|e| DriverError::Harness(e.to_string()))?;
        recorded.iter().try_fold(A::initial(), |state, event| {
            decode_event::<AttemptEvent>(event)
                .map(|e| A::evolve(state, &e))
                .map_err(|e| DriverError::Harness(format!("{e:?}")))
        })
    }

    /// Saves `body` as `actor` in the current run epoch; a refusal is recorded for the
    /// observation and stops a composite step.
    fn send(&mut self, actor: Actor, body: AttemptCommand) -> Result<bool, DriverError> {
        let id = self.mint();
        let command = Command::new(
            CommandId::from_u128(0xC000_0000 + id),
            Millis::new(1_700_000_000_000),
            Epoch::new(self.run_epoch),
            actor,
            body,
        );
        let outcome = block_on(
            self.store
                .save::<A>(SaveRequest::new(&self.instance, command)),
        )
        .map_err(|e: SaveError| DriverError::Harness(e.to_string()))?;
        match outcome {
            Outcome::Accepted { .. } => Ok(true),
            Outcome::Refused { rejection, .. } => {
                self.refused = Some(rejection.code());
                Ok(false)
            }
        }
    }

    fn observation(&mut self, fact: Fact) -> Result<bool, DriverError> {
        let generation = self
            .state()?
            .identity
            .map_or(Generation::FIRST, |i| i.generation);
        let id = self.mint();
        let body = AttemptCommand::RecordObservation(Seen {
            observation_id: ObservationId::from_u128(0x0B5_0000 + id),
            epoch: Epoch::new(self.run_epoch),
            generation,
            fact,
        });
        self.send(SAGA, body)
    }

    fn issue_next(&mut self) -> Result<bool, DriverError> {
        let id = self.mint();
        self.send(
            SAGA,
            AttemptCommand::IssueNext {
                directive_id: DirectiveId::from_u128(0xD000_0000 + id),
                deadline: Deadline::at(Millis::new(1_700_000_060_000)),
            },
        )
    }

    /// The pending directive's id and tag, as the aggregate records them.
    fn pending(&self) -> Result<(DirectiveId, DirectiveTag), DriverError> {
        self.state()?
            .pending
            .map(|p| (p.directive_id, p.tag))
            .ok_or_else(|| {
                DriverError::Harness(
                    "the model answers a directive the attempt does not have pending".to_owned(),
                )
            })
    }

    fn start(&mut self, step: &Step<'_>) -> Result<(), DriverError> {
        let name = pick_int(step, "name")?;
        let generation = pick_int(step, "generation")?;
        let generation = u32::try_from(generation)
            .ok()
            .and_then(Generation::new)
            .ok_or_else(|| DriverError::BadRead(format!("generation {generation}")))?;
        let slot = u32::try_from(name)
            .map(Slot::new)
            .map_err(|_| DriverError::BadRead(format!("name {name}")))?;
        self.send(
            SAGA,
            AttemptCommand::StartAttempt(StartAttempt {
                attempt_id: AttemptId::from_u128(ATTEMPT),
                host_id: HostId::sample(),
                slot,
                generation,
                pool: PoolName::sample(),
                image_digest: ImageDigest::sample(),
                retry_budget: RETRY_BUDGET,
            }),
        )?;
        Ok(())
    }

    fn reply_succeeded(&mut self, step: &Step<'_>) -> Result<(), DriverError> {
        let (directive_id, tag) = self.pending()?;
        let fact = match tag {
            DirectiveTag::CloneVm => Fact::VmCloned {
                directive_id,
                instance_id: instance_id(step.after.int("w.instance")?),
            },
            DirectiveTag::StartVm => Fact::VmBooted { directive_id },
            DirectiveTag::VerifyGuest => Fact::GuestVerified {
                directive_id,
                facts: guest_facts(pick_int(step, "claim")?),
            },
            DirectiveTag::LaunchRunner => Fact::RunnerLaunched { directive_id },
            DirectiveTag::ExportDiagnostics => Fact::DiagnosticsExported {
                directive_id,
                bytes: 512,
                sha256: [7; 32],
            },
            DirectiveTag::StopVm => Fact::VmStopped { directive_id },
            DirectiveTag::DeleteVm => Fact::VmDeleted { directive_id },
        };
        self.observation(fact)?;
        Ok(())
    }

    fn refusal(&mut self, reason: RefusalReason) -> Result<(), DriverError> {
        let (directive_id, _) = self.pending()?;
        self.observation(Fact::DirectiveRefused {
            directive_id,
            reason,
        })?;
        Ok(())
    }

    fn prepare_at_once(&mut self, step: &Step<'_>) -> Result<(), DriverError> {
        let instance = instance_id(pick_int(step, "instance")?);
        let activated = step
            .pick("activated")?
            .as_bool()
            .ok_or_else(|| DriverError::BadRead("activated is not a boolean".to_owned()))?;
        let claim = guest_facts(1);
        if !self.issue_next()? {
            return Ok(());
        }
        let (directive_id, _) = self.pending()?;
        if !self.observation(Fact::VmCloned {
            directive_id,
            instance_id: instance,
        })? || !self.issue_next()?
        {
            return Ok(());
        }
        let (directive_id, _) = self.pending()?;
        if !self.observation(Fact::VmBooted { directive_id })? || !self.issue_next()? {
            return Ok(());
        }
        let (directive_id, _) = self.pending()?;
        if !self.observation(Fact::GuestVerified {
            directive_id,
            facts: claim,
        })? || !activated
        {
            return Ok(());
        }
        let grant = self.mint();
        if !self.send(
            POLICY,
            AttemptCommand::BeginActivation {
                grant_ref: EventId::from_u128(0xE000_0000 + grant),
            },
        )? || !self.issue_next()?
        {
            return Ok(());
        }
        let (directive_id, _) = self.pending()?;
        self.observation(Fact::GuestVerified {
            directive_id,
            facts: claim,
        })?;
        Ok(())
    }
}

fn pick_int(step: &Step<'_>, name: &str) -> Result<i128, DriverError> {
    step.pick(name)?
        .as_int()
        .ok_or_else(|| DriverError::BadRead(format!("pick {name} is not an integer")))
}

/// The model action an `mbt::actionTaken` names: the scripts of `lifecycle_traces` start with
/// `tinit` and call the parameterised forms (`replySucceededWith`) that the model's relations
/// wrap (`replySucceeded`).
#[must_use]
pub fn canonical_action(taken: &str) -> &str {
    if taken == "tinit" {
        return "init";
    }
    taken.strip_suffix("With").unwrap_or(taken)
}

/// A divergence between the model and the aggregate that a replay found and that is left for
/// a follow-up task to resolve (VER-01 reports them; it changes neither side). A replay that
/// ends in one is reported `KNOWN`, not failed, and the rest of that trace is not compared; a
/// test replays its regression trace and requires exactly this divergence, so fixing either
/// side fails that test until the entry is removed
/// (`tests/lifecycle.rs::should_reproduce_each_known_divergence_when_its_regression_trace_is_replayed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownDivergence {
    /// A short id.
    pub id: &'static str,
    /// The model action (canonical) at the divergent step; `*` for any.
    pub action: &'static str,
    /// The fields it may show; a differing field outside them makes the divergence unknown.
    pub fields: &'static [&'static str],
    /// Fields the divergence must show, with their values `(field, expected, actual)`; `*`
    /// matches any value.
    pub pins: &'static [(&'static str, &'static str, &'static str)],
    /// The regression script of `lifecycle_traces` that reproduces it.
    pub regression: &'static str,
    /// What differs, and which side the follow-up has to decide.
    pub summary: &'static str,
}

/// The divergences VER-01's replays found (see [`KnownDivergence`]).
pub const KNOWN_DIVERGENCES: &[KnownDivergence] = &[
    KnownDivergence {
        id: "unknown-stop-resolved-by-listing-keeps-tries",
        action: "inventoryPresent",
        fields: &["tries"],
        pins: &[("tries", "*", "0")],
        regression: "divergenceUnknownStopResolvedByListing",
        summary: "a StopVm of unknown outcome resolved by a listing that shows the VM stopped: \
                  the model keeps the stop counted in `tries` (inventoryPresent), the aggregate \
                  records VmStopped and resets `tries` to 0, as a stop's success reply does in both",
    },
    KnownDivergence {
        id: "confirm-retirement-refused-outside-activating-or-listening",
        action: "confirmRetirement",
        fields: &["phase", "refused", "retire_requested", "stage", "tries"],
        pins: &[
            ("phase", "Retiring", "*"),
            ("refused", "-", "no_retirement_requested"),
        ],
        regression: "divergenceConfirmRetirementInBackoff",
        summary: "a retirement requested while activating survives into Backoff(activate) (a \
                  failed fresh verification) or Finished (the runner exited after a launch of \
                  unknown outcome); the model confirms it there (retire), the aggregate refuses \
                  ConfirmRetirement outside Activating and Listening (no_retirement_requested) \
                  while keeping `retirement_requested` set",
    },
    KnownDivergence {
        id: "clone-answered-after-removal-records-no-instance",
        action: "replySucceeded",
        fields: &["instance"],
        pins: &[("instance", "-", "0")],
        regression: "divergenceCloneAnsweredAfterRemoval",
        summary: "a CloneVm whose reply was lost (applied) and whose VM was then removed is \
                  answered by a re-delivery: the model's success reply records the world's \
                  instance NONE and reaches Booting with no instance id; a real VmCloned always \
                  carries an instance id, so the model allows a reply the data plane cannot give",
    },
    KnownDivergence {
        id: "requarantine-after-resolved-vanished-keeps-the-flag",
        action: "*",
        fields: &["vanished"],
        pins: &[("vanished", "true", "false")],
        regression: "divergenceRequarantineAfterVanished",
        summary: "the model never clears `vanished`: after an operator resolves a vanished \
                  quarantine, any later quarantine (a policy's, or the delete refused again and \
                  again) is read as vanished again, so a later absence would not release it, \
                  while the aggregate records the new reason",
    },
];

/// The known divergence `divergence` is, if any.
#[must_use]
pub fn known(divergence: &crate::replay::Divergence) -> Option<&'static KnownDivergence> {
    KNOWN_DIVERGENCES.iter().find(|k| {
        let value = |side: &Observation, field: &str| side.0.get(field).cloned();
        (k.action == "*" || k.action == canonical_action(&divergence.action))
            && divergence.fields.iter().all(|f| k.fields.contains(f))
            && k.pins.iter().all(|(field, expected, actual)| {
                let matches =
                    |want: &str, got: Option<String>| want == "*" || got.as_deref() == Some(want);
                divergence.fields.contains(field)
                    && matches(expected, value(&divergence.expected, field))
                    && matches(actual, value(&divergence.actual, field))
            })
    })
}

/// The guest's own claim `claim` as typed facts: read by no ownership field (I30).
fn guest_facts(claim: i128) -> GuestFacts {
    GuestFacts {
        os: GuestOs::Macos,
        uptime: Millis::new(u64::try_from(claim).unwrap_or(0).saturating_mul(1_000)),
    }
}

/// The world's listing of the VM before an inventory: environment, not implementation.
fn listed(view: &ModelView<'_>) -> Result<(ProviderInstanceId, bool), DriverError> {
    Ok((
        instance_id(view.int("w.instance")?),
        view.bool("w.running")?,
    ))
}

impl<A: AttemptLike> crate::replay::Driver for LifecycleDriver<A> {
    const MODEL: &'static str = "lifecycle";
    /// The provider's truth: the instance id a clone got (after a reply), and what a complete
    /// listing shows (before an inventory). Nothing of `a` or `g`.
    const MODEL_READS: &'static [&'static str] = &["w.instance", "w.running"];

    fn expected(state: &State) -> Result<Observation, DriverError> {
        expected(state)
    }

    fn apply(&mut self, step: &Step<'_>) -> Result<(), DriverError> {
        self.refused = None;
        let job = |step: &Step<'_>| -> Result<u64, DriverError> {
            u64::try_from(pick_int(step, "job")?)
                .map_err(|_| DriverError::BadRead("job is negative".to_owned()))
        };
        match canonical_action(step.action) {
            "init" => self.start(step)?,
            "issueNext" => {
                self.issue_next()?;
            }
            "replySucceeded" => self.reply_succeeded(step)?,
            "replyRefused" => self.refusal(RefusalReason::EffectFailed)?,
            "replyProofRefused" => self.refusal(RefusalReason::ProofMismatch)?,
            "replyTargetAbsent" => self.refusal(RefusalReason::TargetAbsent)?,
            "replyGuestRejected" => {
                let (directive_id, _) = self.pending()?;
                self.observation(Fact::GuestRejected {
                    directive_id,
                    reason: GuestRejection::ChallengeFailed,
                })?;
            }
            "replyLost" => {
                let (directive_id, _) = self.pending()?;
                self.observation(Fact::OutcomeUnknown { directive_id })?;
            }
            "replyLaunchUnknown" => {
                let (directive_id, _) = self.pending()?;
                self.observation(Fact::RunnerLaunchUnknown { directive_id })?;
            }
            "inventoryPresent" => {
                let (instance_id, running) = listed(&step.before()?)?;
                self.observation(Fact::VmPresent {
                    instance_id,
                    running,
                })?;
            }
            "inventoryAbsent" => {
                self.observation(Fact::VmAbsent)?;
            }
            "vmObservedStopped" => {
                self.observation(Fact::VmObservedStopped)?;
            }
            "runnerExited" => {
                self.observation(Fact::RunnerExited { code: 0 })?;
            }
            // the provider changes behind the journal's back: no stimulus
            "vmReplacedOrRemoved" => {}
            "beginActivation" => {
                let grant = self.mint();
                self.send(
                    POLICY,
                    AttemptCommand::BeginActivation {
                        grant_ref: EventId::from_u128(0xE000_0000 + grant),
                    },
                )?;
            }
            "recordJitHandle" => {
                let id = self.mint();
                self.send(
                    POLICY,
                    AttemptCommand::RecordJitHandle {
                        handle_id: SecretHandleId::from_u128(0x5EC0_0000 + id),
                        registration_ref: EventId::from_u128(0xE100_0000 + id),
                    },
                )?;
            }
            "bindJob" => {
                let request_id = RunnerRequestId(job(step)?);
                self.send(POLICY, AttemptCommand::BindJob { request_id })?;
            }
            "requestRetirement" => {
                self.send(
                    POLICY,
                    AttemptCommand::RequestRetirement {
                        reason: RetireReason::Surplus,
                    },
                )?;
            }
            "confirmRetirement" => {
                self.send(POLICY, AttemptCommand::ConfirmRetirement)?;
            }
            "withdrawRetirement" => {
                self.send(POLICY, AttemptCommand::WithdrawRetirement)?;
            }
            "quarantineCommand" => {
                self.send(
                    POLICY,
                    AttemptCommand::Quarantine {
                        reason: QuarantineReason::ForensicRetention,
                    },
                )?;
            }
            "adopt" => {
                self.run_epoch += 1;
                let id = self.mint();
                self.send(
                    Actor::Startup,
                    AttemptCommand::Adopt {
                        inventory_ref: EventId::from_u128(0xE200_0000 + id),
                    },
                )?;
            }
            "resolveQuarantine" => {
                self.send(OPERATOR, AttemptCommand::ResolveQuarantine)?;
            }
            "close" => {
                self.send(POLICY, AttemptCommand::Close)?;
            }
            "prepareAtOnce" => self.prepare_at_once(step)?,
            other => return Err(DriverError::UnknownAction(other.to_owned())),
        }
        Ok(())
    }

    fn observe(&self) -> Result<Observation, DriverError> {
        Ok(observe_state(&self.state()?, self.refused))
    }
}

/// Builds a fresh driver for `A` and replays `trace`: the entry point the replay binary and
/// the tests share.
///
/// # Errors
/// See [`crate::replay::replay`].
pub fn replay<A: AttemptLike>(
    entry: &crate::manifest::Entry,
    trace: &crate::itf::Trace,
) -> Result<crate::replay::Replayed, crate::replay::ReplayError> {
    let mut driver =
        LifecycleDriver::<A>::new().map_err(|error| crate::replay::ReplayError::Driver {
            trace: entry.id().to_owned(),
            step: 0,
            action: "init".to_owned(),
            error,
        })?;
    crate::replay::replay(&mut driver, entry, trace)
}
