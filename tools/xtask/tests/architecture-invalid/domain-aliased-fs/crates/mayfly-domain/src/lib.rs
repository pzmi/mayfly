//! Fixture: the domain reaches `std::fs` through an alias.

use std::fs as f;

pub fn load() -> Vec<u8> {
    f::read("index.html").unwrap_or_default()
}
