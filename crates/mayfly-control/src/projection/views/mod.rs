//! The control plane's first read models (`doc/domain-model.md` §8), each a stream-local inline
//! projection: `attempts` and `pending_effects` from attempt streams, `slots` from host streams,
//! `pools` and `recent_failures` from pool streams. Cross-stream counts are queries over their
//! rows ([`crate::queries`]).

mod attempts;
mod pools;
mod slots;

pub use attempts::{AttemptRow, Attempts, PendingEffectRow, PendingEffects};
pub use pools::{FailureRow, PoolRow, Pools, RecentFailures, TrippedBreaker};
pub use slots::{SlotRow, Slots};
