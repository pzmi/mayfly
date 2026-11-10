//! `mayfly vm-shim`: the per-VM shim the Tart adapter starts for each VM (ADR 0037). Wiring
//! only: the shim's body and its argument contract belong to the adapter that spawns it
//! (`mayfly_adapter_tart::process::vm_shim`).

use std::ffi::OsString;

/// Runs the shim with `args` (everything after `vm-shim`); returns the process's exit code.
#[must_use]
pub fn run(args: &[OsString]) -> i32 {
    let stderr = std::io::stderr();
    mayfly_adapter_tart::process::vm_shim::main(args, &mut stderr.lock())
}
