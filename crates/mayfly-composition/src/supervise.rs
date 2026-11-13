//! The supervision tree's shared pieces: the root cancellation ([`Shutdown`]), terminal mode's
//! graceful stop beside it ([`Stops`]), the signals that trigger them, and the store's
//! after-commit [`Notifier`].
//!
//! **Signals (ADR 0031, amended 2026-11-12).** SIGTERM, a service manager's stop, is P24 at
//! once: [`Stops::force`]. Ctrl-C (SIGINT) escalates ([`escalation`]): the first starts
//! terminal mode's graceful stop ([`Stops::graceful`]), the second prints a warning that the
//! next forces the stop, the third is exactly SIGTERM.
//!
//! **One root, structured.** Every supervised loop of the daemon is a future polled by the
//! daemon's own task (`crate::daemon`), never a detached spawn: a loop cannot outlive the run
//! that started it, and the run returns only once every loop returned. Each loop selects on
//! [`ShutdownSignal::cancelled`] between units of work, never inside one, so a save, a drain
//! step or a directive's effect in flight finishes before its loop ends ("finish durable
//! writes", P24). Rejected alternative: `tokio::spawn` per loop with a `JoinSet`, which needs
//! every borrowed component (`AggregateStore`, the saga, the sink) to be `'static` and lets a
//! panicking loop leave the others running.
//!
//! **Kicks per consumer.** `AggregateStore` takes one notifier; the daemon has several readers
//! of the after-commit kick (each durable consumer's worker, the reconciler), each with its own
//! [`Kicks`] set so one draining never takes another's kick (ES-05). A stored kind this build
//! cannot decode, met mid-run (§6.3 rule 4), raises a flag the run reacts to by entering safe
//! mode.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use mayfly_control::delivery::Kicks;
use mayfly_control::journal::{StoredKind, StreamId};
use mayfly_control::reconciler::kick_prepared;
use mayfly_control::store::{Committed, StoreNotifier};
use mayfly_protocol::{Directive, DirectiveSource, LinkError};
use tokio::sync::{Notify, broadcast, watch};

/// The root cancellation of the supervision tree; clones share it.
#[derive(Debug, Clone)]
pub struct Shutdown(Arc<watch::Sender<bool>>);

/// A loop's view of the root cancellation.
#[derive(Debug, Clone)]
pub struct ShutdownSignal(watch::Receiver<bool>);

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    /// A root not cancelled yet.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    /// Cancels the tree: every loop ends at its next point between units of work.
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    /// Whether the tree was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// A signal for one loop.
    #[must_use]
    pub fn signal(&self) -> ShutdownSignal {
        ShutdownSignal(self.0.subscribe())
    }
}

impl ShutdownSignal {
    /// Completes once the tree is cancelled (at once if it is). Cancel-safe.
    pub async fn cancelled(&mut self) {
        let _ = self.0.wait_for(|cancelled| *cancelled).await;
    }

    /// Whether the tree was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
}

/// How the daemon stops: P24 ([`Stops::force`], the root every loop ends on) and terminal
/// mode's graceful stop ([`Stops::graceful`], RT-05a), which the daemon turns into a clean end
/// of the run once its idle VMs are deleted, or into P24 when its wait runs out.
#[derive(Debug, Clone, Default)]
pub struct Stops {
    /// P24: SIGTERM, the third Ctrl-C, or the graceful stop's wait run out.
    pub force: Shutdown,
    /// The first Ctrl-C: retire and delete the idle VMs, wait for busy jobs, then end clean.
    pub graceful: Shutdown,
}

/// What the `nth` Ctrl-C (counted from 1) does in terminal mode (ADR 0031, amended
/// 2026-11-12). Pinned by `tests::should_escalate_to_p24_when_ctrl_c_is_pressed_three_times`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escalation {
    /// The first: the graceful stop.
    Graceful,
    /// The second: a warning that the next one forces the stop.
    Warn,
    /// The third and any later: P24, exactly as SIGTERM.
    Force,
}

/// The step of the `nth` Ctrl-C, counted from 1.
#[must_use]
pub const fn escalation(nth: NonZeroU32) -> Escalation {
    match nth.get() {
        1 => Escalation::Graceful,
        2 => Escalation::Warn,
        _ => Escalation::Force,
    }
}

/// The second Ctrl-C's line on stderr.
pub const FORCE_WARNING: &str = "mayflyd: stopping; press Ctrl-C again to stop now and leave \
     the remaining VMs running for the next start";

/// Waits for SIGTERM (a service manager's stop: [`Stops::force`] at once) and SIGINT (Ctrl-C in
/// a terminal, escalating by [`escalation`]); `warn` writes the second Ctrl-C's
/// [`FORCE_WARNING`]. Returns once the stop is forced; without cancelling anything when the
/// signals cannot be installed, which the caller reports. Presses the kernel delivers before the
/// handler reads the first are coalesced into one (a pending signal is not queued), so a quick
/// double press may count once; the next press then counts.
///
/// # Errors
/// The handlers could not be installed.
pub async fn on_signals(stops: Stops, warn: impl Fn(&str)) -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut interrupts: Option<NonZeroU32> = None;
    loop {
        tokio::select! {
            _ = term.recv() => break,
            _ = int.recv() => {
                let nth = interrupts.map_or(NonZeroU32::MIN, |n| n.saturating_add(1));
                interrupts = Some(nth);
                match escalation(nth) {
                    Escalation::Graceful => {
                        tracing::info!("terminal_stop_started");
                        stops.graceful.cancel();
                    }
                    Escalation::Warn => warn(FORCE_WARNING),
                    Escalation::Force => break,
                }
            }
        }
    }
    stops.force.cancel();
    Ok(())
}

/// The store's notifier: a kick for each reader, and the unknown-kind flag.
#[derive(Debug, Clone)]
pub struct Notifier {
    /// The attempt saga's worker.
    pub saga: Kicks,
    /// The directive relay's worker.
    pub relay: Kicks,
    /// The reconciler.
    pub reconciler: Kicks,
    /// The reconciler's wake for an attempt that became Prepared (CP-10): only the attempt
    /// streams a commit appended `attempt_prepared` to ([`kick_prepared`]).
    pub prepared: Kicks,
    /// The image placement manager (CP-07).
    pub placement: Kicks,
    /// The placement relay's worker (CP-07).
    pub placement_relay: Kicks,
    /// The terms policy, P4 (CP-08).
    pub terms: Kicks,
    /// The registration manager's worker (RT-07).
    pub registration: Kicks,
    /// The job facts' worker, P12 (RT-07).
    pub job_facts: Kicks,
    /// Terminal mode's graceful stop (RT-05a): it reads the host's leases again after a commit.
    pub stop: Kicks,
    /// The reconciler's wake while the run stops (RT-05a): any commit, so a lease is released
    /// as soon as its VM's deletion is recorded, not an interval later.
    pub stopping: Kicks,
    /// `mayfly events --follow` (RT-03, `doc/event-store.md` §5.6): every committed unit, best
    /// effort. A follower that falls more than [`FOLLOW_CAPACITY`] units behind is told how
    /// many it lost; nothing decides from it.
    pub follow: broadcast::Sender<Committed>,
    unknown: Arc<(AtomicBool, Notify)>,
}

/// Committed units a follower may lag behind before it loses the oldest.
pub const FOLLOW_CAPACITY: usize = 1024;

/// Streams one reader may hold pending before a kick is dropped (a dropped kick costs only
/// latency: the mark stays in the ledger for the next `catch_up`).
pub const KICK_CAPACITY: usize = 1024;

impl Default for Notifier {
    fn default() -> Self {
        Self::new()
    }
}

impl Notifier {
    /// Empty kick sets and no unknown kind seen.
    #[must_use]
    pub fn new() -> Self {
        Self {
            saga: Kicks::new(KICK_CAPACITY),
            relay: Kicks::new(KICK_CAPACITY),
            reconciler: Kicks::new(KICK_CAPACITY),
            prepared: Kicks::new(KICK_CAPACITY),
            placement: Kicks::new(KICK_CAPACITY),
            placement_relay: Kicks::new(KICK_CAPACITY),
            terms: Kicks::new(KICK_CAPACITY),
            registration: Kicks::new(KICK_CAPACITY),
            job_facts: Kicks::new(KICK_CAPACITY),
            stop: Kicks::new(KICK_CAPACITY),
            stopping: Kicks::new(KICK_CAPACITY),
            follow: broadcast::channel(FOLLOW_CAPACITY).0,
            unknown: Arc::new((AtomicBool::new(false), Notify::new())),
        }
    }

    /// Whether a save met a stored kind this build does not decode.
    #[must_use]
    pub fn unknown_kind_seen(&self) -> bool {
        self.unknown.0.load(Ordering::SeqCst)
    }

    /// Completes once a save met a stored kind this build does not decode (at once if one
    /// did).
    pub async fn unknown_kind(&self) {
        loop {
            let notified = self.unknown.1.notified();
            if self.unknown_kind_seen() {
                return;
            }
            notified.await;
        }
    }
}

impl StoreNotifier for Notifier {
    /// Pinned by `tests::should_kick_every_reader_when_a_unit_commits` and
    /// `tests::should_kick_the_prepared_wake_only_when_an_attempt_becomes_prepared`.
    fn committed(&self, commit: &Committed) {
        self.saga.committed(commit);
        self.relay.committed(commit);
        // before the reconciler's own kick, which wakes its loop: the tick then finds it
        kick_prepared(&self.prepared, commit);
        self.reconciler.committed(commit);
        self.placement.committed(commit);
        self.placement_relay.committed(commit);
        self.terms.committed(commit);
        self.registration.committed(commit);
        self.job_facts.committed(commit);
        self.stop.committed(commit);
        self.stopping.committed(commit);
        if self.follow.receiver_count() > 0 {
            // no follower is not an error: the send only reaches those subscribed now
            let _ = self.follow.send(commit.clone());
        }
    }

    fn unknown_kind(&self, _stream: &StreamId, _kind: &StoredKind) {
        self.unknown.0.store(true, Ordering::SeqCst);
        self.unknown.1.notify_waiters();
    }
}

/// Ends data-plane sessions on request: each [`SessionCut::cut`] makes the next directive read
/// of one session (the first that looks) report the link closed, so that session's serve
/// returns as it does when a real link ends (its effects in flight finished first). A cut made
/// before any session is consumed by the first one. Production never cuts; the seam lets a test
/// lose the link mid-run (`tests/daemon.rs`) without a second transport.
#[derive(Debug, Clone, Default)]
pub struct SessionCut(Arc<(AtomicU64, Notify)>);

impl SessionCut {
    /// Ends one session.
    pub fn cut(&self) {
        self.0.0.fetch_add(1, Ordering::SeqCst);
        self.0.1.notify_waiters();
    }

    fn take(&self) -> bool {
        self.0
            .0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
    }
}

/// A session's directive source that a [`SessionCut`] can end.
#[derive(Debug)]
pub struct Cuttable<S> {
    inner: S,
    cut: SessionCut,
}

impl<S> Cuttable<S> {
    /// `inner`, ended by `cut`.
    pub const fn new(inner: S, cut: SessionCut) -> Self {
        Self { inner, cut }
    }
}

impl<S: DirectiveSource> DirectiveSource for Cuttable<S> {
    /// Cancel-safe as `inner` is.
    async fn recv(&mut self) -> Result<Directive, LinkError> {
        loop {
            let notified = self.cut.0.1.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.cut.take() {
                return Err(LinkError::Closed);
            }
            tokio::select! {
                () = &mut notified => {}
                received = self.inner.recv() => return received,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use mayfly_control::journal::{Appended, AppendedStream, StreamRevision};
    use mayfly_domain::CommandId;

    use super::*;

    #[test]
    fn should_kick_every_reader_when_a_unit_commits() {
        // given
        let notifier = Notifier::new();
        let stream = StreamId::new("attempt-1").unwrap_or_else(|e| panic!("{e}"));
        let commit = Committed {
            command_id: CommandId::from_u128(1),
            appended: Appended {
                streams: vec![AppendedStream {
                    stream: stream.clone(),
                    first: StreamRevision::FIRST,
                    last: StreamRevision::FIRST,
                    event_ids: Vec::new(),
                }],
            },
            kinds: Vec::new(),
        };

        // when
        notifier.committed(&commit);

        // then
        assert_eq!(notifier.saga.take(), std::slice::from_ref(&stream));
        assert_eq!(notifier.relay.take(), std::slice::from_ref(&stream));
        assert_eq!(notifier.reconciler.take(), std::slice::from_ref(&stream));
        assert_eq!(notifier.placement.take(), std::slice::from_ref(&stream));
        assert_eq!(
            notifier.placement_relay.take(),
            std::slice::from_ref(&stream)
        );
        assert_eq!(notifier.terms.take(), std::slice::from_ref(&stream));
        assert_eq!(notifier.registration.take(), std::slice::from_ref(&stream));
        assert_eq!(notifier.job_facts.take(), std::slice::from_ref(&stream));
        assert_eq!(notifier.stop.take(), std::slice::from_ref(&stream));
        assert_eq!(notifier.stopping.take(), [stream]);
        assert!(notifier.prepared.take().is_empty());
    }

    #[test]
    fn should_kick_the_prepared_wake_only_when_an_attempt_becomes_prepared() {
        // given
        let notifier = Notifier::new();
        let attempt = StreamId::new("attempt-1").unwrap_or_else(|e| panic!("{e}"));
        let commit = |kind: &str| Committed {
            command_id: CommandId::from_u128(1),
            appended: Appended {
                streams: vec![AppendedStream {
                    stream: attempt.clone(),
                    first: StreamRevision::FIRST,
                    last: StreamRevision::FIRST,
                    event_ids: Vec::new(),
                }],
            },
            kinds: vec![StoredKind {
                event_type: kind.to_owned(),
                schema_version: 1,
            }],
        };

        // when
        notifier.committed(&commit("vm_cloned"));
        let on_clone = notifier.prepared.take();
        notifier.committed(&commit("attempt_prepared"));
        let on_prepared = notifier.prepared.take();

        // then
        assert!(on_clone.is_empty());
        assert_eq!(on_prepared, [attempt]);
    }

    #[test]
    fn should_escalate_to_p24_when_ctrl_c_is_pressed_three_times() {
        // given
        let presses = [1, 2, 3, 4].map(|n| NonZeroU32::new(n).unwrap_or(NonZeroU32::MIN));

        // when
        let steps = presses.map(escalation);

        // then: graceful, a warning, then P24 (and P24 again for any later press)
        assert_eq!(
            steps,
            [
                Escalation::Graceful,
                Escalation::Warn,
                Escalation::Force,
                Escalation::Force
            ]
        );
        assert!(FORCE_WARNING.contains("Ctrl-C again"));
    }

    #[test]
    fn should_report_the_tree_cancelled_when_shutdown_is_cancelled() {
        // given
        let shutdown = Shutdown::new();
        let signal = shutdown.signal();
        let before = signal.is_cancelled();

        // when
        shutdown.cancel();

        // then
        assert!(!before);
        assert!(signal.is_cancelled());
        assert!(shutdown.is_cancelled());
    }
}
