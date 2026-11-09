//! Fixture: the domain reads a file itself.

pub fn load() -> Vec<u8> {
    std::fs::read("index.html").unwrap_or_default()
}
