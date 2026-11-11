//! `mayfly-protocol` (shared plane): the published language between the control plane and
//! the data plane (plan §3.2, dm §4).
//!
//! Plain data (ADR 0022): the handshake (`Hello`, `Welcome`), `Directive`, `Observation`, the
//! `SecretHandle` that stands for a secret on the link, the `OwnershipProof` of destructive
//! directives, the version rules and the epoch rules as pure functions, and the link port
//! traits. Messages are serde data; the encoding (JSON, ADR 0010) belongs to the transports.
//! Pure: no clock, randomness or I/O. The rules are those of `formal/link.qnt`.

mod directive;
mod frame;
mod handshake;
mod link;
mod observation;
mod version;

pub use directive::{
    AttemptRef, BaseName, Directive, DirectiveKind, DirectiveTag, MalformedValue, Nonce,
    OwnershipProof, RunOptions, SecretHandle,
};
pub use frame::{Frame, FrameHeader, Message, MessageKind};
pub use handshake::{
    EpochCheck, HandshakeRefusal, HandshakeRefused, Hello, Mode, Welcome, check_epoch,
    enters_session, mint_epoch,
};
pub use link::{
    DirectiveSink, DirectiveSource, HandshakeInitiator, HandshakeResponder, LinkError,
    ObservationSink, ObservationSource,
};
pub use observation::{
    Capacity, CapacityConstraint, CapacityReport, DiagnosticsLoss, DirectiveOutcome, DirectiveRef,
    GuestFacts, GuestOs, GuestRejection, HostFacts, InventoryGap, Observation, ObservationKind,
    RefusalReason, ResourceDimension, UnknownCapacity, UnknownCause, UnknownReason, VmObservation,
};
pub use version::{
    CURRENT, GUEST_IDENTITY, ProtocolVersion, SUPPORTED, UNKNOWN_FACTS, VersionRefusal,
    admit_frame, handshake_version, negotiate,
};
