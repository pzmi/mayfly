//! `mayfly-data` (data plane): the host agent's application (plan §3.1, §8, §9).
//!
//! The ports the data plane acts and observes through ([`ports`]: `VmProvider`, `ImageStore`,
//! `GuestTransport`, `HostProbe`, `ProcessClock`, `ObservationIds`, `FloorStore`) and the
//! executor that drives the pure rules of `mayfly-data-domain` over them ([`executor`]): it
//! rebuilds the resource ledger from observation, enters sessions, runs directives with
//! deadlines and bounded concurrency, re-observes before a stop or delete, observes the
//! provider without ever applying a listing an effect overlapped ([`gate`]), and reports
//! capacity. It stores nothing durable but the epoch floor, through its port, and names no
//! journal type. [`launch_store`] holds the JIT configs awaiting delivery in memory, bound to
//! their VM (ADR 0082); [`redaction`] is what an exported guest log may keep.

pub mod executor;
pub mod gate;
mod inflight;
pub mod launch_store;
pub mod ports;
pub mod redaction;

pub use executor::{Config, EnterError, Executor, Ports, SessionEnd, StartError, Stats};
pub use launch_store::{AlreadyHeld, LaunchBinding, LaunchStore};
/// The reading [`HostProbe::resources`] answers with, named apart from the port.
pub use mayfly_data_domain::HostProbe as HostReading;
pub use redaction::redact_log;
// The types the ports name, so an adapter implements them through this crate alone.
pub use mayfly_data_domain::{ForeignUse, Inventory, Shim, foreign_use};
pub use mayfly_protocol::{
    BaseName, Capacity, DiagnosticsLoss, GuestFacts, GuestOs, GuestRejection, HostFacts,
    InventoryGap, Nonce, OwnershipProof, ResourceDimension, RunOptions, SecretHandle,
    VmObservation,
};
pub use ports::{
    Ambiguity, Diagnostics, FloorFailed, FloorStore, GuestAnswer, GuestTransport, HostProbe,
    ImageListing, ImageStore, LaunchAnswer, LaunchSecrets, ObservationIds, ProbeFailed,
    ProcessClock, RunnerStatus, VmError, VmProvider,
};
