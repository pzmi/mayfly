//! Fixture: the CLI spawns a process through composition's exported macro, whose body
//! expands here but was checked only where it is written (second checkpoint review M2).

pub fn shell() {
    let _ = mayfly_composition::shell!();
}
