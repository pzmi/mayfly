//! `mayfly-data-domain` (data plane): the pure rules of a host agent (plan §3.1, §6, §8.4, §9).
//!
//! Plain data and free functions (ADR 0022): the session and its epoch floor ([`link`]), the
//! directive executor with its per-session outcome table, fencing, safe mode and ownership
//! checks ([`executor`]), the resource ledger that refuses what does not fit (I33, ADR 0024;
//! [`ledger`]), the VM supervisor derived from the provider's listing and the shim's files
//! (ADR 0037; [`supervisor`]) and the inventory that is complete or not (I25; [`inventory`]).
//! Pure: no clock, randomness, I/O or process; time and ids arrive as values. The rules follow
//! `formal/link.qnt` (fencing, idempotency, safe mode), `formal/lifecycle.qnt` (the proof
//! re-checked against the re-observed instance id) and `formal/scheduling.qnt` (`dpFits`).

pub mod executor;
pub mod inventory;
pub mod ledger;
pub mod link;
pub mod supervisor;

pub use executor::{
    DataPlane, Discard, Entry, MAX_RUNNER_VERSION, Misfit, Phase, Progress, Receipt, Reobserved,
    Reply, Stage, Stimulus, admission, answered_kind, proof_of, reobserve, reobserve_name,
    reobserves,
};
pub use inventory::Inventory;
pub use ledger::{
    BaseUse, ForeignUse, HostProbe, Ledger, MeasuredDisks, Reservation, Settles, Sizing, VmUse,
    foreign_use, gaps,
};
pub use link::{Link, LinkIgnored, LinkOutcome, LinkStimulus, Session};
pub use supervisor::{Seen, Shim, Signal, VmFact, VmFacts, VmState};
