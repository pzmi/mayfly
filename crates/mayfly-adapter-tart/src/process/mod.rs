//! The adapter's processes (plan §8.2, D12): the module the architecture checker lets own
//! children (`SPAWN_MODULES`).
//!
//! - [`runner`]: one bounded `tart` call.
//! - [`launch`]: starting a VM's shim, the one child the adapter does not wait for.
//! - [`vm_shim`]: the shim itself, the body of `mayfly vm-shim`.

pub mod launch;
pub mod runner;
pub mod vm_shim;
