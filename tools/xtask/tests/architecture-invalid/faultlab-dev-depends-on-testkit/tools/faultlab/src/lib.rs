//! Fixture: the fault lab borrows the test kit in its tests; a dev edge to a test-only crate
//! is legal elsewhere, but it would share code with production's tests (D11).

pub fn verdict() -> bool {
    true
}
