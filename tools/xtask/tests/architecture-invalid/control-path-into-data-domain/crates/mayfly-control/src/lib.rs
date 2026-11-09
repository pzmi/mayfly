//! Fixture: the control plane compiles the data-plane domain's source as its own module, so
//! no dependency edge and no plane rule ever sees it (checkpoint finding H1).
#[path = "../../mayfly-data-domain/src/lib.rs"]
mod borrowed;

pub fn plan() -> u8 {
    borrowed::slot()
}
