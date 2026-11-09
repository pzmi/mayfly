//! Fixture: the CLI spawns a process through composition's re-export (checkpoint finding
//! M2); the CLI has no process authority.

pub fn shell() {
    let _ = mayfly_composition::Spawner::new("sh").status();
}
