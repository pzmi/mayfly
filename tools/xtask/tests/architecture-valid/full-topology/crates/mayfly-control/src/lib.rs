//! Positive fixture: the application uses domain types and tokio's sync primitives, and keeps
//! commands, queries and instrumentation apart: a command that publishes and is traced, a
//! query that reads and fills a buffer, and an instrumentation module (ADR 0042).

pub mod commands;
pub mod observability;
pub mod queries;
pub mod reload_state;

pub use commands::{Apply, Outcome};
pub use queries::Read;
pub use reload_state::ReloadState;

use mayfly_domain::Revision;
use tokio::sync::watch;

/// The control plane's own vocabulary is its to use (`ARCH-plane-vocabulary`).
pub trait Journal {
    fn head(&self) -> Revision;
}

pub fn publish(sender: &watch::Sender<u64>, revision: &Revision) {
    let _ = sender.send(revision.0);
}

#[cfg(test)]
mod tests {
    use std::fs;

    #[test]
    fn should_read_fixture_when_test_builds_a_site() {
        // given
        let dir = std::env::temp_dir();

        // when
        let listed = fs::read_dir(dir).is_ok();

        // then
        assert!(listed);
    }
}
