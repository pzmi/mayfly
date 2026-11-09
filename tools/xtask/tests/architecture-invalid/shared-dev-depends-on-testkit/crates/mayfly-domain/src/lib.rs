//! Fixture: the kernel's tests use the control plane's kit, so a kernel test may drive
//! the control plane (ADR 0019, amended 2026-11-09).

pub fn double(n: u32) -> u32 {
    n * 2
}
