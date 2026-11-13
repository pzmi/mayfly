//! The registration manager and the reaper over a real store and the in-memory GitHub
//! (component level, CP-05; policies P9 to P14 but P12; ver §8.4 F06, F07, F32): one mint per
//! registration and only its handle journaled, an unknown issue never minted again and resolved
//! only by listings past the grace, a config that reached no handle presumed leaked and removed
//! by its id, the online confirmation, deregistration before every stop (a retirement's and a
//! quarantined VM's) with GitHub's busy answer withdrawing it, and the reaper removing only
//! journaled ids and reporting the rest.

use std::collections::BTreeSet;
use std::future::{Future as _, poll_fn};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use mayfly_control::github::{
    GitHubCallError, JitConfig, MessageId, Offer, OpenedSession, PolledMessage, RunnerId,
    RunnerRegistry as _, ScaleSetApi, ScaleSetId, ScaleSetInfo, ScaleSetSpec, SessionId,
};
use mayfly_control::journal::{Journal as _, StreamId};
use mayfly_control::registration::{
    JitBroker, JitScope, RegistrationConfig, RegistrationManager, ScaleSetDirectory, Step,
    SweepError, SweepTrigger, handle_of,
};
use mayfly_control::sessions::HostSession;
use mayfly_control::store::{Outcome, SaveRequest};
use mayfly_control_domain::attempt::{
    AttemptCommand, AttemptEvent, AttemptState, DirectiveKind, Fact, GuestFacts, GuestOs, Jit,
    Phase as AttemptPhase, QuarantineReason, RetireReason, RunnerRequestId, StopDeregistration,
    issuable,
};
use mayfly_control_domain::registration::{
    LeakReason, Phase, RegistrationCommand, RegistrationState, RequestRegistration,
    RunnerRegistration, SessionHealth,
};
use mayfly_control_domain::{Actor, Command};
use mayfly_domain::{
    AttemptId, CommandId, Deadline, DirectiveId, Epoch, EventId, Generation, HostId, Millis,
    OwnedVmName, PoolName, ProviderInstanceId, Sample, Slot,
};
use mayfly_protocol::Mode;
use mayfly_testkit::attempts::{instance, observe, save};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::contracts::scaleset::Flow;
use mayfly_testkit::github::{DEFAULT_GROUP, FakeRunnerRegistry, FakeScaleSetApi};
use mayfly_testkit::journal::MemoryJournal;

use crate::attempts::{EPOCH, Harness, NOW, harness};

const LABEL: &str = "mayfly";

type Manager<'s> = RegistrationManager<
    's,
    MemoryJournal,
    FakeClock,
    SeededRng,
    mayfly_control::delivery::Kicks,
    FakeScaleSetApi,
    FakeRunnerRegistry,
    Directory,
>;

/// One scale set, serving every pool, healthy or degraded.
struct Directory {
    set: ScaleSetId,
    health: SessionHealth,
}

impl ScaleSetDirectory for Directory {
    fn scale_set(&self, _pool: &PoolName) -> Option<(ScaleSetId, SessionHealth)> {
        Some((self.set, self.health))
    }

    fn scale_sets(&self) -> BTreeSet<ScaleSetId> {
        BTreeSet::from([self.set])
    }
}

struct Fixture {
    h: Harness,
    api: FakeScaleSetApi,
    registry: FakeRunnerRegistry,
    directory: Directory,
    broker: JitBroker,
}

fn fixture_with(health: SessionHealth) -> Fixture {
    let h = harness();
    let api = FakeScaleSetApi::new(Flow::Direct, FakeClock::at(NOW));
    let set = block_on(api.create_scale_set(&ScaleSetSpec {
        name: "mayfly".to_owned(),
        runner_group: DEFAULT_GROUP.to_owned(),
        labels: vec![LABEL.to_owned()],
    }))
    .unwrap_or_else(|e| panic!("{e}"))
    .id;
    let registry = api.registry();
    Fixture {
        h,
        api,
        registry,
        directory: Directory { set, health },
        broker: JitBroker::new(),
    }
}

fn fixture() -> Fixture {
    fixture_with(SessionHealth::Healthy)
}

impl Fixture {
    fn manager(&self) -> Manager<'_> {
        self.manager_with(&self.broker)
    }

    /// A manager as a restarted daemon has it: `broker` is the new, empty one.
    fn manager_with<'s>(&'s self, broker: &'s JitBroker) -> Manager<'s> {
        RegistrationManager::new(
            &self.h.store,
            self.h.sessions.clone(),
            &self.api,
            &self.registry,
            &self.directory,
            broker,
            RegistrationConfig::default(),
        )
    }

    fn attempt(&self, n: u128) -> AttemptState {
        self.h.state(n)
    }

    fn registration(&self, n: u128) -> RegistrationState {
        block_on(self.h.store.load::<RunnerRegistration>(&instance(n)))
            .unwrap_or_else(|e| panic!("{e}"))
    }

    fn vm_name(&self, n: u128) -> String {
        self.attempt(n)
            .identity
            .unwrap_or_else(|| panic!("attempt {n} is started"))
            .vm_name
            .to_string()
    }

    /// Saves `body` on attempt `n`, which must accept it.
    fn apply(&self, n: u128, command_id: u128, epoch: Epoch, body: AttemptCommand) {
        let outcome = block_on(save(&self.h.store, n, command_id, epoch, body));
        assert!(matches!(outcome, Outcome::Accepted { .. }), "{outcome:?}");
    }

    /// Saves `body` on attempt `n`'s registration, which must accept it (what an earlier run
    /// recorded before it died).
    fn record(&self, n: u128, command_id: u128, body: RegistrationCommand) {
        let command = Command::new(
            CommandId::from_u128(command_id),
            NOW,
            EPOCH,
            Actor::Automation {
                policy: "registration_manager",
            },
            body,
        );
        let outcome = block_on(
            self.h
                .store
                .execute::<RunnerRegistration>(SaveRequest::new(instance(n), command)),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(outcome, Outcome::Accepted { .. }), "{outcome:?}");
    }

    /// Attempt `n` activating: prepared, granted and freshly verified.
    fn activating(&self, n: u128) {
        self.h.start(n);
        let steps = [
            issue(n, 1),
            observe(
                ob(n, 1),
                EPOCH,
                Fact::VmCloned {
                    directive_id: directive(n, 1),
                    instance_id: ProviderInstanceId::sample(),
                },
            ),
            issue(n, 2),
            observe(
                ob(n, 2),
                EPOCH,
                Fact::VmBooted {
                    directive_id: directive(n, 2),
                },
            ),
            issue(n, 3),
            observe(
                ob(n, 3),
                EPOCH,
                Fact::GuestVerified {
                    directive_id: directive(n, 3),
                    facts: facts(),
                },
            ),
            AttemptCommand::BeginActivation {
                grant_ref: EventId::from_u128((n << 16) + 1),
            },
            issue(n, 4),
            observe(
                ob(n, 4),
                EPOCH,
                Fact::GuestVerified {
                    directive_id: directive(n, 4),
                    facts: facts(),
                },
            ),
        ];
        for (k, body) in steps.into_iter().enumerate() {
            self.apply(n, (n << 20) + k as u128, EPOCH, body);
        }
        assert_eq!(self.attempt(n).phase, AttemptPhase::Activating);
    }

    /// Attempt `n`'s runner launched (the handle recorded, the launch answered).
    fn launched(&self, n: u128) {
        self.apply(n, (n << 20) + 0x100, EPOCH, issue(n, 5));
        self.apply(
            n,
            (n << 20) + 0x101,
            EPOCH,
            observe(
                ob(n, 5),
                EPOCH,
                Fact::RunnerLaunched {
                    directive_id: directive(n, 5),
                },
            ),
        );
    }

    /// Attempt `n` listening, its runner online and its registration connected.
    fn connected(&self, m: &Manager<'_>, n: u128) -> RunnerId {
        self.activating(n);
        assert_eq!(block_on(m.step(attempt(n))), Ok(Step::Moved));
        self.launched(n);
        let id = self.runner_id(n);
        self.api
            .set_runner_online(id, true)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(block_on(m.step(attempt(n))), Ok(Step::Moved));
        assert_eq!(self.registration(n).phase, Phase::Connected);
        id
    }

    fn runner_id(&self, n: u128) -> RunnerId {
        self.registration(n)
            .runner_id
            .unwrap_or_else(|| panic!("attempt {n}'s runner id is journaled"))
    }

    /// The runner `id` takes a job, as GitHub assigns one to it.
    fn busy(&self, id: RunnerId) {
        block_on(self.api.create_session(self.directory.set, "host/pool"))
            .unwrap_or_else(|e| panic!("{e}"));
        let job = self.api.queue_jobs(1, LABEL, 7).remove(0);
        block_on(self.api.poll(self.directory.set, None, 1)).unwrap_or_else(|e| panic!("{e}"));
        self.api
            .start_job(&job, id)
            .unwrap_or_else(|e| panic!("{e}"));
    }

    /// Every payload of attempt `n`'s and its registration's streams.
    fn payloads(&self, n: u128) -> String {
        ["attempt", "registration"]
            .iter()
            .flat_map(|aggregate| {
                let stream = StreamId::new(&format!("{aggregate}-{}", instance(n)))
                    .unwrap_or_else(|e| panic!("{e}"));
                block_on(self.h.journal.read_stream(&stream, None, None))
                    .unwrap_or_else(|e| panic!("{e}"))
            })
            .map(|e| e.payload.as_str().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn registration_events(&self, n: u128) -> usize {
        let stream = StreamId::new(&format!("registration-{}", instance(n)))
            .unwrap_or_else(|e| panic!("{e}"));
        block_on(self.h.journal.read_stream(&stream, None, None))
            .unwrap_or_else(|e| panic!("{e}"))
            .len()
    }

    fn runners_named(&self, name: &str) -> usize {
        self.api.catch_up_listings();
        block_on(self.registry.find_runners(name))
            .unwrap_or_else(|e| panic!("{e}"))
            .len()
    }
}

fn facts() -> GuestFacts {
    GuestFacts {
        os: GuestOs::Linux,
        uptime: Millis::new(30_000),
    }
}

fn attempt(n: u128) -> AttemptId {
    AttemptId::from_u128(n)
}

fn directive(n: u128, k: u128) -> DirectiveId {
    DirectiveId::from_u128((n << 16) + k)
}

fn ob(n: u128, k: u128) -> u128 {
    (n << 16) + 0x8000 + k
}

fn issue(n: u128, k: u128) -> AttemptCommand {
    AttemptCommand::IssueNext {
        directive_id: directive(n, k),
        deadline: Deadline::after(NOW, Millis::new(60_000)),
    }
}

fn launches(events: &[AttemptEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AttemptEvent::JitHandleIssued(..)))
        .count()
}

#[test]
fn should_mint_once_and_record_only_the_handle_when_an_attempt_is_activating() {
    // given
    let w = fixture();
    let m = w.manager();
    w.activating(1);

    // when
    let first = block_on(m.step(attempt(1)));
    let again = block_on(m.step(attempt(1)));

    // then: one runner of the VM's name, its id journaled, the handle on the attempt, the
    // config in the broker and nowhere in the journal
    assert_eq!(first, Ok(Step::Moved));
    assert_eq!(again, Ok(Step::Idle));
    let r = w.registration(1);
    assert_eq!(r.phase, Phase::Issued);
    assert_eq!(w.runners_named(&w.vm_name(1)), 1);
    let handle = handle_of(attempt(1));
    assert_eq!(w.attempt(1).jit, Jit::Issued(handle));
    let config = w
        .broker
        .take(
            handle,
            JitScope {
                attempt: attempt(1),
                epoch: EPOCH,
            },
        )
        .unwrap_or_else(|| panic!("the config is held"));
    assert!(config.expose().starts_with("fake-jit-config-"));
    assert!(!w.payloads(1).contains(config.expose().as_str()));
}

#[test]
fn should_mint_nothing_when_the_scale_sets_session_is_degraded() {
    // given
    let w = fixture_with(SessionHealth::Degraded);
    let m = w.manager();
    w.activating(1);

    // when
    let step = block_on(m.step(attempt(1)));

    // then
    assert_eq!(step, Ok(Step::Blocked("scale_set_degraded")));
    assert_eq!(w.registration(1).phase, Phase::Unrequested);
    assert_eq!(w.runners_named(&w.vm_name(1)), 0);
}

#[test]
fn should_retire_the_attempt_and_never_mint_again_when_the_jit_answer_is_lost() {
    // given: F06, GitHub registers the runner and the answer is lost
    let w = fixture();
    let m = w.manager();
    w.activating(1);
    w.api.lose_next_jit_answer();

    // when
    let first = block_on(m.step(attempt(1)));
    let again = block_on(m.step(attempt(1)));

    // then: an unknown issue, the attempt retired without a handle, one runner of that name
    assert_eq!(first, Ok(Step::Moved));
    assert_eq!(again, Ok(Step::Idle));
    assert_eq!(w.registration(1).phase, Phase::IssueUnknown);
    let a = w.attempt(1);
    assert_eq!(a.phase, AttemptPhase::Retiring);
    assert_eq!(a.retire_reason, Some(RetireReason::JitLost));
    assert_eq!(launches(&w.h.events(1)), 0);
    assert_eq!(w.runners_named(&w.vm_name(1)), 1);
}

#[test]
fn should_find_the_runner_by_name_on_the_second_listing_when_the_first_lagged() {
    // given: F07, the lost answer's runner not listed yet, and a runner of another name
    let w = fixture();
    let m = w.manager();
    w.activating(1);
    w.api.lose_next_jit_answer();
    block_on(m.step(attempt(1))).unwrap_or_else(|e| panic!("{e}"));
    let other = w.api.add_runner(w.directory.set, "build-box-7");

    // when: the first listing shows nothing; GitHub catches up; the grace passes
    let first = block_on(m.pass());
    let unchanged = w.registration(1).phase;
    w.api.catch_up_listings();
    w.h.clock
        .advance(RegistrationConfig::default().listing_grace);
    let second = block_on(m.pass());

    // then: found by name, journaled, removed by that id; nothing else removed; the attempt
    // never got a config
    assert_eq!(first.waiting, vec![(attempt(1), "listing_grace")]);
    assert_eq!(unchanged, Phase::IssueUnknown);
    assert_eq!(second.moved, vec![attempt(1)]);
    let r = w.registration(1);
    assert_eq!(r.phase, Phase::Deregistered);
    assert_eq!(r.leaked, Some(LeakReason::IssueUnknown));
    assert!(w.api.runner(w.runner_id(1)).is_none());
    assert!(w.api.runner(other).is_some());
    assert_eq!(launches(&w.h.events(1)), 0);
}

#[test]
fn should_record_absent_only_after_a_second_empty_listing_when_the_grace_passed() {
    // given: a run that died after its mint was sent and before its answer was recorded, and
    // GitHub never registered the runner
    let w = fixture();
    let m = w.manager();
    w.activating(1);
    w.record(
        1,
        0x9001,
        RegistrationCommand::RequestRegistration(RequestRegistration {
            attempt_id: attempt(1),
            runner_name: w
                .attempt(1)
                .identity
                .unwrap_or_else(|| panic!("started"))
                .vm_name,
            scale_set: w.directory.set,
            session: SessionHealth::Healthy,
        }),
    );

    // when
    let recorded = block_on(m.step(attempt(1)));
    let first = block_on(m.pass());
    let at_once = block_on(m.pass());
    w.h.clock.advance(Millis::new(
        RegistrationConfig::default().listing_grace.get() - 1,
    ));
    let early = block_on(m.pass());
    w.h.clock.advance(Millis::new(1));
    let late = block_on(m.pass());

    // then
    assert_eq!(recorded, Ok(Step::Moved));
    for waited in [&first, &at_once, &early] {
        assert_eq!(waited.waiting, vec![(attempt(1), "listing_grace")]);
    }
    assert_eq!(late.moved, vec![attempt(1)]);
    assert_eq!(w.registration(1).phase, Phase::Absent);
    assert_eq!(w.attempt(1).retire_reason, Some(RetireReason::JitLost));
    assert_eq!(w.api.runner_listings(), 4);
}

#[test]
fn should_presume_leaked_and_remove_by_id_when_a_crash_left_an_issued_config_without_handle() {
    // given: F32, a run minted, recorded the runner id and died before the handle
    let w = fixture();
    w.activating(1);
    let name = w.vm_name(1);
    let jit = block_on(w.api.generate_jit_config(w.directory.set, &name, "_work"))
        .unwrap_or_else(|e| panic!("{e}"));
    w.record(
        1,
        0x9001,
        RegistrationCommand::RequestRegistration(RequestRegistration {
            attempt_id: attempt(1),
            runner_name: w
                .attempt(1)
                .identity
                .unwrap_or_else(|| panic!("started"))
                .vm_name,
            scale_set: w.directory.set,
            session: SessionHealth::Healthy,
        }),
    );
    w.record(
        1,
        0x9002,
        RegistrationCommand::RecordIssued {
            runner_id: jit.runner.id,
        },
    );
    let restarted = JitBroker::new();
    let m = w.manager_with(&restarted);

    // when
    let step = block_on(m.step(attempt(1)));

    // then: presumed leaked, removed by its id, the attempt retired and never launched
    assert_eq!(step, Ok(Step::Moved));
    let r = w.registration(1);
    assert_eq!(r.leaked, Some(LeakReason::HandleOrphaned));
    assert_eq!(r.phase, Phase::Deregistered);
    assert!(w.api.runner(jit.runner.id).is_none());
    let a = w.attempt(1);
    assert_eq!(a.phase, AttemptPhase::Retiring);
    assert_eq!(a.retire_reason, Some(RetireReason::JitPresumedLeaked));
    assert_eq!(launches(&w.h.events(1)), 0);
}

#[test]
fn should_deregister_then_retire_when_a_restart_orphaned_the_handle() {
    // given: F32, the handle recorded, then a restart adopts the attempt at epoch 2
    let w = fixture();
    w.activating(1);
    block_on(w.manager().step(attempt(1))).unwrap_or_else(|e| panic!("{e}"));
    let id = w.runner_id(1);
    let restarted_epoch = Epoch::new(2);
    w.h.sessions.open(
        HostId::sample(),
        HostSession {
            epoch: restarted_epoch,
            mode: Mode::Normal,
        },
    );
    w.apply(
        1,
        0x9100,
        restarted_epoch,
        AttemptCommand::Adopt {
            inventory_ref: EventId::from_u128(0x77),
        },
    );
    assert!(matches!(w.attempt(1).jit, Jit::Orphaned(..)));
    let restarted = JitBroker::new();
    let m = w.manager_with(&restarted);

    // when
    let step = block_on(m.step(attempt(1)));

    // then: the runner removed by its id before the attempt's retirement is confirmed
    assert_eq!(step, Ok(Step::Moved));
    assert_eq!(w.registration(1).phase, Phase::Deregistered);
    assert!(w.api.runner(id).is_none());
    let events = w.h.events(1);
    let requested = events
        .iter()
        .position(|e| matches!(e, AttemptEvent::RetirementRequested(..)));
    let retired = events
        .iter()
        .position(|e| matches!(e, AttemptEvent::AttemptRetired(..)));
    assert!(requested.is_some() && requested < retired, "{events:?}");
    assert_eq!(
        w.attempt(1).retire_reason,
        Some(RetireReason::JitPresumedLeaked)
    );
}

#[test]
fn should_confirm_the_runner_online_when_github_shows_its_id_after_the_launch() {
    // given
    let w = fixture();
    let m = w.manager();

    // when
    let id = w.connected(&m, 1);

    // then
    let r = w.registration(1);
    assert_eq!(r.phase, Phase::Connected);
    assert_eq!(r.runner_id, Some(id));
    assert!(r.leaked.is_none());
}

#[test]
fn should_presume_leaked_and_deregister_when_the_runner_is_not_seen_online_by_the_deadline() {
    // given: the launch answered, the runner never online
    let w = fixture();
    let m = w.manager();
    w.activating(1);
    block_on(m.step(attempt(1))).unwrap_or_else(|e| panic!("{e}"));
    w.launched(1);
    let id = w.runner_id(1);

    // when
    let before = block_on(m.step(attempt(1)));
    w.h.clock
        .advance(RegistrationConfig::default().online_deadline);
    let after = block_on(m.step(attempt(1)));

    // then
    assert_eq!(before, Ok(Step::Waiting("not_online_yet")));
    assert_eq!(after, Ok(Step::Moved));
    let r = w.registration(1);
    assert_eq!(r.leaked, Some(LeakReason::NotSeenOnline));
    assert_eq!(r.phase, Phase::Deregistered);
    assert!(w.api.runner(id).is_none());
    let a = w.attempt(1);
    assert_eq!(a.phase, AttemptPhase::Retiring);
    assert_eq!(a.retire_reason, Some(RetireReason::JitPresumedLeaked));
}

#[test]
fn should_deregister_before_confirming_when_a_listening_runners_retirement_is_requested() {
    // given: P11, an idle connected runner, its attempt retired as surplus
    let w = fixture();
    let m = w.manager();
    let id = w.connected(&m, 1);
    w.apply(
        1,
        0x9200,
        EPOCH,
        AttemptCommand::RequestRetirement {
            reason: RetireReason::Surplus,
        },
    );
    assert_eq!(w.attempt(1).phase, AttemptPhase::Listening);

    // when
    let step = block_on(m.step(attempt(1)));

    // then
    assert_eq!(step, Ok(Step::Moved));
    assert_eq!(w.registration(1).phase, Phase::Deregistered);
    assert!(w.api.runner(id).is_none());
    let a = w.attempt(1);
    assert_eq!(a.phase, AttemptPhase::Retiring);
    assert_eq!(a.retire_reason, Some(RetireReason::Surplus));
}

#[test]
fn should_withdraw_the_retirement_when_github_answers_busy_for_its_runner() {
    // given: P11's race (I21), the runner took a job before its removal
    let w = fixture();
    let m = w.manager();
    let id = w.connected(&m, 1);
    w.apply(
        1,
        0x9200,
        EPOCH,
        AttemptCommand::RequestRetirement {
            reason: RetireReason::Surplus,
        },
    );
    w.busy(id);

    // when
    let answered = block_on(m.step(attempt(1)));
    let next = block_on(m.step(attempt(1)));

    // then: busy, the retirement withdrawn, the runner and its VM kept
    assert_eq!(answered, Ok(Step::Waiting("runner_busy")));
    assert_eq!(next, Ok(Step::Moved));
    let a = w.attempt(1);
    assert_eq!(a.phase, AttemptPhase::Listening);
    assert!(a.retirement_requested.is_none());
    assert!(w.api.runner(id).is_some());
    assert_eq!(w.registration(1).phase, Phase::Connected);
}

#[test]
fn should_stop_a_quarantined_vm_only_after_the_deregistration_when_its_runner_is_idle() {
    // given: DEC-06F, the attempt quarantined with its idle runner registered
    let w = fixture();
    let m = w.manager();
    let id = w.connected(&m, 1);
    w.apply(
        1,
        0x9300,
        EPOCH,
        AttemptCommand::Quarantine {
            reason: QuarantineReason::ForensicRetention,
        },
    );
    let before = w.attempt(1);
    assert_eq!(before.stop_deregistration, StopDeregistration::Requested);
    assert!(!matches!(
        issuable(&before),
        Ok(Some(DirectiveKind::StopVm { .. }))
    ));

    // when
    let step = block_on(m.step(attempt(1)));

    // then: the runner removed by its id, then the stop may be issued
    assert_eq!(step, Ok(Step::Moved));
    assert!(w.api.runner(id).is_none());
    assert_eq!(w.registration(1).phase, Phase::Deregistered);
    let after = w.attempt(1);
    assert_eq!(after.stop_deregistration, StopDeregistration::Confirmed);
    assert!(matches!(
        issuable(&after),
        Ok(Some(DirectiveKind::StopVm { .. }))
    ));
}

#[test]
fn should_keep_a_quarantined_vm_running_when_github_answers_busy_for_its_runner() {
    // given: DEC-06F, a job runs on the quarantined VM's runner
    let w = fixture();
    let m = w.manager();
    let id = w.connected(&m, 1);
    w.busy(id);
    w.apply(
        1,
        0x9300,
        EPOCH,
        AttemptCommand::Quarantine {
            reason: QuarantineReason::ForensicRetention,
        },
    );

    // when
    let answered = block_on(m.step(attempt(1)));
    let next = block_on(m.step(attempt(1)));

    // then
    assert_eq!(answered, Ok(Step::Waiting("runner_busy")));
    assert_eq!(next, Ok(Step::Moved));
    let a = w.attempt(1);
    assert_eq!(a.stop_deregistration, StopDeregistration::RefusedBusy);
    assert!(!matches!(
        issuable(&a),
        Ok(Some(DirectiveKind::StopVm { .. }))
    ));
    assert!(w.api.runner(id).is_some());
}

#[test]
fn should_remove_only_journaled_ids_and_report_the_rest_when_the_reaper_sweeps() {
    // given: attempt 1's job done and its VM retiring while GitHub still lists its runner; a
    // runner of mayfly's shape and one malformed `mf-` runner no registration journaled; a
    // runner of another name
    let w = fixture();
    let m = w.manager();
    let journaled = w.connected(&m, 1);
    w.apply(
        1,
        0x9400,
        EPOCH,
        AttemptCommand::BindJob {
            request_id: RunnerRequestId(1),
        },
    );
    w.apply(
        1,
        0x9401,
        EPOCH,
        observe(ob(1, 9), EPOCH, Fact::RunnerExited { code: 0 }),
    );
    w.apply(
        1,
        0x9402,
        EPOCH,
        AttemptCommand::RequestRetirement {
            reason: RetireReason::JobDone,
        },
    );
    assert_eq!(w.attempt(1).phase, AttemptPhase::Retiring);
    let shaped = OwnedVmName::new(
        PoolName::sample(),
        HostId::sample(),
        Slot::new(9),
        Generation::FIRST,
        AttemptId::from_u128(0x99),
    )
    .to_string();
    let unrecognised = w.api.add_runner(w.directory.set, &shaped);
    let malformed = w.api.add_runner(w.directory.set, "mf-not-a-vm-name");
    let foreign = w.api.add_runner(w.directory.set, "build-box-7");

    // when
    let report = block_on(m.sweep(SweepTrigger::RunStarted)).unwrap_or_else(|e| panic!("{e:?}"));

    // then
    assert_eq!(report.listed, 4);
    assert_eq!(report.deregistered, vec![journaled]);
    assert_eq!(
        report
            .unrecognised
            .iter()
            .map(|u| (u.id, u.name.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (unrecognised, shaped.as_str()),
            (malformed, "mf-not-a-vm-name")
        ]
    );
    assert_eq!(report.foreign, 1);
    assert!(w.api.runner(journaled).is_none());
    for kept in [unrecognised, malformed, foreign] {
        assert!(w.api.runner(kept).is_some(), "{kept:?} was removed");
    }
    assert_eq!(w.registration(1).phase, Phase::Deregistered);
    assert_eq!(m.last_sweep(), Some(report));
}

#[test]
fn should_list_at_most_once_an_hour_when_sweeps_follow_each_other() {
    // given
    let w = fixture();
    let m = w.manager();
    let interval = RegistrationConfig::default().listing_interval;
    block_on(m.sweep(SweepTrigger::RunStarted)).unwrap_or_else(|e| panic!("{e:?}"));
    let listed = w.api.runner_listings();

    // when
    let doctor = block_on(m.sweep(SweepTrigger::Doctor));
    w.h.clock.advance(interval);
    let daily = block_on(m.sweep(SweepTrigger::Daily));

    // then
    assert_eq!(
        doctor,
        Err(SweepError::BudgetSpent {
            next: Deadline::after(NOW, interval).instant()
        })
    );
    assert!(daily.is_ok(), "{daily:?}");
    assert_eq!(w.api.runner_listings(), listed + 1);
}

#[test]
fn should_hold_nothing_in_the_broker_when_the_attempt_left_activation_before_its_handle() {
    // given: the attempt retires as surplus while it is activating, before any mint
    let w = fixture();
    let m = w.manager();
    w.activating(1);
    w.apply(
        1,
        0x9500,
        EPOCH,
        AttemptCommand::RequestRetirement {
            reason: RetireReason::Surplus,
        },
    );

    // when
    let step = block_on(m.step(attempt(1)));

    // then: no registration asked for, nothing minted or held
    assert_eq!(step, Ok(Step::Idle));
    assert_eq!(w.registration(1).phase, Phase::Unrequested);
    assert!(w.broker.is_empty());
    assert_eq!(w.runners_named(&w.vm_name(1)), 0);
}

#[test]
fn should_bound_the_removals_when_a_leaked_runner_stays_busy() {
    // given: a restart orphaned the handle while the leaked config's runner runs a job
    let w = fixture();
    w.activating(1);
    block_on(w.manager().step(attempt(1))).unwrap_or_else(|e| panic!("{e}"));
    let id = w.runner_id(1);
    w.busy(id);
    let restarted_epoch = Epoch::new(2);
    w.h.sessions.open(
        HostId::sample(),
        HostSession {
            epoch: restarted_epoch,
            mode: Mode::Normal,
        },
    );
    w.apply(
        1,
        0x9100,
        restarted_epoch,
        AttemptCommand::Adopt {
            inventory_ref: EventId::from_u128(0x77),
        },
    );
    let restarted = JitBroker::new();
    let m = w.manager_with(&restarted);
    let first = block_on(m.step(attempt(1)));
    let removals = w.api.runner_removals();
    let events = w.registration_events(1);

    // when: deliveries and passes keep coming while the job runs
    let repeats: Vec<_> = (0..20).map(|_| block_on(m.step(attempt(1)))).collect();
    let passes: Vec<_> = (0..5).map(|_| block_on(m.pass())).collect();
    let while_busy = (w.api.runner_removals(), w.registration_events(1));
    w.api.finish_job(id).unwrap_or_else(|e| panic!("{e}"));
    w.h.clock.advance(RegistrationConfig::default().busy_retry);
    let done = block_on(m.step(attempt(1)));

    // then: one busy answer, nothing asked or recorded again until the backoff passed, then the
    // runner removed by its id and the attempt retired
    assert_eq!(first, Ok(Step::Waiting("runner_busy")));
    assert!(
        repeats
            .iter()
            .all(|s| *s == Ok(Step::Waiting("runner_busy"))),
        "{repeats:?}"
    );
    assert!(
        passes
            .iter()
            .all(|p| p.waiting == vec![(attempt(1), "runner_busy")]),
        "{passes:?}"
    );
    assert_eq!(while_busy, (removals, events));
    assert_eq!(done, Ok(Step::Moved));
    assert_eq!(w.api.runner_removals(), removals + 1);
    assert_eq!(w.registration(1).phase, Phase::Deregistered);
    assert_eq!(w.attempt(1).phase, AttemptPhase::Retiring);
}

/// A gate a JIT generation waits at until the test opens it.
#[derive(Default)]
struct Gate {
    open: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl Gate {
    async fn pass(&self) {
        poll_fn(|cx| {
            if self.open.load(Ordering::SeqCst) {
                return Poll::Ready(());
            }
            *self.waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;
    }

    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        if let Some(waker) = self
            .waker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            waker.wake();
        }
    }
}

/// The in-memory GitHub, whose JIT generation waits at a gate.
struct GatedApi {
    inner: FakeScaleSetApi,
    gate: Arc<Gate>,
}

impl ScaleSetApi for GatedApi {
    async fn find_scale_set(
        &self,
        spec: &ScaleSetSpec,
    ) -> Result<Option<ScaleSetInfo>, GitHubCallError> {
        self.inner.find_scale_set(spec).await
    }

    async fn get_scale_set(&self, id: ScaleSetId) -> Result<ScaleSetInfo, GitHubCallError> {
        self.inner.get_scale_set(id).await
    }

    async fn create_scale_set(&self, spec: &ScaleSetSpec) -> Result<ScaleSetInfo, GitHubCallError> {
        self.inner.create_scale_set(spec).await
    }

    async fn create_session(
        &self,
        scale_set: ScaleSetId,
        owner: &str,
    ) -> Result<OpenedSession, GitHubCallError> {
        self.inner.create_session(scale_set, owner).await
    }

    async fn delete_session(
        &self,
        scale_set: ScaleSetId,
        session: SessionId,
    ) -> Result<(), GitHubCallError> {
        self.inner.delete_session(scale_set, session).await
    }

    async fn poll(
        &self,
        scale_set: ScaleSetId,
        last: Option<MessageId>,
        max_capacity: u32,
    ) -> Result<Option<PolledMessage>, GitHubCallError> {
        self.inner.poll(scale_set, last, max_capacity).await
    }

    async fn ack(&self, scale_set: ScaleSetId, message: MessageId) -> Result<(), GitHubCallError> {
        self.inner.ack(scale_set, message).await
    }

    async fn acquire_jobs(
        &self,
        scale_set: ScaleSetId,
        requests: &BTreeSet<RunnerRequestId>,
    ) -> Result<BTreeSet<RunnerRequestId>, GitHubCallError> {
        self.inner.acquire_jobs(scale_set, requests).await
    }

    async fn acquirable_jobs(&self, scale_set: ScaleSetId) -> Result<Vec<Offer>, GitHubCallError> {
        self.inner.acquirable_jobs(scale_set).await
    }

    async fn generate_jit_config(
        &self,
        scale_set: ScaleSetId,
        runner_name: &str,
        work_folder: &str,
    ) -> Result<JitConfig, GitHubCallError> {
        self.gate.pass().await;
        self.inner
            .generate_jit_config(scale_set, runner_name, work_folder)
            .await
    }
}

#[test]
fn should_block_a_second_step_when_a_mint_is_in_flight_for_the_attempt() {
    // given: attempt 1's mint waits inside GitHub's answer, its registration requested
    let w = fixture();
    w.activating(1);
    let gate = Arc::new(Gate::default());
    let api = GatedApi {
        inner: w.api.clone(),
        gate: Arc::clone(&gate),
    };
    let m = RegistrationManager::new(
        &w.h.store,
        w.h.sessions.clone(),
        &api,
        &w.registry,
        &w.directory,
        &w.broker,
        RegistrationConfig::default(),
    );
    let mut first = pin!(m.step(attempt(1)));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(first.as_mut().poll(&mut cx).is_pending());
    assert_eq!(w.registration(1).phase, Phase::Requested);

    // when: a delivery and a pass come for the same attempt meanwhile, then GitHub answers
    let second = block_on(m.step(attempt(1)));
    let pass = block_on(m.pass());
    gate.open();
    let finished = block_on(first);

    // then: neither decided on the request in flight; the mint ends with its config held
    assert_eq!(second, Ok(Step::Blocked("stepping")));
    assert_eq!(pass.waiting, vec![(attempt(1), "stepping")], "{pass:?}");
    assert_eq!(finished, Ok(Step::Moved));
    let r = w.registration(1);
    assert_eq!(r.phase, Phase::Issued);
    assert!(r.leaked.is_none());
    let handle = handle_of(attempt(1));
    assert_eq!(w.attempt(1).jit, Jit::Issued(handle));
    assert!(w.broker.holds(handle));
}

#[test]
fn should_ask_again_at_once_and_once_only_when_the_runner_exits_during_the_busy_backoff() {
    // given: a listening runner presumed leaked (identity doubt) runs a job; GitHub answered
    // busy to its removal
    let w = fixture();
    let m = w.manager();
    let id = w.connected(&m, 1);
    w.busy(id);
    w.record(
        1,
        0x9600,
        RegistrationCommand::PresumeLeaked {
            reason: LeakReason::IdentityDoubt,
        },
    );
    let first = block_on(m.step(attempt(1)));
    let before = w.api.runner_removals();

    // when: the runner's exit is recorded, then the steps come again, without the backoff
    // passing
    w.apply(
        1,
        0x9601,
        EPOCH,
        observe(ob(1, 9), EPOCH, Fact::RunnerExited { code: 0 }),
    );
    let after_exit = block_on(m.step(attempt(1)));
    let again = block_on(m.step(attempt(1)));

    // then: one more removal at once (still busy at GitHub here), none after it
    assert_eq!(first, Ok(Step::Waiting("runner_busy")));
    assert_eq!(after_exit, Ok(Step::Waiting("runner_busy")));
    assert_eq!(again, Ok(Step::Waiting("runner_busy")));
    assert_eq!(w.api.runner_removals(), before + 1);
}
