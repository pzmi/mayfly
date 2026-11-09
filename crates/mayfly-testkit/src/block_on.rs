//! A minimal executor for synchronous callers of the async ports and contract suites.
//!
//! The ports are native `async fn` traits (D8); the in-memory fakes complete without waiting,
//! so a test that only drives fakes does not need a runtime. A real adapter that needs Tokio
//! runs the async suite inside `#[tokio::test]` instead.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Polls `future` to completion on the calling thread, parking between wake-ups.
///
/// Pinned by `should_complete_future_when_it_is_woken_from_another_thread`.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        thread::park();
    }
}
