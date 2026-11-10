//! Positive fixture: the Tart adapter drives the `tart` CLI by argv, the process authority
//! plan section 3.4 grants it.

use std::process::Command;

pub fn clone_vm(name: &str) -> std::io::Result<bool> {
    Ok(Command::new("tart").env_clear().args(["clone", "base", name]).status()?.success())
}
