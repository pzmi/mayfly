//! The futures the executor has in flight, polled together on its own task.
//!
//! Nothing is spawned (`ARCH-detached-spawn`): the executor's one future, which the supervision
//! tree runs, polls every in-flight future whenever it is woken. The set holds at most the
//! executor's concurrency bound plus a listing, so polling each on a wake costs little, and a
//! future dropped with the set is cancelled (its directive's deadline, or the session's end).

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A boxed future the set holds.
pub type Job<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Futures in flight, completed in whatever order they finish.
pub struct InFlight<'a, T> {
    jobs: Vec<Job<'a, T>>,
}

impl<T> Default for InFlight<'_, T> {
    fn default() -> Self {
        Self { jobs: Vec::new() }
    }
}

impl<'a, T> InFlight<'a, T> {
    /// Adds `job`; it is first polled on the next [`InFlight::poll_next`].
    pub fn push(&mut self, job: Job<'a, T>) {
        self.jobs.push(job);
    }

    /// Whether none is.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// The output of one future that finished, removing it; `Pending` while none has, and
    /// while the set is empty (the caller waits on its other sources then).
    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<T> {
        for index in 0..self.jobs.len() {
            if let Poll::Ready(out) = self.jobs[index].as_mut().poll(cx) {
                drop(self.jobs.swap_remove(index));
                return Poll::Ready(out);
            }
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use std::future::{pending, ready};
    use std::task::Waker;

    use super::*;

    #[test]
    fn should_yield_each_finished_job_once_when_polled() {
        // given
        let mut cx = Context::from_waker(Waker::noop());
        let mut set: InFlight<'_, u8> = InFlight::default();
        set.push(Box::pin(pending()));
        set.push(Box::pin(ready(1)));
        set.push(Box::pin(ready(2)));

        // when
        let first = set.poll_next(&mut cx);
        let second = set.poll_next(&mut cx);
        let third = set.poll_next(&mut cx);

        // then
        let mut done = [first, second].map(|p| match p {
            Poll::Ready(v) => v,
            Poll::Pending => 0,
        });
        done.sort_unstable();
        assert_eq!(done, [1, 2]);
        assert_eq!(third, Poll::Pending);
        assert!(!set.is_empty());
    }

    #[test]
    fn should_stay_pending_when_nothing_is_in_flight() {
        // given
        let mut cx = Context::from_waker(Waker::noop());
        let mut set: InFlight<'_, u8> = InFlight::default();

        // when
        let polled = set.poll_next(&mut cx);

        // then
        assert_eq!(polled, Poll::Pending);
        assert!(set.is_empty());
    }
}
