//! Fixture: the control-plane domain reads a file itself.

pub fn load() -> Vec<u8> {
    std::fs::read("pools.json").unwrap_or_default()
}
