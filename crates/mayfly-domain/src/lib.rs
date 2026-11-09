//! `mayfly-domain` (shared plane): the kernel both planes and the protocol share.
//!
//! Ids, VM name ownership, `Generation`, `Epoch`, `Fence`, `ImageDigest`, `Resources`, the time
//! value types and their errors. Pure: no clock, randomness or I/O (ADR 0017 allows `serde`
//! derive only). Values that carry an invariant deserialise only through their validating
//! parser, so a decoded value is as valid as a constructed one.

mod digest;
mod error;
mod fence;
mod ids;
mod resources;
mod time;
mod vm_name;

pub use digest::ImageDigest;
pub use error::{DigestError, IdError, VmNameError};
pub use fence::{Epoch, Fence, FenceError, Generation};
pub use ids::{AttemptId, CommandId, DirectiveId, HostId, ObservationId, PoolName};
pub use resources::Resources;
pub use time::{Deadline, Millis};
pub use vm_name::{OwnedVmName, Slot, VmNameClass, classify_vm_name};
