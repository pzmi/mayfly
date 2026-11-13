//! The supervision tree's shared pieces: the root cancellation ([`Shutdown`]), the signals that
//! trigger it, and the store's after-commit [`Notifier`].
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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use mayfly_control::delivery::Kicks;
use mayfly_control::journal::{StoredKind, StreamId};
use mayfly_control::store::{Committed, StoreNotifier};
use mayfly_protocol::{Directive, DirectiveSource, LinkError};
use tokio::sync::{Notify, watch};

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

/// Waits for SIGTERM (a service manager's stop) or SIGINT (Ctrl-C in a terminal), then cancels
/// `shutdown`. Both mean P24 here; RT-05 gives Ctrl-C its own terminal-mode stop. Returns
/// without cancelling when the signals cannot be installed, which the caller reports.
///
/// # Errors
/// The handlers could not be installed.
pub async fn on_signals(shutdown: Shutdown) -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    shutdown.cancel();
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
    /// The image placement manager (CP-07).
    pub placement: Kicks,
    /// The placement relay's worker (CP-07).
    pub placement_relay: Kicks,
    /// The terms policy, P4 (CP-08).
    pub terms: Kicks,
    unknown: Arc<(AtomicBool, Notify)>,
}

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
            placement: Kicks::new(KICK_CAPACITY),
            placement_relay: Kicks::new(KICK_CAPACITY),
            terms: Kicks::new(KICK_CAPACITY),
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
    /// Pinned by `tests::should_kick_every_reader_when_a_unit_commits`.
    fn committed(&self, commit: &Committed) {
        self.saga.committed(commit);
        self.relay.committed(commit);
        self.reconciler.committed(commit);
        self.placement.committed(commit);
        self.placement_relay.committed(commit);
        self.terms.committed(commit);
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
        assert_eq!(notifier.terms.take(), [stream]);
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
