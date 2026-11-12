//! Policy P2 (`doc/domain-model.md` §7): a `SlotReserved` lease becomes an attempt.
//!
//! [`start_attempt`] folds the `Host` and the revision of the last event it folded in one read
//! ([`AggregateStore::load_with_head`]), takes the lease of the attempt from it and the pool's
//! pinned image, and saves `StartAttempt` with a read of the host stream **at that revision**
//! (ADR 0041): a release, a drain or any other write that
//! commits on the host stream between the read and the commit refuses the unit, and the retry
//! reads the host again, so an attempt is never started for a lease the host no longer holds
//! (CP-03F; `formal/scheduling.qnt`'s `staleUnstartedReadKeepsTheSlotTest`). Pinned by
//! `tests/store/start.rs::should_refuse_the_start_at_commit_when_the_host_stream_moves_after_the_read`.
//!
//! The pool's image is read without a condition: an attempt started on an image the pool
//! replaced a moment later is retired by the reconciler (`ImageReplaced`), as every attempt of
//! the old image is. **The image's base must be materialised on the host** (P17, CP-07): the
//! start reads the placement of the pinned image on the lease's host and is saved with a read
//! of that placement stream at the revision showing it `Materialised`, so an eviction (or any
//! write to the placement) between the read and the commit refuses the unit and the retry reads
//! the pin and the placement again; a pin not materialised there starts nothing
//! ([`Started::NotPlaced`]). Without it an attempt reserved under one pin and started after a
//! repin would take the new, unplaced image while the old base, no longer referenced, could be
//! evicted (`formal/scheduling.qnt`'s `startAttempt`, `startWithoutPlacementViolatesTest`;
//! pinned by `tests/store/start.rs::should_start_nothing_when_the_pin_is_not_materialised_on_the_host`
//! and `should_refuse_the_start_at_commit_when_the_placement_moves_after_the_read`).
//!
//! The command id is derived from the attempt and the host revision read, so a call repeated
//! over the same host revision is answered by its receipt and starts nothing twice, and a retry
//! after the host moved is a new decision.

use mayfly_control_domain::attempt::{Attempt, AttemptCommand, AttemptRejection, StartAttempt};
use mayfly_control_domain::host::Host;
use mayfly_control_domain::placement::{ImagePlacement, PlacementPhase, placement_instance};
use mayfly_control_domain::pool::Pool;
use mayfly_control_domain::{Actor, Command};
use mayfly_domain::{AttemptId, CommandId, Epoch, HostId, ImageDigest};

use crate::ids::derive;
use crate::journal::{Appended, ExpectedRevision, Journal};
use crate::store::{
    AggregateStore, Clock, Outcome, Rng, SaveError, SaveRequest, StoreNotifier, stream_of,
};

/// P2's name: the actor of its commands.
pub const START: &str = "attempt_start";

const ID_DOMAIN: &[u8] = b"mayfly-attempt-start-v1\n";

/// The id of the `StartAttempt` of `attempt` on `image` decided over host revision
/// `host_revision` (`None` for a host stream with no event), derived from the three: a start
/// decided again after a repin is a new command, never `command_id_reused`.
#[must_use]
pub fn start_command_id(
    attempt: AttemptId,
    host_revision: Option<u64>,
    image: &ImageDigest,
) -> CommandId {
    let revision = host_revision.map_or_else(Vec::new, |r| r.to_be_bytes().to_vec());
    CommandId::from_u128(derive(
        ID_DOMAIN,
        &[
            &attempt.as_u128().to_be_bytes(),
            &revision,
            image.as_str().as_bytes(),
        ],
    ))
}

/// What [`start_attempt`] asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRequest {
    /// The host whose lease starts the attempt.
    pub host: HostId,
    /// The attempt the reconciler minted with the reservation.
    pub attempt: AttemptId,
    /// Directives a stage may issue before a stage failure retires the attempt.
    pub retry_budget: u32,
    /// The run epoch the command is issued under.
    pub run_epoch: Epoch,
}

/// How a start ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    /// `AttemptStarted` committed (or a receipt of it answered: `replayed`).
    Started {
        /// What the unit appended.
        appended: Appended,
        /// Answered by an earlier run's receipt.
        replayed: bool,
    },
    /// The host holds no lease for the attempt (released, or never reserved): nothing started.
    NoLease,
    /// The lease's pool has no pinned image: nothing to start from.
    NoImage,
    /// The pinned image's base is not materialised on the host (P17): nothing to clone from.
    /// The caller waits for a placement kick (the placement stream moving) and starts again
    /// then; it neither releases the lease nor retries at once.
    NotPlaced,
    /// The attempt refused (`already_started`, `stale_run`).
    Refused(AttemptRejection),
}

/// Starts `request.attempt` from the host's lease, under a read of the host stream at the head
/// the lease was read from, retried by [`AggregateStore::with_revision_retry`] while that stream
/// moves (see the module docs).
///
/// # Errors
/// [`SaveError`]: a store fault, or [`SaveError::Conflict`] when the host stream kept moving
/// through every retry.
pub async fn start_attempt<J: Journal, C: Clock, R: Rng, N: StoreNotifier>(
    store: &AggregateStore<J, C, R, N>,
    request: &StartRequest,
) -> Result<Started, SaveError> {
    store
        .with_revision_retry(|| async {
            let host_stream =
                stream_of::<Host>(request.host.as_str()).map_err(SaveError::InvalidName)?;
            let (host, head) = store.load_with_head::<Host>(request.host.as_str()).await?;
            let Some(lease) = host.leases.get(&request.attempt) else {
                return Ok(Started::NoLease);
            };
            let pool = store.load::<Pool>(lease.pool.as_str()).await?;
            let Some(image_digest) = pool.image_pin else {
                return Ok(Started::NoImage);
            };
            let placement = placement_instance(&request.host, &image_digest);
            let placement_stream =
                stream_of::<ImagePlacement>(&placement).map_err(SaveError::InvalidName)?;
            let (placed, placed_head) = store.load_with_head::<ImagePlacement>(&placement).await?;
            if placed.phase != PlacementPhase::Materialised
                || placed.digest.as_ref() != Some(&image_digest)
            {
                return Ok(Started::NotPlaced);
            }
            let command_id =
                start_command_id(request.attempt, head.map(|r| r.get()), &image_digest);
            let body = AttemptCommand::StartAttempt(StartAttempt {
                attempt_id: request.attempt,
                host_id: request.host.clone(),
                slot: lease.slot,
                generation: lease.generation,
                pool: lease.pool.clone(),
                image_digest,
                retry_budget: request.retry_budget,
            });
            let command = Command::new(
                command_id,
                store.clock().now(),
                request.run_epoch,
                Actor::Automation { policy: START },
                body,
            );
            let save = SaveRequest::new(request.attempt, command)
                .with_read(host_stream, ExpectedRevision::of(head))
                .with_read(placement_stream, ExpectedRevision::of(placed_head));
            Ok(match store.save::<Attempt>(save).await? {
                Outcome::Accepted { appended, replayed } => Started::Started { appended, replayed },
                Outcome::Refused { rejection, .. } => Started::Refused(rejection),
            })
        })
        .await
}
