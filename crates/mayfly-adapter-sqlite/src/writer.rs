//! The threads that own the SQLite connections (fnd §4.4 rule 3, ADR 0008): `rusqlite` blocks,
//! so no connection is ever used on an async caller's thread. Each [`Worker`] owns one
//! connection on one thread and runs the jobs sent to it in order; the async port methods send
//! a job and await its [`Pending`] reply, so the adapter needs no runtime and never blocks one.
//!
//! How many workers there are is an implementation detail and never a correctness premise
//! (`doc/event-store.md` §4.3 rule 2): the journal runs one writer in production, two in the
//! contract's two-connection mode, and every refusal comes from the data (`crate::append`).
//!
//! A worker is joined when it is dropped: its queue closes, it finishes the job in hand and
//! closes its connection (`ARCH-detached-spawn` allows threads in this module only).

use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};
use std::thread::{self, JoinHandle};

use rusqlite::Connection;

/// Jobs a worker queues before a sender is refused: enough for every unit and read a control
/// plane has in flight at once (one per command, consumer and query), small enough that a
/// stalled store pushes back instead of buffering without bound (fnd §4.4 rule 5).
pub(crate) const QUEUE: usize = 64;

/// The connection a job runs on: `None` once a crash barrier dropped it.
pub(crate) struct Slot {
    pub(crate) conn: Option<Connection>,
}

type Job = Box<dyn FnOnce(&mut Slot) + Send>;

/// Why a job was not queued; nothing of it ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotQueued {
    /// The queue is full.
    Full,
    /// The worker is gone.
    Gone,
}

/// One connection on its own thread.
pub(crate) struct Worker {
    sender: Option<SyncSender<Job>>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Moves `conn` to a new thread named `name`; `close` runs on it before the connection
    /// is dropped (when the worker is dropped).
    pub(crate) fn spawn(
        conn: Connection,
        name: &str,
        close: impl FnOnce(Connection) + Send + 'static,
    ) -> std::io::Result<Self> {
        let (sender, jobs) = sync_channel::<Job>(QUEUE);
        let thread = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let mut slot = Slot { conn: Some(conn) };
                while let Ok(job) = jobs.recv() {
                    job(&mut slot);
                }
                if let Some(conn) = slot.conn.take() {
                    close(conn);
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
        })
    }

    /// Queues `job`; its result arrives through the returned [`Pending`].
    pub(crate) fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Slot) -> T + Send + 'static,
    ) -> Result<Pending<T>, NotQueued> {
        let shared = Arc::new(Mutex::new(Reply {
            value: None,
            done: false,
            waker: None,
        }));
        let sender = Sender(Arc::clone(&shared));
        let boxed: Job = Box::new(move |slot| sender.send(job(slot)));
        match self.sender.as_ref().map(|s| s.try_send(boxed)) {
            Some(Ok(())) => Ok(Pending(shared)),
            Some(Err(TrySendError::Full(_))) => Err(NotQueued::Full),
            Some(Err(TrySendError::Disconnected(_))) | None => Err(NotQueued::Gone),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(thread) = self.thread.take() {
            // A worker that panicked has already dropped its connection (rolling back the
            // transaction in hand); there is nothing left to report from here.
            let _ = thread.join();
        }
    }
}

struct Reply<T> {
    value: Option<T>,
    done: bool,
    waker: Option<Waker>,
}

/// The worker's half of a reply. Dropped without sending (a job that panicked), it still
/// wakes the waiter, which then reads `None`.
struct Sender<T>(Arc<Mutex<Reply<T>>>);

impl<T> Sender<T> {
    fn send(self, value: T) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).value = Some(value);
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut reply = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            reply.done = true;
            reply.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// A job's result: `Some` when it ran to the end, `None` when its worker died first.
pub(crate) struct Pending<T>(Arc<Mutex<Reply<T>>>);

impl<T> Future for Pending<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut reply = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if reply.done {
            Poll::Ready(reply.value.take())
        } else {
            reply.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use mayfly_testkit::block_on;

    use super::*;

    fn worker() -> Worker {
        let conn = Connection::open_in_memory().unwrap_or_else(|e| panic!("{e}"));
        Worker::spawn(conn, "mayfly-sqlite-test", drop).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn should_run_jobs_on_the_connection_when_queued() {
        // given
        let worker = worker();

        // when
        let answer = worker
            .run(|slot| {
                slot.conn
                    .as_ref()
                    .map(|c| c.query_row("SELECT 6 * 7", [], |r| r.get::<_, i64>(0)))
            })
            .map(block_on);

        // then
        assert!(matches!(answer, Ok(Some(Some(Ok(42))))), "{answer:?}");
    }

    #[test]
    fn should_answer_none_when_the_job_dies_before_replying() {
        // given
        let worker = worker();

        // when
        let pending = worker.run(|_| -> u8 { std::panic::panic_any("job died") });
        let answer = pending.map(block_on);
        let after = worker.run(|_| 1_u8).map(block_on);

        // then
        assert_eq!(answer, Ok(None));
        assert!(
            matches!(after, Err(NotQueued::Gone) | Ok(None)),
            "{after:?}"
        );
    }

    #[test]
    fn should_refuse_a_job_when_the_queue_is_full() {
        // given
        let worker = worker();
        let (release, gate) = sync_channel::<()>(0);
        let (started, running) = sync_channel::<()>(1);
        let held = worker.run(move |_| {
            started.send(()).ok();
            gate.recv().is_ok()
        });
        running.recv().unwrap_or_else(|e| panic!("{e}"));
        let queued: Vec<_> = (0..QUEUE).map(|_| worker.run(|_| ())).collect();

        // when
        let refused = worker.run(|_| ()).map(|_| ());

        // then
        assert_eq!(refused, Err(NotQueued::Full));
        release.send(()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(held.map(block_on), Ok(Some(true)));
        assert!(queued.into_iter().all(|q| q.map(block_on) == Ok(Some(()))));
    }
}
