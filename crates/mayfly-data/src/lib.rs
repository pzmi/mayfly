//! `mayfly-data` (data plane): the host agent's application (plan §3.1, §8, §9).
//!
//! The ports the data plane acts and observes through ([`ports`]: `VmProvider`, `ImageStore`,
//! `GuestTransport`, `HostProbe`, `ProcessClock`, `ObservationIds`, `FloorStore`) and the
//! executor that drives the pure rules of `mayfly-data-domain` over them ([`executor`]): it
//! rebuilds the resource ledger from observation, enters sessions, runs directives with
//! deadlines and bounded concurrency, re-observes before a stop or delete, observes the
//! provider without ever applying a listing an effect overlapped ([`gate`]), and reports
//! capacity. It stores nothing durable but the epoch floor, through its port, and names no
//! journal type.

pub mod executor;
pub mod gate;
mod inflight;
pub mod ports;

pub use executor::{Config, EnterError, Executor, Ports, SessionEnd, StartError, Stats};
/// The reading [`HostProbe::resources`] answers with, named apart from the port.
pub use mayfly_data_domain::HostProbe as HostReading;
// The types the ports name, so an adapter implements them through this crate alone.
pub use mayfly_data_domain::{ForeignUse, Inventory, Shim, foreign_use};
pub use mayfly_protocol::{
    BaseName, Capacity, GuestOs, HostFacts, InventoryGap, OwnershipProof, ResourceDimension,
    RunOptions, VmObservation,
};
pub use ports::{
    Ambiguity, Diagnostics, FloorFailed, FloorStore, GuestAnswer, GuestTransport, HostProbe,
    ImageListing, ImageStore, ObservationIds, ProbeFailed, ProcessClock, VmError, VmProvider,
};
