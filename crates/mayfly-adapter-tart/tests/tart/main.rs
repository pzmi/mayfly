//! The Tart adapter at integration level: the `VmProvider` contract over `mayfly-fake-tart`
//! (`contract`), the shim and re-adoption across a killed daemon (`shim`), the process runner
//! (`runner`), the adapter's own promises about every `tart` call (`calls`), and the
//! `GuestTransport` contract over the fake's emulated guests (`guest`), and the `ImageStore`
//! contract over the fake's registry and launcher (`images`).

mod calls;
mod contract;
mod guest;
mod images;
mod runner;
mod shim;
mod support;
