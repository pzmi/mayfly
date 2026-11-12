//! The `placement_relay` consumer (policy P7 for image placements; `doc/event-store.md` §5.2,
//! §5.3; ADR 0034; CP-07): delivers every committed `PlacementDirectiveIssued`, at least once,
//! over the link as the protocol's `PullImage`, `MaterialiseImage` or `EvictImage`, stamped with
//! the host session's epoch, and records `RecordSent` with the deadline its first send fixed.
//!
//! A separate consumer from the attempts' `directive_relay`, with its own checkpoint: a
//! placement directive is judged against its placement stream, an attempt's against its attempt,
//! and neither waits on the other's stream.
//!
//! **Each directive is judged on its own**, as the attempts' sink judges them: sends are at
//! least once and not ordered across a takeover, so a directive is sent only while it is its
//! placement's pending, in-flight directive, and dropped as answered or superseded otherwise.
//! A directive still pending is sent again on every redelivery with its one id, which the data
//! plane executes at most once per epoch; under a new run's epoch it is executed again, which
//! the data plane's image store makes harmless (a pull of a present image, the materialisation
//! of a ready base and the eviction of an absent one each succeed doing nothing; DP-07).
//!
//! **Safe mode holds them** (I29): every placement kind is one
//! [`mayfly_protocol::DirectiveKind::held_in_safe_mode`] names, so in safe mode the sink answers
//! `held_in_safe_mode` and the relay delivers it again after a backoff.
//!
//! **Bases only by name** (ADR 0039): a `MaterialiseImage` or `EvictImage` names its base
//! `mfb-<digest12>-r<major>-<minor>-<patch>` (`image::base_name`), validated as a
//! [`BaseName`], which no OCI reference can be.

use mayfly_control_domain::image::{RunnerVersion, base_name};
use mayfly_control_domain::placement::{
    ImagePlacement, PendingOutcome, PlacementCommand, PlacementDirective, PlacementDirectiveIssued,
    PlacementEvent,
};
use mayfly_control_domain::{Actor, Command, DomainEvent};
use mayfly_domain::{Deadline, Millis};
use mayfly_protocol::{BaseName, Directive, DirectiveKind, DirectiveSink as Link, LinkError, Mode};

use super::worker::within;
use super::{Consumer, ConsumerError, Delivery, DeliveryConfig, DeliveryTimer, send_deadline};
use crate::journal::Journal;
use crate::placement::{PendingPlacement, PlacementIndex, PlacementTimeouts};
use crate::saga::sent_of;
use crate::sessions::HostSessions;
use crate::store::{
    AggregateStore, Clock, Rng, SaveError, SaveRequest, StoreNotifier, decode_event,
};

/// The relay's name: the delivery ledger's consumer and the actor of its commands.
pub const PLACEMENT_RELAY: &str = "placement_relay";

/// The runner version as the data plane's image store reads it: canonical `X.Y.Z`.
fn version_text(version: RunnerVersion) -> String {
    format!("{}.{}.{}", version.major, version.minor, version.patch)
}

/// The protocol's kind of a placement directive of `digest`. Pinned by
/// `tests/store/placement.rs::should_send_only_named_local_bases_when_placement_directives_are_relayed`.
///
/// # Errors
/// `base_name_malformed` when the base name is not a valid `mfb-*` name (never for a digest and
/// a runner version, whose name is built from hex digits and numbers).
pub fn compose_placement(
    digest: &mayfly_domain::ImageDigest,
    directive: PlacementDirective,
) -> Result<DirectiveKind, ConsumerError> {
    let base = |version| {
        BaseName::new(&base_name(digest, version))
            .map_err(|_| ConsumerError::new("base_name_malformed"))
    };
    Ok(match directive {
        PlacementDirective::PullImage => DirectiveKind::PullImage {
            digest: digest.clone(),
        },
        PlacementDirective::MaterialiseImage { runner_version } => {
            DirectiveKind::MaterialiseImage {
                digest: digest.clone(),
                base_name: base(runner_version)?,
                runner_version: version_text(runner_version),
            }
        }
        PlacementDirective::EvictImage { runner_version } => DirectiveKind::EvictImage {
            base_name: base(runner_version)?,
        },
    })
}

/// The relay's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementRelayConfig {
    /// Each kind's time from its first send: the deadline the frame carries.
    pub timeouts: PlacementTimeouts,
    /// The deadline wanted for one send, bounded by [`send_deadline`].
    pub send: Millis,
}

impl Default for PlacementRelayConfig {
    /// The default timeouts, and five seconds for a send.
    fn default() -> Self {
        Self {
            timeouts: PlacementTimeouts::default(),
            send: Millis::new(5_000),
        }
    }
}

/// The `placement_relay` consumer over the link's sending half `L`.
pub struct PlacementRelay<'s, J, C, R, N, L, T> {
    store: &'s AggregateStore<J, C, R, N>,
    link: L,
    timer: T,
    sessions: HostSessions,
    timeouts: PlacementTimeouts,
    send_deadline: Millis,
    index: PlacementIndex,
}

impl<J, C, R, N, L, T> std::fmt::Debug for PlacementRelay<'_, J, C, R, N, L, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlacementRelay")
            .field("send_deadline", &self.send_deadline)
            .finish_non_exhaustive()
    }
}

impl<'s, J, C, R, N, L, T> PlacementRelay<'s, J, C, R, N, L, T>
where
    J: Journal,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
    L: Link,
    T: DeliveryTimer,
{
    /// A relay over `store` sending over `link`, each send cut at
    /// [`send_deadline`]`(config.send, delivery)` by `timer`; every directive it sends is noted
    /// in `index` for the inbox.
    pub fn new(
        store: &'s AggregateStore<J, C, R, N>,
        link: L,
        timer: T,
        sessions: HostSessions,
        config: PlacementRelayConfig,
        delivery: &DeliveryConfig,
        index: PlacementIndex,
    ) -> Self {
        Self {
            store,
            link,
            timer,
            sessions,
            timeouts: config.timeouts,
            send_deadline: send_deadline(config.send, delivery),
            index,
        }
    }
}

impl<J, C, R, N, L, T> Consumer for PlacementRelay<'_, J, C, R, N, L, T>
where
    J: Journal,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
    L: Link,
    T: DeliveryTimer,
{
    const NAME: &'static str = PLACEMENT_RELAY;

    /// Sends the event's directive if it is still its placement's pending, in-flight directive;
    /// its first send fixes the deadline (now plus the kind's timeout) and records `RecordSent`
    /// once the link took the frame, so a directive held unsent never times out. Every other
    /// event passes. Pinned by
    /// `tests/store/placement.rs::should_place_the_pin_and_admit_the_pool_when_the_base_is_materialised`.
    async fn handle(&self, delivery: Delivery<'_>) -> Result<(), ConsumerError> {
        let event = delivery.event;
        if event.kind.event_type != PlacementDirectiveIssued::EVENT_TYPE {
            return Ok(());
        }
        let Ok(PlacementEvent::PlacementDirectiveIssued(issued)) =
            decode_event::<PlacementEvent>(event)
        else {
            return Err(ConsumerError::new("directive_undecodable"));
        };
        let instance = event
            .stream
            .as_str()
            .strip_prefix("placement-")
            .ok_or(ConsumerError::new("placement_unreadable"))?;
        let state = self
            .store
            .load::<ImagePlacement>(instance)
            .await
            .map_err(|_| ConsumerError::new("placement_unreadable"))?;
        let pending = state.pending.filter(|p| {
            p.directive_id == issued.directive_id && p.outcome == PendingOutcome::InFlight
        });
        let (Some(pending), Some(host), Some(digest)) = (pending, &state.host_id, &state.digest)
        else {
            return Ok(());
        };
        let session = self
            .sessions
            .get(host)
            .ok_or(ConsumerError::new("no_session"))?;
        let kind = compose_placement(digest, pending.directive)?;
        if session.mode == Mode::Safe && kind.held_in_safe_mode() {
            return Err(ConsumerError::new("held_in_safe_mode"));
        }
        let now = self.store.clock().now();
        let deadline = if pending.sent {
            pending.deadline
        } else {
            Deadline::after(now, self.timeouts.timeout(pending.directive))
        };
        self.index.insert(
            pending.directive_id,
            PendingPlacement {
                instance: instance.to_owned(),
                host_id: host.clone(),
                digest: digest.clone(),
            },
        );
        let frame = Directive {
            directive_id: pending.directive_id,
            epoch: session.epoch,
            attempt: None,
            deadline,
            kind,
        };
        match within(self.timer.sleep(self.send_deadline), self.link.send(frame)).await {
            Some(Ok(())) => {}
            Some(Err(LinkError::Closed)) => return Err(ConsumerError::new("link_closed")),
            Some(Err(
                LinkError::Refused(..) | LinkError::Version(..) | LinkError::Transport(..),
            )) => return Err(ConsumerError::new("link_failed")),
            None => return Err(ConsumerError::new("send_deadline")),
        }
        if pending.sent {
            return Ok(());
        }
        let command = Command::new(
            sent_of(pending.directive_id),
            now,
            session.epoch,
            Actor::Automation {
                policy: PLACEMENT_RELAY,
            },
            PlacementCommand::RecordSent {
                directive_id: pending.directive_id,
                deadline,
            },
        );
        // accepted, refused (answered first) or recorded before by another worker: the send is
        // on record; anything else delivers it again, and the data plane dedupes the resend
        match self
            .store
            .execute::<ImagePlacement>(SaveRequest::new(instance, command))
            .await
        {
            Ok(..) | Err(SaveError::CommandIdReused { .. }) => Ok(()),
            Err(..) => Err(ConsumerError::new("record_sent_failed")),
        }
    }
}
