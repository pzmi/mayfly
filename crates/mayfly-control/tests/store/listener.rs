//! The scale-set listener over a real store and the in-memory GitHub (component level, GH-04;
//! plan §7.2, `doc/github-scaleset.md` §3–§6, §9; ver §8.4 F08, F09, F10, F11, F25, F27, F42,
//! F44): each message recorded with its inbox key before it is acked, duplicates told only by
//! the cursor, the wire max read from the stream, acquisitions made exactly as journaled and asked
//! again after a lost answer, sessions lost, deleted, re-opened and re-created, and protocol
//! drift degrading the session, never more.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

use mayfly_control::delivery::Kicks;
use mayfly_control::github::{
    Failure, GitHubCallError, JitConfig, MessageId, MessageItem, Offer, OpenedSession,
    PolledMessage, RunnerRequestId, ScaleSetApi, ScaleSetId, ScaleSetInfo, ScaleSetSpec, SessionId,
    Statistics,
};
use mayfly_control::journal::{Journal as _, StreamId};
use mayfly_control::listener::{
    JobFacts, Listener, ListenerConfig, ListenerError, Polled, Recorded, Tick, gap_of, inbox_key,
};
use mayfly_control::store::{Clock as _, Outcome, SaveRequest, decode_event};
use mayfly_control_domain::demand::{Capacity, RunLookup, WireMode};
use mayfly_control_domain::pool::TrustTier;
use mayfly_control_domain::scaleset::{
    DegradedCause, LossCause, MessageOrigin, MessageRecorded, Phase, ScaleSet, ScaleSetCommand,
    ScaleSetEvent, ScaleSetRejection, ScaleSetState,
};
use mayfly_control_domain::{Actor, Command};
use mayfly_domain::{CommandId, Epoch, Millis, PoolName, Sample};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::contracts::journal::InjectedFault;
use mayfly_testkit::contracts::scaleset::{Flow, WorldFault};
use mayfly_testkit::github::{DEFAULT_GROUP, FIRST_MESSAGE_ID, FakeScaleSetApi};
use mayfly_testkit::journal::MemoryJournal;

use crate::attempts::{EPOCH, Harness, NOW, Store, harness};

const LABEL: &str = "mayfly";

type L<'s, A> = Listener<'s, MemoryJournal, FakeClock, SeededRng, Kicks, A>;

struct Bench {
    h: Harness,
    api: FakeScaleSetApi,
}

fn bench(flow: Flow) -> Bench {
    let h = harness();
    let api = FakeScaleSetApi::new(flow, h.clock.clone());
    Bench { h, api }
}

fn pool() -> PoolName {
    PoolName::sample()
}

fn spec() -> ScaleSetSpec {
    ScaleSetSpec {
        name: "mayfly".to_owned(),
        runner_group: DEFAULT_GROUP.to_owned(),
        labels: vec![LABEL.to_owned()],
    }
}

fn config() -> ListenerConfig {
    ListenerConfig::new(
        pool(),
        spec(),
        TrustTier::Trusted,
        "host0/pool0".to_owned(),
        EPOCH,
    )
}

async fn save_scale_set(
    store: &Store,
    id: u128,
    body: ScaleSetCommand,
) -> Outcome<ScaleSetRejection> {
    let command = Command::new(
        CommandId::from_u128(id),
        NOW,
        EPOCH,
        Actor::Automation {
            policy: "reconciler",
        },
        body,
    );
    store
        .execute::<ScaleSet>(SaveRequest::new(pool(), command))
        .await
        .unwrap_or_else(|e| panic!("{e}"))
}

fn tick<A: ScaleSetApi>(l: &L<'_, A>) -> Tick {
    block_on(l.tick()).unwrap_or_else(|e| panic!("{e}"))
}

fn polled(t: Tick) -> Polled {
    match t {
        Tick::Polled(p) => p,
        other => panic!("not a poll: {other:?}"),
    }
}

fn accepted() -> Option<Recorded> {
    Some(Recorded::Accepted {
        replayed: false,
        anew: false,
    })
}

impl Bench {
    fn listener(&self) -> L<'_, FakeScaleSetApi> {
        Listener::new(&self.h.store, &self.api, config())
    }

    fn state(&self) -> ScaleSetState {
        block_on(self.h.store.load::<ScaleSet>(pool().as_str())).unwrap_or_else(|e| panic!("{e}"))
    }

    fn set(&self) -> ScaleSetId {
        self.state()
            .github_id
            .unwrap_or_else(|| panic!("the scale set is ensured"))
    }

    fn command(&self, id: u128, body: ScaleSetCommand) -> Outcome<ScaleSetRejection> {
        block_on(save_scale_set(&self.h.store, id, body))
    }

    /// The capacity policy's input, as the reconciler applies it.
    fn capacity(&self, revision: u64, deliverable: u32, reported_max: u32, mode: WireMode) {
        let body = ScaleSetCommand::ApplyCapacity(Capacity {
            revision,
            deliverable,
            reported_max,
            mode,
        });
        let outcome = self.command(0xCA00 + u128::from(revision), body);
        assert!(matches!(outcome, Outcome::Accepted { .. }), "{outcome:?}");
    }

    /// Ensures the scale set and opens a session.
    fn live<A: ScaleSetApi>(&self, l: &L<'_, A>) -> SessionId {
        assert!(matches!(tick(l), Tick::Ensured(..)));
        match tick(l) {
            Tick::Opened(session) => session,
            other => panic!("not opened: {other:?}"),
        }
    }

    fn events(&self) -> Vec<ScaleSetEvent> {
        let stream =
            StreamId::new(&format!("scaleset-{}", pool())).unwrap_or_else(|e| panic!("{e}"));
        block_on(self.h.journal.read_stream(&stream, None, None))
            .unwrap_or_else(|e| panic!("{e}"))
            .iter()
            .map(|e| decode_event::<ScaleSetEvent>(e).unwrap_or_else(|e| panic!("{e:?}")))
            .collect()
    }

    fn recorded(&self) -> Vec<MessageRecorded> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                ScaleSetEvent::MessageRecorded(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    fn statistics(&self, assigned: u32, running: u32) -> Statistics {
        Statistics {
            total_assigned_jobs: assigned,
            total_running_jobs: running,
            ..self.api.statistics(self.set())
        }
    }
}

/// The in-memory GitHub, saving a command on the scale set's stream right before the next poll
/// answers (a write that races the listener between its load and its record), or failing the
/// next call of a named operation with a given class, before it reaches the world.
struct Hooked<'a> {
    api: FakeScaleSetApi,
    store: &'a Store,
    before_poll: Mutex<Option<ScaleSetCommand>>,
    fail: Mutex<BTreeMap<&'static str, Failure>>,
    always: Mutex<BTreeMap<&'static str, Failure>>,
    calls: Mutex<BTreeMap<&'static str, u32>>,
}

impl<'a> Hooked<'a> {
    fn new(api: FakeScaleSetApi, store: &'a Store) -> Self {
        Self {
            api,
            store,
            before_poll: Mutex::new(None),
            fail: Mutex::new(BTreeMap::new()),
            always: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(BTreeMap::new()),
        }
    }

    /// Every call of `operation` answers `failure` from now on.
    fn fail_always(&self, operation: &'static str, failure: Failure) {
        self.always
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(operation, failure);
    }

    /// How many calls of `operation` were made.
    fn calls(&self, operation: &str) -> u32 {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(operation)
            .copied()
            .unwrap_or(0)
    }

    /// The next call of `operation` answers `failure`.
    fn fail_next(&self, operation: &'static str, failure: Failure) {
        self.fail
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(operation, failure);
    }

    fn failed(&self, operation: &'static str) -> Result<(), GitHubCallError> {
        *self
            .calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(operation)
            .or_insert(0) += 1;
        let once = self
            .fail
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(operation);
        let always = self
            .always
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(operation)
            .copied();
        once.or(always)
            .map_or(Ok(()), |f| Err(GitHubCallError::new(f, "injected")))
    }
}

impl ScaleSetApi for Hooked<'_> {
    async fn find_scale_set(
        &self,
        spec: &ScaleSetSpec,
    ) -> Result<Option<ScaleSetInfo>, GitHubCallError> {
        self.failed("find")?;
        self.api.find_scale_set(spec).await
    }

    async fn get_scale_set(&self, id: ScaleSetId) -> Result<ScaleSetInfo, GitHubCallError> {
        self.api.get_scale_set(id).await
    }

    async fn create_scale_set(&self, spec: &ScaleSetSpec) -> Result<ScaleSetInfo, GitHubCallError> {
        self.api.create_scale_set(spec).await
    }

    async fn create_session(
        &self,
        scale_set: ScaleSetId,
        owner: &str,
    ) -> Result<OpenedSession, GitHubCallError> {
        self.api.create_session(scale_set, owner).await
    }

    async fn delete_session(
        &self,
        scale_set: ScaleSetId,
        session: SessionId,
    ) -> Result<(), GitHubCallError> {
        self.failed("delete")?;
        self.api.delete_session(scale_set, session).await
    }

    async fn poll(
        &self,
        scale_set: ScaleSetId,
        last: Option<MessageId>,
        max_capacity: u32,
    ) -> Result<Option<PolledMessage>, GitHubCallError> {
        let hook = self
            .before_poll
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(body) = hook {
            save_scale_set(self.store, 0xE500, body).await;
        }
        self.failed("poll")?;
        self.api.poll(scale_set, last, max_capacity).await
    }

    async fn ack(&self, scale_set: ScaleSetId, message: MessageId) -> Result<(), GitHubCallError> {
        self.failed("ack")?;
        self.api.ack(scale_set, message).await
    }

    async fn acquire_jobs(
        &self,
        scale_set: ScaleSetId,
        requests: &BTreeSet<RunnerRequestId>,
    ) -> Result<BTreeSet<RunnerRequestId>, GitHubCallError> {
        self.failed("acquire")?;
        self.api.acquire_jobs(scale_set, requests).await
    }

    async fn acquirable_jobs(&self, scale_set: ScaleSetId) -> Result<Vec<Offer>, GitHubCallError> {
        self.api.acquirable_jobs(scale_set).await
    }

    async fn generate_jit_config(
        &self,
        scale_set: ScaleSetId,
        runner_name: &str,
        work_folder: &str,
    ) -> Result<JitConfig, GitHubCallError> {
        self.api
            .generate_jit_config(scale_set, runner_name, work_folder)
            .await
    }
}

#[test]
fn should_record_ack_and_start_no_runner_when_an_assigned_job_is_pending_or_refused() {
    // given: github.com assigns a job directly
    let w = bench(Flow::Direct);
    let l = w.listener();
    w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);

    // when
    let p = polled(tick(&l));

    // then: recorded, then acked; the job waits for its run's lookup, so no runner may start
    assert_eq!(p.recorded, accepted());
    assert!(p.acked);
    assert!(w.api.unacked(w.set()).is_empty());
    let s = w.state();
    assert_eq!(s.pending.len(), 1);
    assert!(!s.runner_may_start(0));

    // and when its lookup fails, it is refused and still starts no runner
    let job = s
        .pending
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| panic!("a pending job"));
    let lookup = ScaleSetCommand::RecordRunLookup {
        job: job.clone(),
        lookup: RunLookup::Failed,
    };
    assert!(matches!(w.command(0x77, lookup), Outcome::Accepted { .. }));
    let s = w.state();
    assert!(s.refused.contains(&job));
    assert!(!s.runner_may_start(0));
}

#[test]
fn should_ack_only_after_the_record_commits_when_the_store_fails() {
    // given
    let w = bench(Flow::Direct);
    let l = w.listener();
    w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    assert!(w.h.journal.inject(InjectedFault::StorageBeforeCommit));

    // when
    let failed = block_on(l.tick());
    let unacked = w.api.unacked(w.set()).len();
    let p = polled(tick(&l));

    // then: no ack without the record; the redelivery records it, once
    assert!(failed.is_err());
    assert_eq!(unacked, 1);
    assert_eq!(p.recorded, accepted());
    assert!(p.acked);
    assert_eq!(w.recorded().len(), 1);
    assert!(w.api.unacked(w.set()).is_empty());
}

#[test]
fn should_keep_the_session_and_lose_no_message_when_a_poll_connection_drops() {
    // given: F08
    let w = bench(Flow::Direct);
    let l = w.listener();
    let session = w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    w.api.drop_next_poll();

    // when
    let dropped = polled(tick(&l));
    let waiting = tick(&l);
    w.h.clock.advance(Millis::new(30_000));
    let next = polled(tick(&l));

    // then
    assert_eq!(dropped.poll_failure, Some("unavailable"));
    assert!(matches!(waiting, Tick::Waiting(..)));
    assert_eq!(dropped.message, None);
    assert_eq!(next.recorded, accepted());
    assert!(next.acked);
    let s = w.state();
    assert_eq!((s.phase, s.session), (Phase::Active, Some(session)));
    assert_eq!(s.desired, 1, "min(deliverable 2, assigned 1)");
}

#[test]
fn should_retry_the_ack_and_reserve_once_when_the_ack_answer_is_lost() {
    // given: F09, GHES offers
    let w = bench(Flow::Acquire);
    let l = w.listener();
    w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    w.api
        .inject(WorldFault::AckAnswerLost { scale_set: w.set() });

    // when
    let first = polled(tick(&l));
    w.h.clock.advance(Millis::new(30_000));
    let second = polled(tick(&l));

    // then: the ack is asked again before the next poll, the job reserved once
    assert_eq!(first.recorded, accepted());
    assert!(!first.acked);
    assert_eq!(first.acquired.as_ref().map(BTreeSet::len), Some(1));
    assert_eq!(second.message, None);
    assert!(w.api.unacked(w.set()).is_empty());
    let s = w.state();
    assert_eq!(s.acquired.len(), 1);
    assert!(s.acquiring.is_empty());
    assert_eq!(s.waiting(), 1);
}

#[test]
fn should_ack_without_a_write_when_the_cursor_message_comes_again() {
    // given: F10, the cursor's message recorded, then delivered again
    let w = bench(Flow::Acquire);
    let l = w.listener();
    w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    let first = polled(tick(&l));
    let cursor = first.message.unwrap_or_else(|| panic!("a message"));
    let offer = MessageItem::Offer(Offer {
        request_id: first.acquire[0],
        event_name: mayfly_control::github::EventName::Push,
    });
    let stats = w.api.statistics(w.set());
    w.api
        .script_message(w.set(), cursor.0, vec![offer], Some(stats));
    let events = w.events().len();

    // when
    let again = polled(tick(&l));

    // then: a duplicate, acked, nothing written, nothing reserved twice
    assert_eq!(again.message, Some(cursor));
    assert_eq!(again.recorded, Some(Recorded::Duplicate));
    assert!(again.acked);
    assert_eq!(w.events().len(), events);
    assert_eq!(w.state().reserved().len(), 1);
}

#[test]
fn should_record_a_reused_id_anew_and_degrade_when_its_receipt_exists() {
    // given: KER-06G, two messages recorded in the session, then the first id comes again
    let w = bench(Flow::Direct);
    let l = w.listener();
    w.live(&l);
    w.capacity(1, 4, 4, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    polled(tick(&l));
    w.api.queue_jobs(1, LABEL, 43);
    polled(tick(&l));
    let stats = w.api.statistics(w.set());
    w.api.script_message(
        w.set(),
        FIRST_MESSAGE_ID,
        vec![MessageItem::Unknown],
        Some(stats),
    );
    let pending = w.state().pending.len();

    // when
    let reused = polled(tick(&l));

    // then: recorded under its own command id (not dropped by the receipt), applying nothing
    assert_eq!(
        reused.recorded,
        Some(Recorded::Accepted {
            replayed: false,
            anew: true
        })
    );
    assert!(reused.acked);
    let s = w.state();
    assert_eq!(s.phase, Phase::Degraded(DegradedCause::MessageIdsRegressed));
    assert_eq!(s.pending.len(), pending);
    assert_eq!(s.drift, 0, "a reused id's items apply nothing");
    let last = w.recorded().pop().unwrap_or_else(|| panic!("recorded"));
    assert_eq!(last.origin, MessageOrigin::IdRegressed);
}

#[test]
fn should_apply_nothing_and_degrade_when_an_earlier_id_comes_out_of_order() {
    // given: F10, an id below the cursor that was never recorded
    let w = bench(Flow::Direct);
    let l = w.listener();
    w.live(&l);
    w.capacity(1, 4, 4, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    polled(tick(&l));
    let stats = w.statistics(9, 0);
    w.api
        .script_message(w.set(), FIRST_MESSAGE_ID - 3, vec![], Some(stats));

    // when
    let late = polled(tick(&l));

    // then: recorded so it can be acked, its statistics ignored, the session degraded
    assert_eq!(late.recorded, accepted());
    assert!(late.acked);
    let s = w.state();
    assert_eq!(s.phase, Phase::Degraded(DegradedCause::MessageIdsRegressed));
    assert_eq!(s.statistics.map(|s| s.total_assigned_jobs), Some(1));
}

#[test]
fn should_send_at_most_the_deliverable_and_take_demand_from_statistics_when_a_backlog_is_large() {
    // given: F11, 100 queued jobs and 2 deliverable runners
    let w = bench(Flow::Acquire);
    let l = w.listener();
    w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(100, LABEL, 42);

    // when
    let mut sent = Vec::new();
    for _ in 0..5 {
        polled(tick(&l));
        sent.push(w.api.max_capacity(w.set()));
    }

    // then
    assert!(sent.iter().all(|max| *max == 2), "{sent:?}");
    let s = w.state();
    assert_eq!(s.reserved().len(), 2);
    assert!(s.desired <= 2);
}

#[test]
fn should_wait_out_retry_after_and_keep_the_wire_max_when_a_poll_is_rate_limited() {
    // given: F25
    let w = bench(Flow::Direct);
    let l = w.listener();
    w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    w.api.inject(WorldFault::PollRateLimited {
        scale_set: w.set(),
        retry_after_secs: 30,
    });
    let polls = w.api.polls();

    // when
    let p = polled(tick(&l));

    // then: one request, answered after the delay, the capacity not raised
    assert_eq!(w.api.polls(), polls + 1);
    assert!(w.h.clock.sleeps().contains(&Millis::new(30_000)));
    assert_eq!(w.api.max_capacity(w.set()), 2);
    assert_eq!(p.recorded, accepted());
    assert!(p.acked);
}

#[test]
fn should_acquire_again_and_serve_the_job_once_when_the_acquire_answer_was_lost() {
    // given: F27
    let w = bench(Flow::Acquire);
    let l = w.listener();
    w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    w.api.lose_next_acquire_answer();

    // when
    let lost = polled(tick(&l));
    let pending = w.state().acquiring.len();
    w.h.clock.advance(Millis::new(30_000));
    polled(tick(&l));

    // then: pending until an answer names it, then acquired once
    assert_eq!(lost.acquire.len(), 1);
    assert_eq!(lost.acquired, None);
    assert_eq!(pending, 1);
    let s = w.state();
    assert!(s.acquiring.is_empty());
    assert_eq!(s.acquired.len(), 1);
    assert_eq!(s.waiting(), 1);
}

#[test]
fn should_delete_the_journaled_session_and_reopen_when_the_daemon_restarted() {
    // given: F42, a message recorded and acked, then the daemon restarts within the session's
    // life: GitHub keeps the old session
    let w = bench(Flow::Direct);
    let l = w.listener();
    let old = w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    polled(tick(&l));
    drop(l);
    w.api.restart_client();
    let l = w.listener();

    // when
    let lost = tick(&l);
    let deleted = tick(&l);
    let opened = tick(&l);

    // then: the journaled session deleted before a new create, desired from the new opening
    assert_eq!(lost, Tick::SessionLost(LossCause::Restarted));
    assert_eq!(deleted, Tick::StaleDeleted(old));
    let Tick::Opened(new) = opened else {
        panic!("not opened: {opened:?}");
    };
    assert_ne!(new, old);
    let s = w.state();
    assert_eq!(s.phase, Phase::Active);
    assert_eq!(s.desired, 1);
    assert_eq!(s.pending.len(), 1, "the job is held once");
}

#[test]
fn should_back_off_and_open_after_the_wait_when_a_create_answers_conflict() {
    // given: F42, a session mayfly has no record of (a create whose answer was lost)
    let w = bench(Flow::Direct);
    let l = w.listener();
    assert!(matches!(tick(&l), Tick::Ensured(..)));
    let unknown = block_on(w.api.create_session(w.set(), "lost"))
        .unwrap_or_else(|e| panic!("{e}"))
        .session_id;

    // when
    let conflict = tick(&l);
    let waiting = tick(&l);
    block_on(w.api.delete_session(w.set(), unknown)).unwrap_or_else(|e| panic!("{e}"));
    w.h.clock.advance(Millis::new(30_000));
    let opened = tick(&l);

    // then: 409 backs off, never deletes the unknown session, and opens after the wait
    assert_eq!(conflict, Tick::OpenFailed("session_conflict"));
    assert_eq!(waiting, Tick::Waiting(Millis::new(NOW.get() + 30_000)));
    assert!(matches!(opened, Tick::Opened(..)));
}

#[test]
fn should_recreate_the_scale_set_when_a_create_answers_gone() {
    // given: GitHub deleted the scale set as dormant
    let w = bench(Flow::Direct);
    let l = w.listener();
    assert!(matches!(tick(&l), Tick::Ensured(..)));
    let old = w.set();
    w.api.inject(WorldFault::ScaleSetDeleted { scale_set: old });

    // when
    let gone = tick(&l);
    let vanished = w.state().vanished();
    w.h.clock.advance(Millis::new(30_000));
    let ensured = tick(&l);
    let opened = tick(&l);

    // then: the 404 returns the stream to unconfigured; ensure re-creates the set
    assert_eq!(gone, Tick::OpenFailed("scale_set_gone"));
    assert!(vanished);
    let Tick::Ensured(new) = ensured else {
        panic!("not ensured: {ensured:?}");
    };
    assert_ne!(new, old);
    assert_eq!(w.set(), new);
    assert!(matches!(opened, Tick::Opened(..)));
}

#[test]
fn should_journal_no_session_when_the_create_answer_is_unusable() {
    // given: a create whose queue URL a token could leak through (GH-03: the client deletes it)
    let w = bench(Flow::Direct);
    let l = w.listener();
    assert!(matches!(tick(&l), Tick::Ensured(..)));
    w.api
        .inject(WorldFault::QueueUrlUnusable { scale_set: w.set() });

    // when
    let failed = tick(&l);

    // then
    assert_eq!(failed, Tick::OpenFailed("protocol_drift"));
    let s = w.state();
    assert_eq!(s.phase, Phase::Backoff(LossCause::Failed));
    assert_eq!((s.session, s.stale_session), (None, None));
    assert_eq!(w.api.live_session(w.set()), None);
}

#[test]
fn should_restart_the_numbering_and_keep_the_session_when_a_new_session_starts_below_the_floor() {
    // given: a message recorded, the session lost and opened again
    let w = bench(Flow::Direct);
    let l = w.listener();
    w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    polled(tick(&l));
    w.api
        .inject(WorldFault::SessionExpired { scale_set: w.set() });
    assert_eq!(tick(&l), Tick::SessionLost(LossCause::Expired));
    assert!(matches!(tick(&l), Tick::StaleDeleted(..)));
    assert!(matches!(tick(&l), Tick::Opened(..)));
    let stats = w.api.statistics(w.set());
    w.api.script_message(w.set(), 7, vec![], Some(stats));

    // when
    let p = polled(tick(&l));

    // then
    assert_eq!(p.recorded, accepted());
    let s = w.state();
    assert_eq!(s.phase, Phase::Active);
    assert!(s.renumbered);
    assert_eq!(s.cursor, Some(MessageId(7)));
    assert!(
        w.events()
            .iter()
            .any(|e| matches!(e, ScaleSetEvent::MessageIdsRestarted(..)))
    );
}

#[test]
fn should_degrade_but_keep_recording_when_unknown_types_reach_the_threshold() {
    // given: F44
    let w = bench(Flow::Direct);
    let l = w.listener();
    w.live(&l);
    let stats = w.api.statistics(w.set());
    for n in 0..3 {
        w.api.script_message(
            w.set(),
            FIRST_MESSAGE_ID + n,
            vec![MessageItem::Unknown],
            Some(stats),
        );
    }

    // when
    let all: Vec<Polled> = (0..3).map(|_| polled(tick(&l))).collect();

    // then: every message recorded and acked; degraded at the threshold, never more
    assert!(all.iter().all(|p| p.recorded == accepted() && p.acked));
    let s = w.state();
    assert_eq!(s.phase, Phase::Degraded(DegradedCause::ProtocolDrift));
    assert_eq!(s.drift, 3);
}

#[test]
fn should_count_drift_and_keep_the_statistics_when_a_message_has_none() {
    // given: a message with statistics, then one whose envelope lacked a count
    let w = bench(Flow::Direct);
    let l = w.listener();
    w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    polled(tick(&l));
    let before = w.state().statistics;
    w.api
        .script_message(w.set(), FIRST_MESSAGE_ID + 1, vec![], None);

    // when
    let p = polled(tick(&l));

    // then
    assert_eq!(p.recorded, accepted());
    assert!(p.acked);
    let s = w.state();
    assert_eq!(
        s.statistics, before,
        "an older count is never replaced by none"
    );
    assert_eq!(s.drift, 1);
    let last = w.recorded().pop().unwrap_or_else(|| panic!("recorded"));
    assert_eq!((last.statistics, last.unknown_types), (None, 1));
}

#[test]
fn should_send_the_streams_wire_max_when_a_sink_pins_it() {
    // given: KER-06G, a refused job and its refusal sink up
    let w = bench(Flow::Direct);
    let l = w.listener();
    w.live(&l);
    w.capacity(1, 3, 3, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    polled(tick(&l));
    let job = w
        .state()
        .pending
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| panic!("a pending job"));
    let lookup = ScaleSetCommand::RecordRunLookup {
        job: job.clone(),
        lookup: RunLookup::Failed,
    };
    assert!(matches!(w.command(0x71, lookup), Outcome::Accepted { .. }));
    let sink = ScaleSetCommand::StartSink { job };
    assert!(matches!(w.command(0x72, sink), Outcome::Accepted { .. }));

    // when
    polled(tick(&l));

    // then: the pin (the assigned count, 1), not the applied capacity's snapshot (3)
    let snapshot = w.events().iter().find_map(|e| match e {
        ScaleSetEvent::CapacityApplied(c) => Some(c.wire_max),
        _ => None,
    });
    assert_eq!(snapshot, Some(3));
    assert_eq!(w.state().wire_max(), 1);
    assert_eq!(w.api.max_capacity(w.set()), 1);
}

#[test]
fn should_keep_the_session_and_send_the_mode_wire_max_when_paused_draining_or_in_safe_mode() {
    // given: ADR 0029
    let w = bench(Flow::Acquire);
    let l = w.listener();
    let session = w.live(&l);
    let modes = [
        (WireMode::Paused, 2),
        (WireMode::Draining, 0),
        (WireMode::SafeMode, 0),
        (WireMode::Open, 2),
    ];

    // when
    let sent: Vec<(u32, Phase)> = (1_u64..)
        .zip(modes)
        .map(|(revision, (mode, _))| {
            w.capacity(revision, 3, 2, mode);
            polled(tick(&l));
            (w.api.max_capacity(w.set()), w.state().phase)
        })
        .collect();

    // then
    let expected: Vec<(u32, Phase)> = modes.iter().map(|(_, max)| (*max, Phase::Active)).collect();
    assert_eq!(sent, expected);
    assert_eq!(w.state().session, Some(session));
}

#[test]
fn should_leave_a_refused_message_unacked_and_record_it_in_the_next_session_when_the_session_was_lost()
 {
    // given: the session's loss committed between the listener's load and its record
    let w = bench(Flow::Direct);
    let hooked = Hooked::new(w.api.clone(), &w.h.store);
    let l: L<'_, Hooked<'_>> = Listener::new(&w.h.store, &hooked, config());
    let session = w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    *hooked
        .before_poll
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(ScaleSetCommand::RecordSessionLoss {
        cause: LossCause::Failed,
    });

    // when
    let refused = polled(tick(&l));
    let message = refused.message.unwrap_or_else(|| panic!("a message"));
    let key = inbox_key(session, message, None).unwrap_or_else(|e| panic!("{e}"));
    let consumed = block_on(w.h.journal.inbox_contains(&key)).unwrap_or_else(|e| panic!("{e}"));
    let unacked = w.api.unacked(w.set());
    assert_eq!(tick(&l), Tick::Waiting(Millis::new(NOW.get() + 30_000)));
    w.h.clock.advance(Millis::new(30_000));
    assert_eq!(tick(&l), Tick::StaleDeleted(session));
    assert!(matches!(tick(&l), Tick::Opened(..)));
    let again = polled(tick(&l));

    // then: refused, not acked, its key not consumed; the next session records and acks it
    assert_eq!(refused.recorded, Some(Recorded::Refused("no_session")));
    assert!(!refused.acked);
    assert!(!consumed);
    assert_eq!(unacked, vec![message.0]);
    assert_eq!(again.message, Some(message));
    assert_eq!(again.recorded, accepted());
    assert!(again.acked);
    assert_eq!(w.recorded().len(), 1);
    assert!(w.api.unacked(w.set()).is_empty());
}

#[test]
fn should_stop_fenced_when_a_newer_run_owns_the_stream() {
    // given: a newer run's command on the stream, then a message for this listener
    let w = bench(Flow::Direct);
    let l = w.listener();
    w.live(&l);
    let newer = Command::new(
        CommandId::from_u128(0xE600),
        NOW,
        Epoch::new(EPOCH.get() + 1),
        Actor::Automation {
            policy: "reconciler",
        },
        ScaleSetCommand::ApplyCapacity(Capacity {
            revision: 1,
            deliverable: 2,
            reported_max: 2,
            mode: WireMode::Open,
        }),
    );
    block_on(
        w.h.store
            .execute::<ScaleSet>(SaveRequest::new(pool(), newer)),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    w.api.queue_jobs(1, LABEL, 42);

    // when
    let fenced = block_on(l.tick());

    // then: the message is left for the newer run
    assert_eq!(fenced, Err(ListenerError::Fenced));
    assert_eq!(w.api.unacked(w.set()).len(), 1);
    assert!(w.recorded().is_empty());
}

#[test]
fn should_ack_once_the_record_is_resolved_when_its_commit_reply_is_lost() {
    // given: the unit commits and its reply is lost
    let w = bench(Flow::Direct);
    let l = w.listener();
    w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    assert!(w.h.journal.inject(InjectedFault::ReplyLostAfterCommit));

    // when
    let p = polled(tick(&l));

    // then: the store finds its own unit, and the message is acked once
    assert!(matches!(p.recorded, Some(Recorded::Accepted { .. })));
    assert!(p.acked);
    assert_eq!(w.recorded().len(), 1);
}

#[test]
fn should_kick_the_reconciler_when_a_poll_is_empty() {
    // given
    let w = bench(Flow::Direct);
    let kicks = Kicks::new(8);
    let l = w.listener().with_kicks(kicks.clone());
    w.live(&l);
    let _earlier = kicks.take();

    // when
    let p = polled(tick(&l));

    // then
    assert_eq!(p.message, None);
    let kicked: Vec<String> = kicks.take().iter().map(|s| s.as_str().to_owned()).collect();
    assert_eq!(kicked, vec![format!("scaleset-{}", pool())]);
}

#[test]
fn should_alarm_once_when_the_assigned_gap_persists() {
    // given: GitHub counts five assigned jobs mayfly knows nothing of (spec §3.4)
    let w = bench(Flow::Direct);
    let l = w.listener();
    w.live(&l);
    let stats = w.statistics(5, 0);
    w.api
        .script_message(w.set(), FIRST_MESSAGE_ID, vec![], Some(stats));
    let first = polled(tick(&l));
    let alarmed_at_once = l.gap_alarmed();

    // when
    w.h.clock.advance(Millis::new(900_000));
    let later = polled(tick(&l));

    // then: reported every poll, alarmed once it persisted, and the scale set kept
    assert_eq!(first.gap, 5);
    assert!(!alarmed_at_once);
    assert_eq!(later.gap, 5);
    assert!(l.gap_alarmed());
    assert_eq!(w.state().phase, Phase::Active);
}

#[test]
fn should_delete_the_session_in_order_when_closed() {
    // given
    let w = bench(Flow::Direct);
    let l = w.listener();
    w.live(&l);

    // when
    let closed = block_on(l.close()).unwrap_or_else(|e| panic!("{e}"));

    // then
    assert_eq!(closed, Tick::Closed);
    let s = w.state();
    assert_eq!((s.phase, s.session), (Phase::Closed, None));
    assert_eq!(w.api.live_session(w.set()), None);
}

#[test]
fn should_wait_before_ensuring_again_when_the_credentials_fail() {
    // given
    let w = bench(Flow::Direct);
    let hooked = Hooked::new(w.api.clone(), &w.h.store);
    let l: L<'_, Hooked<'_>> = Listener::new(&w.h.store, &hooked, config());
    hooked.fail_next("find", Failure::Credentials);

    // when
    let failed = tick(&l);
    let waiting = tick(&l);
    w.h.clock.advance(Millis::new(30_000));
    let ensured = tick(&l);

    // then: nothing changed, and no call before the backoff passed
    assert_eq!(failed, Tick::Failed("credentials"));
    assert_eq!(waiting, Tick::Waiting(Millis::new(NOW.get() + 30_000)));
    assert!(matches!(ensured, Tick::Ensured(..)));
}

#[test]
fn should_keep_the_stale_session_journaled_when_its_delete_fails() {
    // given: a restart left the journaled session stale
    let w = bench(Flow::Direct);
    let hooked = Hooked::new(w.api.clone(), &w.h.store);
    let l: L<'_, Hooked<'_>> = Listener::new(&w.h.store, &hooked, config());
    let session = w.live(&l);
    hooked.fail_next("poll", Failure::NoSession);
    assert_eq!(tick(&l), Tick::SessionLost(LossCause::Restarted));
    hooked.fail_next("delete", Failure::Unavailable);

    // when
    let failed = tick(&l);

    // then: nothing is created while the stale session is journaled (A3), and the delete is
    // asked again after the backoff
    assert_eq!(failed, Tick::Failed("unavailable"));
    assert_eq!(w.state().stale_session, Some(session));
    assert_eq!(tick(&l), Tick::Waiting(Millis::new(NOW.get() + 60_000)));
    w.h.clock.advance(Millis::new(60_000));
    assert_eq!(tick(&l), Tick::StaleDeleted(session));
}

#[test]
fn should_wait_before_polling_again_when_a_poll_is_throttled() {
    // given: a throttle the client could not outlast within its deadline
    let w = bench(Flow::Direct);
    let hooked = Hooked::new(w.api.clone(), &w.h.store);
    let l: L<'_, Hooked<'_>> = Listener::new(&w.h.store, &hooked, config());
    let session = w.live(&l);
    hooked.fail_next("poll", Failure::Throttled);

    // when
    let throttled = polled(tick(&l));
    let waiting = tick(&l);
    let polls_while_waiting = w.api.polls();
    w.h.clock.advance(Millis::new(30_000));
    let next = polled(tick(&l));

    // then: the session kept, no request until the wait passed, then one
    assert_eq!(throttled.poll_failure, Some("throttled"));
    assert_eq!(waiting, Tick::Waiting(Millis::new(NOW.get() + 30_000)));
    assert_eq!(polls_while_waiting, 0);
    assert_eq!(next.poll_failure, None);
    assert_eq!(w.api.polls(), 1);
    assert_eq!(w.state().session, Some(session));
}

#[test]
fn should_reopen_at_most_once_per_backoff_when_every_poll_is_refused() {
    // given: every poll answers a definite refusal
    let w = bench(Flow::Direct);
    let hooked = Hooked::new(w.api.clone(), &w.h.store);
    let l: L<'_, Hooked<'_>> = Listener::new(&w.h.store, &hooked, config());
    w.live(&l);
    let mut waits = Vec::new();

    // when: two rounds of refused poll, wait, stale delete and reopen
    for _ in 0..2 {
        hooked.fail_next("poll", Failure::Refused { status: 403 });
        assert_eq!(tick(&l), Tick::SessionLost(LossCause::Failed));
        let Tick::Waiting(until) = tick(&l) else {
            panic!("no wait after the loss");
        };
        waits.push(until.get() - w.h.clock.now().get());
        w.h.clock
            .advance(Millis::new(until.get() - w.h.clock.now().get()));
        assert!(matches!(tick(&l), Tick::StaleDeleted(..)));
        assert!(matches!(tick(&l), Tick::Opened(..)));
    }

    // then: the wait doubles, since no poll answered in between
    assert_eq!(waits, vec![30_000, 60_000]);
}

#[test]
fn should_lose_the_session_when_an_ack_finds_it_gone() {
    // given
    let w = bench(Flow::Direct);
    let hooked = Hooked::new(w.api.clone(), &w.h.store);
    let l: L<'_, Hooked<'_>> = Listener::new(&w.h.store, &hooked, config());
    w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    hooked.fail_next("ack", Failure::SessionExpired);

    // when
    let lost = tick(&l);

    // then: recorded first, then the loss; the message is redelivered to the next session
    assert_eq!(lost, Tick::SessionLost(LossCause::Expired));
    assert_eq!(w.recorded().len(), 1);
    assert_eq!(w.api.unacked(w.set()).len(), 1);
}

#[test]
fn should_keep_the_ack_pending_when_it_fails_without_an_answer() {
    // given
    let w = bench(Flow::Direct);
    let hooked = Hooked::new(w.api.clone(), &w.h.store);
    let l: L<'_, Hooked<'_>> = Listener::new(&w.h.store, &hooked, config());
    w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    hooked.fail_next("ack", Failure::Unavailable);

    // when
    let first = polled(tick(&l));
    let waiting = tick(&l);
    w.h.clock.advance(Millis::new(30_000));
    let second = polled(tick(&l));

    // then
    assert!(!first.acked);
    assert!(matches!(waiting, Tick::Waiting(..)));
    assert_eq!(second.message, None);
    assert!(w.api.unacked(w.set()).is_empty());
    assert_eq!(w.recorded().len(), 1);
}

#[test]
fn should_release_the_batch_when_an_acquire_is_refused_and_lose_the_session_when_it_is_gone() {
    // given: GHES offers
    let w = bench(Flow::Acquire);
    let hooked = Hooked::new(w.api.clone(), &w.h.store);
    let l: L<'_, Hooked<'_>> = Listener::new(&w.h.store, &hooked, config());
    w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    hooked.fail_next("acquire", Failure::Refused { status: 400 });

    // when: a 400 is a definite refusal of the batch
    let refused = polled(tick(&l));
    let after_refusal = w.state();
    w.api.queue_jobs(1, LABEL, 43);
    hooked.fail_next("acquire", Failure::NoSession);
    let lost = tick(&l);

    // then
    assert_eq!(refused.acquired, Some(BTreeSet::new()));
    assert!(after_refusal.acquiring.is_empty() && after_refusal.acquired.is_empty());
    assert_eq!(lost, Tick::SessionLost(LossCause::Restarted));
}

#[test]
fn should_keep_acquisitions_pending_when_the_session_is_found_gone_on_the_retry() {
    // given: an acquire whose answer was lost, then the session is found gone on the retry
    let w = bench(Flow::Acquire);
    let hooked = Hooked::new(w.api.clone(), &w.h.store);
    let l: L<'_, Hooked<'_>> = Listener::new(&w.h.store, &hooked, config());
    w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    w.api.queue_jobs(1, LABEL, 42);
    hooked.fail_next("acquire", Failure::Unavailable);
    polled(tick(&l));
    hooked.fail_next("acquire", Failure::SessionExpired);
    w.h.clock.advance(Millis::new(30_000));

    // when
    let lost = tick(&l);

    // then: the reservation stays pending until an observation names it (A4)
    assert_eq!(lost, Tick::SessionLost(LossCause::Expired));
    assert_eq!(w.state().acquiring.len(), 1);
}

#[test]
fn should_name_the_cause_when_a_listener_error_is_shown() {
    // given
    let errors = [
        ListenerError::Refused("not_ensured"),
        ListenerError::Name(mayfly_control::journal::JournalValueError::InvalidStreamId),
        ListenerError::Fenced,
    ];

    // when
    let shown: Vec<String> = errors.iter().map(ToString::to_string).collect();

    // then
    assert_eq!(
        shown,
        [
            "refused: not_ensured",
            "name: invalid stream id",
            "fenced: a newer run owns the scale set"
        ]
    );
}

#[test]
fn should_report_no_gap_when_no_statistics_were_applied() {
    // given
    let state = ScaleSetState::initial();

    // when
    let gap = gap_of(&state);

    // then
    assert_eq!(gap, 0);
}

#[test]
fn should_act_on_nothing_when_a_job_runs_on_an_unrecognised_runner() {
    // given: a runner no registration of the scale set names takes a job
    let w = bench(Flow::Direct);
    let l = w.listener();
    w.live(&l);
    w.capacity(1, 2, 2, WireMode::Open);
    let job = w.api.queue_jobs(1, LABEL, 42).remove(0);
    polled(tick(&l));
    let runner = w.api.add_runner(w.set(), "someone-elses-runner");
    w.api
        .start_job(&job, runner)
        .unwrap_or_else(|e| panic!("{e}"));
    polled(tick(&l));
    w.api
        .complete_job(runner, mayfly_control::github::JobResult::Failed)
        .unwrap_or_else(|e| panic!("{e}"));
    polled(tick(&l));
    let facts = JobFacts::new(&w.h.store, EPOCH);
    let stream = StreamId::new(&format!("scaleset-{}", pool())).unwrap_or_else(|e| panic!("{e}"));
    let events =
        block_on(w.h.journal.read_stream(&stream, None, None)).unwrap_or_else(|e| panic!("{e}"));

    // when
    let reports: Vec<_> = events
        .iter()
        .map(|e| block_on(facts.apply(e)).unwrap_or_else(|e| panic!("{e}")))
        .collect();

    // then
    let unrecognised: u32 = reports.iter().map(|r| r.unrecognised).sum();
    let accepted: u32 = reports.iter().map(|r| r.accepted).sum();
    assert_eq!((unrecognised, accepted), (2, 0));
}

#[test]
fn should_ask_a_throttled_acquire_at_most_once_per_backoff_when_messages_keep_coming() {
    // given: GHES offers, every acquire throttled, and a message on every poll
    let w = bench(Flow::Acquire);
    let hooked = Hooked::new(w.api.clone(), &w.h.store);
    let l: L<'_, Hooked<'_>> = Listener::new(&w.h.store, &hooked, config());
    w.live(&l);
    w.capacity(1, 4, 4, WireMode::Open);
    hooked.fail_always("acquire", Failure::Throttled);
    w.api.queue_jobs(1, LABEL, 42);
    polled(tick(&l));
    let stats = w.api.statistics(w.set());
    for n in 1..=3 {
        w.api
            .script_message(w.set(), FIRST_MESSAGE_ID + n, vec![], Some(stats));
    }

    // when: three more messages within the backoff, then one after it
    let within: Vec<Polled> = (0..3).map(|_| polled(tick(&l))).collect();
    let asked_within = hooked.calls("acquire");
    w.h.clock.advance(Millis::new(30_000));
    w.api
        .script_message(w.set(), FIRST_MESSAGE_ID + 4, vec![], Some(stats));
    polled(tick(&l));

    // then: every message recorded, the acquire asked once, then once more after the wait
    assert!(within.iter().all(|p| p.recorded == accepted() && p.acked));
    assert_eq!(asked_within, 1);
    assert_eq!(hooked.calls("acquire"), 2);
    assert_eq!(
        w.state().acquiring.len(),
        1,
        "pending until an answer names it (A4)"
    );
}
