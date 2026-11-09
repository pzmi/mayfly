//! Fixture: a pure crate without its clippy.toml, so Clippy resolves no disallowed call
//! (second checkpoint review H2).

pub fn double(n: u32) -> u32 {
    n * 2
}
