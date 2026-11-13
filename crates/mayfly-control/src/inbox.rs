//! The observation inbox (policy P8, plan §3.2, `doc/event-store.md` §3.3): every observation
//! of one data plane becomes the units of work it maps to, each consuming an
//! [`InboxKey`] `{dp-<host>, <epoch>/<observation id>}`, and the caller acks the frame only once
//! [`Inbox::record`] returned `Ok`, so an observation is never acked before its record commits
//! (I12; pinned by `tests/store/inbox.rs::should_ack_only_after_the_record_commits_when_the_store_fails`).
//!
//! - **Idempotent by inbox key**, `<epoch>/<observation id>`: a data plane restarted under a
//!   new epoch may reuse an id, which is a new observation (pinned by
//!   `should_record_the_fact_when_an_observation_id_recurs_under_a_new_epoch`). A unit's command
//!   id is derived from the host, the epoch, the observation and the target, so a redelivery is answered by the first delivery's receipt, and a unit
//!   that lost its reply settles as it did; a key consumed by another unit is
//!   [`SaveError::DuplicateMessage`], read as already recorded (pinned by
//!   `should_record_once_when_an_observation_is_delivered_twice`).
//! - **A refusal never consumes the key** (ADR 0041 rule 6, JC-34): the store writes a refusal
//!   receipt without the key, the inbox reads the refusal as handled (the frame is acked), and a
//!   redelivery after the attempt moved is decided afresh rather than dropped (pinned by
//!   `should_decide_a_redelivery_afresh_when_its_refusal_consumed_no_key`).
//! - **Stale observations are recorded as ignored by the attempt itself**
//!   (`StaleObservationIgnored`): an older generation or epoch, or an answer to a directive that
//!   is no longer pending (pinned by
//!   `should_record_a_stale_answer_as_ignored_when_its_epoch_is_older`).
//! - **Answers** with an attempt go to that attempt as a [`Fact`]; `RunnerExited` and
//!   `VmObservedStopped` name only the VM and are routed by its exact `OwnedVmName` among the
//!   host's attempts (a name no attempt records is not routed: recovery reports it).
//! - **A complete inventory** becomes, per attempt the host leases ([`host_attempts`]), the facts
//!   [`inventory_facts`] reads from it (`VmPresent`, then `VmObservedStopped` for a VM stopped
//!   not by us; or `VmAbsent`), each its own unit keyed `<observation id>/<attempt>/<n>`; an
//!   incomplete one records nothing and is never read as an absence (I25). A listed VM whose
//!   instance id could not be read says nothing about that attempt.
//! - **A capacity report** becomes the `Host`'s `RecordCapacityReport`, with every dimension the
//!   report lists as unknown delivering nothing (ADR 0052's correction, DEC-04F): the share of
//!   memory or disk, the VM slots or the macOS-guest slots is zero, because admission reads
//!   totals, foreign and base use, never `free` (pinned by
//!   `should_admit_nothing_in_a_dimension_when_the_report_lists_it_unknown`).
//!
//! - **Answers to placement directives** (CP-07) go to the placement stream: `ImagePulled` and
//!   `ImageMaterialised` by their host and digest, `ImageEvicted`, a refusal and an unknown
//!   outcome by the directive, found in the [`PlacementIndex`] the placement manager keeps; an
//!   answer whose placement is not known is not routed (`unknown_placement`), and the
//!   manager's timeout makes its outcome `Unknown`, never a success. A complete inventory also
//!   resolves a placement directive of `Unknown` outcome, from what the data plane observed
//!   and never by a new directive (P7) or an asserted outcome (ADR 0025):
//!   - with the image store's listing beside it (protocol 1.5, DP-08), a pull whose image is
//!     listed (pulled and verified against its digest) completed (`RecordPulled`), and a
//!     materialisation whose base the listing shows (`Ready` and passing the clone proof, ADR
//!     0087, matched by name and image digest) completed (`RecordMaterialised`, with the base's
//!     measured disk), each only from a listing of a later run epoch than the session that sent
//!     the directive (`Pending::sent_epoch`). Observations carry no order on the link (plan
//!     §3.2; no sequence number, random observation ids), and what a listing shows can be
//!     removed after it was taken (a base by eviction; a cache entry by a later pull's failed
//!     check, which deletes the entry Tart linked it to, or by a change outside mayfly), so a
//!     listing of the send's own session may predate the effect, even show what an earlier
//!     directive made before it was removed. A listing of a later session comes after both the
//!     effect and any such removal: the session that sent the directive ended, and with it
//!     every effect it ran, finished or lost (a child that survives a killed data plane cannot
//!     write the registry's Ready or a verified image), and an eviction that removed an earlier
//!     base was resolved before this directive was issued (one directive per placement, none
//!     while one is pending). A directive sent before DP-08 recorded no epoch and is never
//!     resolved positively from a listing. A pull whose image a listing of any session omits
//!     did not apply (`RecordNotApplied`);
//!   - with or without it, a materialisation whose base no VM of the inventory bears did not
//!     apply (`RecordNotApplied`), and an eviction whose base no VM bears did (`RecordEvicted`);
//!   - anything else resolves nothing: a VM under the base's name that the listing does not
//!     show `Ready` may be half made or still being removed (`formal/image_store.qnt`,
//!     `materialisedOnlyFromReady`), and without the listing (a 1.4 session, or a store that
//!     could not be listed) a pull is resolved only by its late answer.
//!
//!   Pinned by `tests/store/placement.rs::should_resolve_an_unknown_materialisation_only_when_its_base_is_absent`,
//!   `should_resolve_an_unknown_pull_from_the_listing_when_its_image_is_listed`,
//!   `should_resolve_an_unknown_pull_as_not_applied_when_a_listing_omits_its_image`,
//!   `should_resolve_an_unknown_materialisation_when_the_listing_shows_its_base_ready` and
//!   `should_resolve_nothing_when_the_listed_base_is_not_ready`,
//!   `should_resolve_nothing_when_the_listing_is_of_the_sends_own_epoch` and
//!   `should_resolve_nothing_from_a_listing_when_the_directive_is_in_flight`. A listing taken
//!   before an effect ran and read after its outcome became unknown can still make a
//!   `RecordNotApplied` wrong; it costs one more directive, which the data plane's image store
//!   makes a no-op (a pull of a present image returns before any fetch, the materialisation of a
//!   `Ready` base succeeds doing nothing), and is the residual recorded on DP-08's card. An
//!   unknown materialisation whose effect completed within its own session is resolved only by
//!   its late answer or a listing of a later session.
//!
//! Host facts are not routed here (`Recorded::not_routed`): their aggregates' inboxes are later
//! tasks'. The report's own non-VM use (`ReportedCapacity::base`) is not separable from
//! the protocol's `used` and is recorded as zero: what does not fit is refused
//! `insufficient_resources` by the data plane's ledger (ADR 0024), which holds the host (CP-03);
//! CP-04B adds the field and a conservative interim.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use mayfly_control_domain::attempt::{
    Attempt, AttemptCommand, AttemptState, DiagnosticsLoss, Fact, GuestFacts, GuestOs,
    GuestRejection, Observation as AttemptObservation, Phase, RefusalReason,
};
use mayfly_control_domain::capacity::ReportedCapacity;
use mayfly_control_domain::host::{Host, HostCommand};
use mayfly_control_domain::image::base_name;
use mayfly_control_domain::placement::{
    ImagePlacement, PendingOutcome, PlacementCommand, PlacementDirective, placement_instance,
};
use mayfly_control_domain::{Actor, Aggregate, Command};
use mayfly_domain::{
    AttemptId, CommandId, DirectiveId, Epoch, Generation, HostId, ImageDigest, ObservationId,
    OwnedVmName,
};
use mayfly_protocol::{
    CapacityReport, ImageStoreListing, Observation, ObservationKind, ResourceDimension,
    VmObservation,
};

use crate::ids::derive;
use crate::journal::{InboxKey, InboxSource, Journal, JournalValueError, MessageId};
use crate::placement::PlacementIndex;
use crate::store::{AggregateStore, Clock, Outcome, Rng, SaveError, SaveRequest, StoreNotifier};

/// The inbox's name: the actor of its commands.
pub const INBOX: &str = "observation_inbox";

const ID_DOMAIN: &[u8] = b"mayfly-observation-inbox-v1\n";

/// The inbox source of `host`'s data plane: `dp-` and 32 hex digits derived from the host id, a
/// valid source label whatever characters the host id holds.
///
/// # Errors
/// Never in practice: the label is always `[a-z0-9-]`.
pub fn inbox_source(host: &HostId) -> Result<InboxSource, JournalValueError> {
    let label = format!(
        "dp-{:032x}",
        derive(ID_DOMAIN, &[b"source", host.as_str().as_bytes()])
    );
    InboxSource::new(&label)
}

/// What one observation was recorded as. Every count is of units decided now or answered by a
/// receipt; the frame is acked whatever the counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Recorded {
    /// Units accepted (or answered by their acceptance's receipt).
    pub accepted: u32,
    /// Units refused by their aggregate (handled; the key stays unconsumed).
    pub refused: u32,
    /// Units whose key a committed unit had consumed already.
    pub duplicate: u32,
    /// The observation's kind is not routed here, by its `snake_case` name.
    pub not_routed: Option<&'static str>,
}

impl Recorded {
    fn add<R>(&mut self, saved: Result<Outcome<R>, SaveError>) -> Result<(), SaveError> {
        match saved {
            Ok(Outcome::Accepted { .. }) => self.accepted += 1,
            Ok(Outcome::Refused { .. }) => self.refused += 1,
            Err(SaveError::DuplicateMessage(_)) => self.duplicate += 1,
            Err(error) => return Err(error),
        }
        Ok(())
    }
}

/// The inbox of one host's data plane, deciding under the run's epoch.
pub struct Inbox<'s, J, C, R, N> {
    store: &'s AggregateStore<J, C, R, N>,
    host: HostId,
    run_epoch: Epoch,
    names: Mutex<BTreeMap<OwnedVmName, AttemptId>>,
    placements: PlacementIndex,
}

impl<J, C, R, N> std::fmt::Debug for Inbox<'_, J, C, R, N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inbox")
            .field("host", &self.host)
            .field("run_epoch", &self.run_epoch)
            .finish_non_exhaustive()
    }
}

impl<'s, J: Journal, C: Clock, R: Rng, N: StoreNotifier> Inbox<'s, J, C, R, N> {
    /// The inbox of `host`'s data plane over `store`, issuing under `run_epoch`.
    pub fn new(store: &'s AggregateStore<J, C, R, N>, host: HostId, run_epoch: Epoch) -> Self {
        Self {
            store,
            host,
            run_epoch,
            names: Mutex::new(BTreeMap::new()),
            placements: PlacementIndex::default(),
        }
    }

    /// Routes answers to placement directives through `index`, the placement manager's
    /// (CP-07); without it every such answer is not routed.
    #[must_use]
    pub fn with_placements(mut self, index: PlacementIndex) -> Self {
        self.placements = index;
        self
    }

    /// Records `observation` (see the module docs). `Ok` is the caller's licence to ack it.
    ///
    /// # Errors
    /// [`SaveError`] other than a duplicate: nothing is acked, and the redelivery records it.
    pub async fn record(&self, observation: &Observation) -> Result<Recorded, SaveError> {
        let mut recorded = Recorded::default();
        let id = observation.observation_id;
        let epoch = observation.epoch;
        match &observation.kind {
            ObservationKind::CapacityReported(report) => {
                let body = HostCommand::RecordCapacityReport {
                    report_id: report.report_id,
                    capacity: reported_capacity(report),
                };
                let saved = self
                    .save::<Host>(self.host.as_str(), &[], epoch, id, None, body)
                    .await;
                recorded.add(saved)?;
            }
            ObservationKind::Inventory { vms, image_store } => {
                for (attempt, state) in host_attempts(self.store, &self.host).await? {
                    let Some(generation) = state.identity.as_ref().map(|i| i.generation) else {
                        continue;
                    };
                    for (n, fact) in (0_u8..).zip(inventory_facts(&state, vms)) {
                        let target = [&attempt.as_u128().to_be_bytes()[..], &[n]].concat();
                        let message = format!("{attempt}/{n}");
                        let body = self.observed(id, observation.epoch, generation, fact);
                        let saved = self
                            .save::<Attempt>(
                                &attempt.to_string(),
                                &target,
                                epoch,
                                id,
                                Some(&message),
                                body,
                            )
                            .await;
                        recorded.add(saved)?;
                    }
                }
                self.resolve_placements(observation, vms, image_store.as_ref(), &mut recorded)
                    .await?;
            }
            ObservationKind::ImagePulled { digest }
            | ObservationKind::ImageMaterialised { digest, .. } => {
                let instance = placement_instance(&self.host, digest);
                self.placement_answer(observation, Some(instance), &mut recorded)
                    .await?;
            }
            ObservationKind::ImageEvicted { .. } => {
                self.placement_answer(observation, None, &mut recorded)
                    .await?;
            }
            ObservationKind::RunnerExited { vm_name, .. }
            | ObservationKind::VmObservedStopped { vm_name } => {
                let Some(fact) = fact_of(observation) else {
                    recorded.not_routed = Some("unmapped");
                    return Ok(recorded);
                };
                let Some(attempt) = self.attempt_named(vm_name).await? else {
                    recorded.not_routed = Some("unknown_vm");
                    return Ok(recorded);
                };
                let body = self.observed(id, observation.epoch, vm_name.generation(), fact);
                let saved = self
                    .save::<Attempt>(&attempt.to_string(), &[], epoch, id, None, body)
                    .await;
                recorded.add(saved)?;
            }
            _ => match (
                observation.answers.and_then(|a| a.attempt),
                fact_of(observation),
            ) {
                (Some(attempt), Some(fact)) => {
                    let body = self.observed(id, observation.epoch, attempt.generation, fact);
                    let saved = self
                        .save::<Attempt>(
                            &attempt.attempt_id.to_string(),
                            &[],
                            epoch,
                            id,
                            None,
                            body,
                        )
                        .await;
                    recorded.add(saved)?;
                }
                _ if observation.answers.is_some_and(|a| {
                    a.attempt.is_none() && self.placements.get(a.directive_id).is_some()
                }) =>
                {
                    self.placement_answer(observation, None, &mut recorded)
                        .await?;
                }
                _ => recorded.not_routed = Some(kind_name(&observation.kind)),
            },
        }
        Ok(recorded)
    }

    /// Records an answer to a placement directive on its placement stream: `instance` when the
    /// observation names its host and digest, else the one the index knows for the directive.
    async fn placement_answer(
        &self,
        observation: &Observation,
        instance: Option<String>,
        recorded: &mut Recorded,
    ) -> Result<(), SaveError> {
        let Some(directive_id) = observation.answers.map(|a| a.directive_id) else {
            recorded.not_routed = Some(kind_name(&observation.kind));
            return Ok(());
        };
        let Some(instance) =
            instance.or_else(|| self.placements.get(directive_id).map(|p| p.instance))
        else {
            recorded.not_routed = Some("unknown_placement");
            return Ok(());
        };
        let body = match &observation.kind {
            ObservationKind::ImagePulled { .. } => PlacementCommand::RecordPulled { directive_id },
            ObservationKind::ImageMaterialised { disk_bytes, .. } => {
                PlacementCommand::RecordMaterialised {
                    directive_id,
                    disk_bytes: *disk_bytes,
                }
            }
            ObservationKind::ImageEvicted { .. } => {
                PlacementCommand::RecordEvicted { directive_id }
            }
            ObservationKind::DirectiveRefused { reason } => PlacementCommand::RecordRefused {
                directive_id,
                reason: refusal(*reason),
            },
            ObservationKind::OutcomeUnknown { .. } => {
                PlacementCommand::RecordOutcomeUnknown { directive_id }
            }
            _ => {
                recorded.not_routed = Some(kind_name(&observation.kind));
                return Ok(());
            }
        };
        let saved = self
            .save::<ImagePlacement>(
                &instance,
                &directive_id.as_u128().to_be_bytes(),
                observation.epoch,
                observation.observation_id,
                None,
                body,
            )
            .await;
        if matches!(saved, Ok(Outcome::Accepted { .. })) {
            self.placements.remove(directive_id);
        }
        recorded.add(saved)
    }

    /// A complete inventory, with the image store's listing beside it when the session carries
    /// one, resolves this host's placement directives of `Unknown` outcome (see the module docs).
    async fn resolve_placements(
        &self,
        observation: &Observation,
        vms: &[VmObservation],
        image_store: Option<&ImageStoreListing>,
        recorded: &mut Recorded,
    ) -> Result<(), SaveError> {
        for (directive_id, pending) in self.placements.on_host(&self.host) {
            let state = self.store.load::<ImagePlacement>(&pending.instance).await?;
            let Some(open) = state
                .pending
                .filter(|p| p.directive_id == directive_id)
                .filter(|p| resolvable(p.outcome))
            else {
                continue;
            };
            let after_send = open.sent_epoch.is_some_and(|sent| observation.epoch > sent);
            let Some(body) = resolution(
                directive_id,
                open.directive,
                &pending.digest,
                Listed {
                    vms,
                    image_store,
                    after_send,
                },
            ) else {
                continue;
            };
            let message = format!("placement/{directive_id}");
            let saved = self
                .save::<ImagePlacement>(
                    &pending.instance,
                    &directive_id.as_u128().to_be_bytes(),
                    observation.epoch,
                    observation.observation_id,
                    Some(&message),
                    body,
                )
                .await;
            if matches!(saved, Ok(Outcome::Accepted { .. })) {
                self.placements.remove(directive_id);
            }
            recorded.add(saved)?;
        }
        Ok(())
    }

    fn observed(
        &self,
        observation_id: ObservationId,
        epoch: Epoch,
        generation: Generation,
        fact: Fact,
    ) -> AttemptCommand {
        AttemptCommand::RecordObservation(AttemptObservation {
            observation_id,
            epoch,
            generation,
            fact,
        })
    }

    /// Saves `body` on `instance`, keyed `<epoch>/<observation id>` (and `/<message>` within it),
    /// its command id derived from the host, the epoch, the observation and `target`: a data
    /// plane restarted under a new epoch may number its observations afresh, so an id is unique
    /// only within its epoch.
    async fn save<A: Aggregate>(
        &self,
        instance: &str,
        target: &[u8],
        epoch: Epoch,
        observation_id: ObservationId,
        message: Option<&str>,
        body: A::Body,
    ) -> Result<Outcome<A::Rejection>, SaveError>
    where
        A::Body: Clone,
    {
        let base = format!("{}/{observation_id}", epoch.get());
        let key_text = message.map_or(base.clone(), |m| format!("{base}/{m}"));
        let key = InboxKey {
            source: inbox_source(&self.host).map_err(SaveError::InvalidName)?,
            message_id: MessageId::new(&key_text).map_err(SaveError::InvalidName)?,
        };
        let command_id = CommandId::from_u128(derive(
            ID_DOMAIN,
            &[
                b"unit",
                self.host.as_str().as_bytes(),
                &epoch.get().to_be_bytes(),
                &observation_id.as_u128().to_be_bytes(),
                target,
            ],
        ));
        let command = Command::new(
            command_id,
            self.store.clock().now(),
            self.run_epoch,
            Actor::Automation { policy: INBOX },
            body,
        );
        self.store
            .execute::<A>(SaveRequest::new(instance, command).with_inbox(key))
            .await
    }

    /// The attempt of this host whose VM is exactly `name`, refreshing the index on a miss.
    async fn attempt_named(&self, name: &OwnedVmName) -> Result<Option<AttemptId>, SaveError> {
        if let Some(found) = self.lock().get(name) {
            return Ok(Some(*found));
        }
        let fresh: BTreeMap<OwnedVmName, AttemptId> = host_attempts(self.store, &self.host)
            .await?
            .into_iter()
            .filter_map(|(id, state)| state.identity.map(|i| (i.vm_name, id)))
            .collect();
        let found = fresh.get(name).copied();
        *self.lock() = fresh;
        Ok(found)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<OwnedVmName, AttemptId>> {
        self.names.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The attempts that may have a VM on `host`, with their states: the `Host`'s leases, one fold
/// of the host's stream and one of each leased attempt's, so the read is bounded by the host's
/// slots and never folds another host's or a released attempt's stream. A lease is released only
/// on the attempt's observed deletion or loss, or when nothing was ever started
/// (`ReleaseEvidence`); a quarantined lease is kept (I28). An attempt with no lease left has no VM
/// to read in a listing: a VM still listed under its name is an unrecorded `mf-*` name, reported
/// (I04).
///
/// # Errors
/// [`SaveError`] when a stream cannot be read or folded.
pub async fn host_attempts<J: Journal, C: Clock, R: Rng, N: StoreNotifier>(
    store: &AggregateStore<J, C, R, N>,
    host: &HostId,
) -> Result<Vec<(AttemptId, AttemptState)>, SaveError> {
    let leased = store.load::<Host>(host.as_str()).await?.leases;
    let mut found = Vec::with_capacity(leased.len());
    for attempt in leased.keys() {
        let state = store.load::<Attempt>(&attempt.to_string()).await?;
        if state
            .identity
            .as_ref()
            .is_some_and(|identity| identity.host_id == *host)
        {
            found.push((*attempt, state));
        }
    }
    Ok(found)
}

/// What a complete listing says of `state`'s VM, in order: `VmPresent` when it lists the exact
/// name with a readable instance id (resolving an `Unknown` effect, U1), then
/// `VmObservedStopped` when the listing shows stopped a VM the attempt records running with no
/// directive pending (stopped not by us: a serving attempt retires); `VmAbsent` when it does
/// not list the name; nothing for an attempt with no VM name or a closed stream, or a listed VM
/// whose instance id is unread. Each fact is its own unit, keyed `<observation>/<attempt>/<n>`.
#[must_use]
pub fn inventory_facts(state: &AttemptState, vms: &[VmObservation]) -> Vec<Fact> {
    let Some(identity) = state.identity.as_ref() else {
        return Vec::new();
    };
    if matches!(state.phase, Phase::Closed | Phase::NotStarted) {
        return Vec::new();
    }
    let name = identity.vm_name.to_string();
    match vms.iter().find(|vm| vm.name == name) {
        Some(vm) => match &vm.instance_id {
            None => Vec::new(),
            Some(instance_id) => {
                let mut facts = vec![Fact::VmPresent {
                    instance_id: instance_id.clone(),
                    running: vm.running,
                }];
                if !vm.running
                    && state.vm_running
                    && state.pending.is_none()
                    && state.instance_id.as_ref() == Some(instance_id)
                {
                    facts.push(Fact::VmObservedStopped);
                }
                facts
            }
        },
        None => vec![Fact::VmAbsent],
    }
}

/// `report` as the `Host` records it: totals, foreign use, slots and macOS guests, with every
/// unknown dimension delivering nothing (see the module docs).
#[must_use]
pub fn reported_capacity(report: &CapacityReport) -> ReportedCapacity {
    let mut capacity = ReportedCapacity {
        total: report.totals.resources,
        foreign: report.foreign.resources,
        base: mayfly_domain::Resources::new(0, 0),
        vm_slots: report.totals.vm_slots,
        foreign_vms: report.foreign.vm_slots,
        macos_guests: report.totals.macos_guests,
        foreign_macos_guests: report.foreign.macos_guests,
    };
    for unknown in &report.unknown {
        match unknown.dimension {
            ResourceDimension::Memory => capacity.total.memory_bytes = 0,
            ResourceDimension::Disk => capacity.total.disk_bytes = 0,
            ResourceDimension::VmSlots => capacity.vm_slots = 0,
            ResourceDimension::MacosGuests => capacity.macos_guests = 0,
        }
    }
    capacity
}

/// The attempt's [`Fact`] for an answer or a VM fact; `None` for a kind that is not about one
/// attempt's VM, or an answer without its directive.
#[must_use]
pub fn fact_of(observation: &Observation) -> Option<Fact> {
    let directive = observation.answers.map(|a| a.directive_id);
    let answer = |make: fn(mayfly_domain::DirectiveId) -> Fact| directive.map(make);
    match &observation.kind {
        ObservationKind::VmCloned { instance_id } => directive.map(|directive_id| Fact::VmCloned {
            directive_id,
            instance_id: instance_id.clone(),
        }),
        ObservationKind::VmBooted => answer(|directive_id| Fact::VmBooted { directive_id }),
        ObservationKind::GuestVerified { facts } => {
            directive.map(|directive_id| Fact::GuestVerified {
                directive_id,
                facts: GuestFacts {
                    os: guest_os(facts.os),
                    uptime: facts.uptime,
                },
            })
        }
        ObservationKind::GuestRejected { reason } => {
            directive.map(|directive_id| Fact::GuestRejected {
                directive_id,
                reason: guest_rejection(*reason),
            })
        }
        ObservationKind::RunnerLaunched => {
            answer(|directive_id| Fact::RunnerLaunched { directive_id })
        }
        ObservationKind::RunnerLaunchUnknown => {
            answer(|directive_id| Fact::RunnerLaunchUnknown { directive_id })
        }
        ObservationKind::RunnerExited { code, .. } => Some(Fact::RunnerExited { code: *code }),
        ObservationKind::DiagnosticsExported { bytes, sha256 } => {
            directive.map(|directive_id| Fact::DiagnosticsExported {
                directive_id,
                bytes: *bytes,
                sha256: *sha256,
            })
        }
        ObservationKind::DiagnosticsLost { reason } => {
            directive.map(|directive_id| Fact::DiagnosticsLost {
                directive_id,
                reason: diagnostics_loss(*reason),
            })
        }
        ObservationKind::VmStopped => answer(|directive_id| Fact::VmStopped { directive_id }),
        ObservationKind::VmDeleted => answer(|directive_id| Fact::VmDeleted { directive_id }),
        ObservationKind::VmObservedStopped { .. } => Some(Fact::VmObservedStopped),
        ObservationKind::DirectiveRefused { reason } => {
            directive.map(|directive_id| Fact::DirectiveRefused {
                directive_id,
                reason: refusal(*reason),
            })
        }
        ObservationKind::OutcomeUnknown { .. } => {
            answer(|directive_id| Fact::OutcomeUnknown { directive_id })
        }
        ObservationKind::Inventory { .. }
        | ObservationKind::InventoryIncomplete { .. }
        | ObservationKind::CapacityReported(..)
        | ObservationKind::HostFacts(..)
        | ObservationKind::HypervisorLimitHit
        | ObservationKind::ImagePulled { .. }
        | ObservationKind::ImageMaterialised { .. }
        | ObservationKind::ImageEvicted { .. } => None,
    }
}

const fn guest_os(os: mayfly_protocol::GuestOs) -> GuestOs {
    match os {
        mayfly_protocol::GuestOs::Macos => GuestOs::Macos,
        mayfly_protocol::GuestOs::Linux => GuestOs::Linux,
    }
}

const fn guest_rejection(reason: mayfly_protocol::GuestRejection) -> GuestRejection {
    match reason {
        mayfly_protocol::GuestRejection::ChallengeFailed => GuestRejection::ChallengeFailed,
        mayfly_protocol::GuestRejection::AgentMissing => GuestRejection::AgentMissing,
        mayfly_protocol::GuestRejection::FactsMismatch => GuestRejection::FactsMismatch,
    }
}

const fn diagnostics_loss(reason: mayfly_protocol::DiagnosticsLoss) -> DiagnosticsLoss {
    match reason {
        mayfly_protocol::DiagnosticsLoss::Refused => DiagnosticsLoss::Refused,
        mayfly_protocol::DiagnosticsLoss::GuestUnreachable => DiagnosticsLoss::GuestUnreachable,
        mayfly_protocol::DiagnosticsLoss::OverLimit => DiagnosticsLoss::OverLimit,
    }
}

/// The attempt's reading of a definite refusal. `capacity_unknown` is a definite no like
/// `insufficient_resources` (the reconciler holds the host until a newer report, CP-03);
/// `protected_target` is a target that is not ours to touch, a proof mismatch; and
/// `deadline_expired`, a stage that never started, is a failed effect: nothing happened either
/// way. `target_absent` is the attempt's own reason (ADR 0066).
#[must_use]
pub const fn refusal(reason: mayfly_protocol::RefusalReason) -> RefusalReason {
    match reason {
        mayfly_protocol::RefusalReason::InsufficientResources { .. }
        | mayfly_protocol::RefusalReason::CapacityUnknown { .. } => {
            RefusalReason::InsufficientResources
        }
        mayfly_protocol::RefusalReason::ProofMismatch
        | mayfly_protocol::RefusalReason::ProtectedTarget => RefusalReason::ProofMismatch,
        mayfly_protocol::RefusalReason::TargetAbsent => RefusalReason::TargetAbsent,
        mayfly_protocol::RefusalReason::SafeMode => RefusalReason::SafeMode,
        mayfly_protocol::RefusalReason::DeadlineExpired
        | mayfly_protocol::RefusalReason::EffectFailed => RefusalReason::EffectFailed,
    }
}

/// Whether a pending placement directive's outcome may be resolved from a listing: only once it
/// is `Unknown` (I10, ADR 0025). An answer still in flight may yet come and is waited for
/// (`tests/store/placement.rs::should_resolve_nothing_from_a_listing_when_the_directive_is_in_flight`).
const fn resolvable(outcome: PendingOutcome) -> bool {
    matches!(outcome, PendingOutcome::Unknown { .. })
}

/// What one complete inventory shows: its VMs, the image store's listing beside it when there
/// is one, and whether it was taken in a later run epoch than the directive's send.
#[derive(Clone, Copy)]
struct Listed<'a> {
    vms: &'a [VmObservation],
    image_store: Option<&'a ImageStoreListing>,
    after_send: bool,
}

/// What `listed` shows about `directive` of `digest`'s placement, as the command that records
/// it; `None` when it shows nothing (see the module docs). Pinned by the placement tests the
/// module docs name.
fn resolution(
    directive_id: DirectiveId,
    directive: PlacementDirective,
    digest: &ImageDigest,
    listed: Listed<'_>,
) -> Option<PlacementCommand> {
    let Listed {
        vms,
        image_store,
        after_send,
    } = listed;
    let named = |version| {
        let name = base_name(digest, version);
        vms.iter().any(|vm| vm.name == name)
    };
    let ready = |version| {
        let name = base_name(digest, version);
        image_store
            .filter(|_| after_send)?
            .bases
            .iter()
            .find(|base| base.base_name.as_str() == name && base.digest == *digest)
    };
    match directive {
        PlacementDirective::PullImage => image_store.and_then(|listing| {
            if !listing.images.contains(digest) {
                Some(PlacementCommand::RecordNotApplied { directive_id })
            } else if after_send {
                Some(PlacementCommand::RecordPulled { directive_id })
            } else {
                None
            }
        }),
        PlacementDirective::MaterialiseImage { runner_version } => {
            if let Some(base) = ready(runner_version) {
                Some(PlacementCommand::RecordMaterialised {
                    directive_id,
                    disk_bytes: base.disk_bytes,
                })
            } else if named(runner_version) {
                None
            } else {
                Some(PlacementCommand::RecordNotApplied { directive_id })
            }
        }
        PlacementDirective::EvictImage { runner_version } => {
            (!named(runner_version)).then_some(PlacementCommand::RecordEvicted { directive_id })
        }
    }
}

const fn kind_name(kind: &ObservationKind) -> &'static str {
    match kind {
        ObservationKind::InventoryIncomplete { .. } => "inventory_incomplete",
        ObservationKind::HostFacts(..) => "host_facts",
        ObservationKind::HypervisorLimitHit => "hypervisor_limit_hit",
        ObservationKind::ImagePulled { .. } => "image_pulled",
        ObservationKind::ImageMaterialised { .. } => "image_materialised",
        ObservationKind::ImageEvicted { .. } => "image_evicted",
        ObservationKind::CapacityReported(..) | ObservationKind::Inventory { .. } => "routed",
        ObservationKind::VmCloned { .. }
        | ObservationKind::VmBooted
        | ObservationKind::GuestVerified { .. }
        | ObservationKind::GuestRejected { .. }
        | ObservationKind::RunnerLaunched
        | ObservationKind::RunnerLaunchUnknown
        | ObservationKind::RunnerExited { .. }
        | ObservationKind::DiagnosticsExported { .. }
        | ObservationKind::DiagnosticsLost { .. }
        | ObservationKind::VmStopped
        | ObservationKind::VmDeleted
        | ObservationKind::VmObservedStopped { .. }
        | ObservationKind::DirectiveRefused { .. }
        | ObservationKind::OutcomeUnknown { .. } => "no_attempt",
    }
}
