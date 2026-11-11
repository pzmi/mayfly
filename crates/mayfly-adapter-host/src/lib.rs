//! `mayfly-adapter-host` (data plane): the host probe (DP-06, plan §9, ADR 0024).
//!
//! [`OsProbe`] implements `mayfly_data::HostProbe` from what the host's operating system says
//! about itself ([`source`]: free and total disk on the volume of the provider's home, total and
//! available memory, the boot id, and two clocks whose drift is the time spent asleep) and from
//! what the VM provider lists ([`ProviderView`]: the VMs that are not ours, sized as the ledger
//! sizes ours, and the provider's version). Its readings feed the data plane's resource ledger
//! through the executor; nothing here reaches the control plane.
//!
//! **Nothing unread is assumed (ADR 0052).** A host reading that fails makes the whole probe
//! fail ([`mayfly_data::ProbeFailed`]), which the executor turns into every dimension unknown
//! (`capacity_unknown`); a failed provider listing makes every foreign dimension unsized, and a
//! foreign VM the listing does not describe makes its own dimensions unsized. No reading is ever
//! replaced by a default or by a previous value.
//!
//! Per-OS code is `#[cfg]` items only ([`source::Os`]): Linux reads `/proc` and the clocks
//! through `rustix`; macOS reads `sysctl` names through the `sysctl` crate. Neither needs
//! `unsafe` here.

mod probe;
pub mod source;

pub use probe::{HostConfig, OsProbe, ProviderView, reading, slept};
pub use source::{Clocks, HostSource, Memory, Os, ReadFailed, Volume};
