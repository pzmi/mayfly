//! Fixture: the control plane spawns a detached task that nothing cancels on shutdown.

pub fn start() {
    tokio::spawn(async {});
}
