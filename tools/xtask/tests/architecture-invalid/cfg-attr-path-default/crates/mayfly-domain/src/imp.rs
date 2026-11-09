//! Every other target's branch: reads a file.

pub fn load() -> Vec<u8> {
    std::fs::read("index.html").unwrap_or_default()
}
