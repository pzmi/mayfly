//! The Tart adapter at integration level: the `VmProvider` contract over `mayfly-fake-tart`
//! (`contract`), the shim and re-adoption across a killed daemon (`shim`), the process runner
//! (`runner`) and the adapter's own promises about every `tart` call (`calls`).

mod calls;
mod contract;
mod runner;
mod shim;
mod support;
