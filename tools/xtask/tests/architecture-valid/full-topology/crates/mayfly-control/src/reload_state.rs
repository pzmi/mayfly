//! The latest-state channel: `current` reads, `publish_next` mutates.

use tokio::sync::watch;

pub struct ReloadState {
    sender: watch::Sender<u64>,
}

impl ReloadState {
    pub fn current(&self) -> u64 {
        *self.sender.borrow()
    }

    pub fn publish_next(&self) -> u64 {
        let next = self.current() + 1;
        self.sender.send_replace(next);
        next
    }
}
