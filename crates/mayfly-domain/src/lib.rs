//! `mayfly-domain` (shared plane): the kernel both planes and the protocol share.
//!
//! Ids, VM name ownership, the provider instance id, `Generation`, `Epoch`, `Fence`,
//! `ImageDigest`, `Resources`, the secret reference ([`SecretRef`], ADR 0033) and the redacting [`Secret`] (ADR 0014), the time value types and their errors, and a [`Sample`] of each validated text type. Pure: no clock,
//! randomness or I/O (ADR 0017 allows `serde` derive only). Values that carry an invariant
//! deserialise only through their validating parser, so a decoded value is as valid as a
//! constructed one.

mod digest;
mod error;
mod fence;
mod ids;
mod instance;
mod resources;
mod sample;
mod secret;
mod secret_ref;
mod time;
mod vm_name;

pub use digest::ImageDigest;
pub use error::{DigestError, IdError, VmNameError};
pub use fence::{Epoch, Fence, FenceError, Generation};
pub use ids::{
    AttemptId, CommandId, DirectiveId, EventId, HostId, ObservationId, PoolName, SecretHandleId,
};
pub use instance::ProviderInstanceId;
pub use resources::Resources;
pub use sample::Sample;
pub use secret::Secret;
pub use secret_ref::{SecretRef, SecretRefError};
pub use time::{Deadline, Millis};
pub use vm_name::{OwnedVmName, Slot, VmNameClass, classify_vm_name};
