//! Fixture: a crate turns on a nightly feature, which the pinned stable toolchain accepts
//! only under `RUSTC_BOOTSTRAP` (third checkpoint review M2); `register_tool` would let it
//! declare a lint tool whose levels no rule reads.
#![cfg_attr(all(), feature(register_tool))]

pub fn value() -> u8 {
    1
}
