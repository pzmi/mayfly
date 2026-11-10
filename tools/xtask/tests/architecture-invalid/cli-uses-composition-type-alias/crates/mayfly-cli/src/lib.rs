//! Fixture: the CLI spawns a process through composition's type alias, which the
//! re-export follower did not resolve (second checkpoint review M2).

pub fn shell() {
    let _ = mayfly_composition::Cmd::new("sh").env_clear().status();
}
