//! The control plane's first read models (`doc/domain-model.md` §8), each a stream-local inline
//! projection: `attempts` and `pending_effects` from attempt streams, `slots` and `hosts` from
//! host streams, `pools` and `recent_failures` from pool streams, `scale_sets` from scale-set
//! streams, `images` and `placements` from image and placement streams (CP-07). Cross-stream
//! counts are queries over their rows ([`crate::queries`]).

mod attempts;
mod hosts;
mod images;
mod pools;
mod slots;

pub use attempts::{AttemptRow, Attempts, PendingEffectRow, PendingEffects};
pub use hosts::{HostRow, HostTermsRow, Hosts, ScaleSetRow, ScaleSets};
pub use images::{
    ImageRow, Images, PlacementPendingRow, PlacementRow, Placements, phase_code, phase_of,
};
pub use pools::{FailureRow, PoolRow, Pools, RecentFailures, TrippedBreaker};
pub use slots::{SlotRow, Slots};
