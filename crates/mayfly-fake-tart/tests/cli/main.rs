//! Integration tests of the built `tart` binary against a fake `TART_HOME` in a tempdir: the
//! command surface (`commands`), the fault file (`faults`) and the guest launcher (`guest`).

mod commands;
mod faults;
mod guest;
mod support;
