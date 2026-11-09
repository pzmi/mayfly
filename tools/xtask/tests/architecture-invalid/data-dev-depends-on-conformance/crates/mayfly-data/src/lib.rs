//! Fixture: the data plane dev-depends on conformance, which sees both planes and re-exports
//! the SQLite adapter, so a data-plane test reaches the journal (checkpoint finding M1).

pub fn ready() -> bool {
    true
}
