//! The image catalog's use cases (OPS-02; `doc/domain-model.md` §6.2, §6.6, P17): an image
//! registered by digest with its manifest, its verification recorded, and a pool pinned to it.
//! The pin is the step that makes a pool's image placeable: the placement manager places every
//! pool's pin on every host ([`crate::placement`]).
//!
//! **Idempotent.** Each command's id is derived from what it is about: a registration from the
//! digest and the manifest, a verification from the digest and its outcome, a pin from the
//! pool, the digest and the pool's revision the use case read. A use case run again over the
//! same inputs is answered by its receipt; a pin of a digest the pool pinned before, then left,
//! reads a later revision and so is a new command, never a replay of the first.
//!
//! **The pin is a verified digest** ([`pin_image`]). The image's stream is read with its head,
//! the pin refused unless the image is `Verified`, its `ImageVerified` event id becomes the
//! pin's `verified_ref`, and `PinImage` is saved with a read of the image stream at that head:
//! a withdrawal (or a rejection) committed between the read and the save refuses the unit
//! (`conflict`), and the retry reads the image again and refuses the pin (pinned by
//! `tests/store/images.rs::should_refuse_the_pin_at_commit_when_the_image_is_withdrawn_after_the_read`).
//!
//! **Limitation.** This narrows the window `crate::placement::withdraw_image` leaves (a pool
//! stream created and pinned after the withdrawal's last listing of the pools) without closing
//! it: when such a pin commits *first*, the withdrawal, which never listed that pool, commits
//! after it, because neither unit reads a stream the other writes. Closing it needs the
//! withdrawal to announce itself on the image stream before it lists the pools (a two-step
//! withdrawal, an `Image` change: model and decision first), recorded on OPS-02's card. No
//! caller withdraws an image under `mayflyd` yet (the operator socket is RT-03's).
//!
//! What "verified" means is the caller's (ADR 0091): under `mayflyd`, the manifest passed the
//! daemon's checks against its configuration; the image's content is verified by the data
//! plane at its pull (ADR 0087 decision 7): a drifted pull fails there (the placement `Failed`,
//! the pool not admissible on that host); recording `ImageRejected` on a drift is P17's pending
//! branch (planned, no task yet).

use mayfly_control_domain::image::{
    Image, ImageCommand, ImageManifest, ImageRejection, ImageStatus, ImageVerified, Verification,
};
use mayfly_control_domain::pool::{Pool, PoolCommand, PoolRejection};
use mayfly_control_domain::{Actor, Command, DomainEvent, Rejection};
use mayfly_domain::{CommandId, Epoch, EventId, ImageDigest, PoolName};

use crate::ids::derive;
use crate::journal::{ExpectedRevision, Journal, StreamId, StreamRevision};
use crate::placement::image_instance;
use crate::store::{
    AggregateStore, Clock, Outcome, Rng, SaveError, SaveRequest, StoreNotifier, stream_of,
};

const ID_DOMAIN: &[u8] = b"mayfly-image-catalog-v1\n";

/// Why a registration or a verification recorded nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// The `Image` aggregate refused, by its code (`manifest_immutable`: the digest is
    /// registered with another manifest).
    Refused(&'static str),
    /// The store gave no outcome.
    Save(SaveError),
}

impl CatalogError {
    /// A fixed `snake_case` code for logs and reports.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Refused(code) => code,
            Self::Save(_) => "save_failed",
        }
    }
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(code) => write!(f, "refused: {code}"),
            Self::Save(e) => write!(f, "not decided: {e}"),
        }
    }
}

impl std::error::Error for CatalogError {}

/// Why [`pin_image`] pinned nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinError {
    /// The image has no `ImageRegistered`.
    NotRegistered,
    /// The image is registered and not verified.
    NotVerified,
    /// The image was rejected: it is never verified again.
    Rejected,
    /// The image was withdrawn.
    Withdrawn,
    /// The `Pool` aggregate refused, by its code (`not_configured`, `decommissioned`).
    Refused(&'static str),
    /// The store gave no outcome.
    Save(SaveError),
}

impl PinError {
    /// A fixed `snake_case` code for logs and reports.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotRegistered => "not_registered",
            Self::NotVerified => "not_verified",
            Self::Rejected => "rejected",
            Self::Withdrawn => "withdrawn",
            Self::Refused(code) => code,
            Self::Save(_) => "save_failed",
        }
    }
}

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Save(e) => write!(f, "pin not decided: {e}"),
            other => write!(f, "pin refused: {}", other.code()),
        }
    }
}

impl std::error::Error for PinError {}

/// What [`pin_image`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pinned {
    /// `ImagePinned` was recorded, naming this `ImageVerified` event.
    Moved {
        /// The image's `ImageVerified` event.
        verified_ref: EventId,
    },
    /// The pool already pinned the digest: nothing recorded.
    Already,
}

fn id(parts: &[&[u8]]) -> CommandId {
    CommandId::from_u128(derive(ID_DOMAIN, parts))
}

fn encoded<T: serde::Serialize>(value: &T) -> Vec<u8> {
    // a value of these plain types always encodes; an empty part would still derive an id
    serde_json::to_vec(value).unwrap_or_default()
}

/// `RegisterImage` of `digest` with `manifest` by `actor` under `run_epoch`. The same
/// registration again records nothing; another manifest for a registered digest is refused
/// `manifest_immutable`.
///
/// # Errors
/// [`CatalogError`].
pub async fn register_image<J, C, R, N>(
    store: &AggregateStore<J, C, R, N>,
    digest: &ImageDigest,
    manifest: ImageManifest,
    actor: Actor,
    run_epoch: Epoch,
) -> Result<(), CatalogError>
where
    J: Journal,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
{
    let command = Command::new(
        id(&[
            b"register_image",
            digest.as_str().as_bytes(),
            &encoded(&manifest),
        ]),
        store.clock().now(),
        run_epoch,
        actor,
        ImageCommand::RegisterImage {
            digest: digest.clone(),
            manifest,
        },
    );
    decided(
        store
            .execute::<Image>(SaveRequest::new(image_instance(digest), command))
            .await,
    )
}

/// `RecordVerification` of `digest` with `outcome` by `actor` under `run_epoch`.
///
/// # Errors
/// [`CatalogError`]: `not_registered`, `already_rejected`, `withdrawn` among the refusals.
pub async fn record_verification<J, C, R, N>(
    store: &AggregateStore<J, C, R, N>,
    digest: &ImageDigest,
    outcome: Verification,
    actor: Actor,
    run_epoch: Epoch,
) -> Result<(), CatalogError>
where
    J: Journal,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
{
    let command = Command::new(
        id(&[
            b"record_verification",
            digest.as_str().as_bytes(),
            &encoded(&outcome),
        ]),
        store.clock().now(),
        run_epoch,
        actor,
        ImageCommand::RecordVerification { outcome },
    );
    decided(
        store
            .execute::<Image>(SaveRequest::new(image_instance(digest), command))
            .await,
    )
}

fn decided(saved: Result<Outcome<ImageRejection>, SaveError>) -> Result<(), CatalogError> {
    match saved {
        Ok(Outcome::Accepted { .. }) => Ok(()),
        Ok(Outcome::Refused { rejection, .. }) => Err(CatalogError::Refused(rejection.code())),
        Err(error) => Err(CatalogError::Save(error)),
    }
}

/// The id of the last `ImageVerified` event of `stream` up to `head`, if any.
async fn verified_event<J: Journal>(
    journal: &J,
    stream: &StreamId,
    head: Option<StreamRevision>,
) -> Result<Option<EventId>, SaveError> {
    let Some(head) = head else {
        return Ok(None);
    };
    let events = journal
        .read_stream(stream, None, Some(head))
        .await
        .map_err(SaveError::Storage)?;
    Ok(events
        .iter()
        .rev()
        .find(|e| e.kind.event_type == ImageVerified::EVENT_TYPE)
        .map(|e| e.event_id))
}

/// `PinImage` of `digest` on `pool` by `actor` under `run_epoch`, refused unless the image is
/// `Verified`, and saved with a read of the image stream at the revision the check read (module
/// docs). On a conflict the image and the pool are read again. Pinned by
/// `tests/store/images.rs::should_pin_the_pool_naming_the_verified_event_when_the_image_is_verified`,
/// `should_refuse_the_pin_when_the_image_is_withdrawn` and
/// `should_refuse_the_pin_at_commit_when_the_image_is_withdrawn_after_the_read`.
///
/// # Errors
/// [`PinError`].
pub async fn pin_image<J, C, R, N>(
    store: &AggregateStore<J, C, R, N>,
    pool: &PoolName,
    digest: &ImageDigest,
    actor: Actor,
    run_epoch: Epoch,
) -> Result<Pinned, PinError>
where
    J: Journal,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
{
    let decided: Result<(Outcome<PoolRejection>, EventId), PinError> = store
        .with_revision_retry(|| async move {
            let instance = image_instance(digest);
            let (image, head) = store.load_with_head::<Image>(instance).await?;
            let refusal = match image.status {
                ImageStatus::Verified => None,
                ImageStatus::Unregistered => Some(PinError::NotRegistered),
                ImageStatus::Registered => Some(PinError::NotVerified),
                ImageStatus::Rejected => Some(PinError::Rejected),
                ImageStatus::Withdrawn => Some(PinError::Withdrawn),
            };
            if let Some(refusal) = refusal {
                return Ok(Err(refusal));
            }
            let stream = stream_of::<Image>(instance).map_err(SaveError::InvalidName)?;
            let Some(verified_ref) = verified_event(store.journal(), &stream, head).await? else {
                return Ok(Err(PinError::NotVerified));
            };
            let (_, pool_head) = store.load_with_head::<Pool>(pool.as_str()).await?;
            let read = pool_head.map_or(0, |h| h.get().saturating_add(1));
            let command = Command::new(
                id(&[
                    b"pin_image",
                    pool.as_str().as_bytes(),
                    digest.as_str().as_bytes(),
                    &read.to_be_bytes(),
                ]),
                store.clock().now(),
                run_epoch,
                actor,
                PoolCommand::PinImage {
                    digest: digest.clone(),
                    verified_ref: Some(verified_ref),
                },
            );
            let request = SaveRequest::new(pool.as_str(), command)
                .with_read(stream, ExpectedRevision::of(head));
            let outcome = store.save::<Pool>(request).await?;
            Ok(Ok((outcome, verified_ref)))
        })
        .await
        .map_err(PinError::Save)?;
    match decided? {
        (Outcome::Accepted { appended, .. }, verified_ref) => {
            if appended.streams.is_empty() {
                Ok(Pinned::Already)
            } else {
                Ok(Pinned::Moved { verified_ref })
            }
        }
        (Outcome::Refused { rejection, .. }, _) => Err(PinError::Refused(rejection.code())),
    }
}

#[cfg(test)]
mod tests {
    use super::PinError;

    #[test]
    fn should_name_each_refusal_by_a_fixed_code_when_reported() {
        // given
        let errors = [
            PinError::NotRegistered,
            PinError::NotVerified,
            PinError::Rejected,
            PinError::Withdrawn,
            PinError::Refused("not_configured"),
        ];

        // when
        let codes: Vec<&str> = errors.iter().map(PinError::code).collect();

        // then
        assert_eq!(
            codes,
            [
                "not_registered",
                "not_verified",
                "rejected",
                "withdrawn",
                "not_configured"
            ]
        );
    }
}
