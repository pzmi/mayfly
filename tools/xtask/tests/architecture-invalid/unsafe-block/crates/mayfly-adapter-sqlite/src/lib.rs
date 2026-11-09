//! Fixture: the filesystem adapter dereferences a raw pointer.

pub fn first(bytes: &[u8]) -> u8 {
    let ptr = bytes.as_ptr();
    unsafe { *ptr }
}
