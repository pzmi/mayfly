//! [`LinkSink`]: the control plane's [`DirectiveSink`] over the link (plan §3.2, policy P7): it
//! turns a committed `DirectiveIssued` into the protocol's `Directive`, stamped with the host
//! session's current epoch, and hands it to the link's sending half within its own deadline.
//!
//! **Each directive is judged on its own** (the ES-05F note of CP-02): sends are at least once
//! and not ordered across a relay takeover, so the sink never assumes "the one after the last
//! sent". It reads the attempt's state and sends a directive only while it is the attempt's
//! pending directive, in flight, decided under the session's epoch; anything else is dropped
//! as answered or superseded (a late directive 5 after 6 is not sent). A directive still
//! pending is sent again on every redelivery, with its one `directive_id`, which the data plane
//! executes at most once per epoch (`formal/link.qnt`, `executesAtMostOncePerEpoch`).
//!
//! **Safe mode holds effects** (I29): a kind the protocol marks
//! [`mayfly_protocol::DirectiveKind::held_in_safe_mode`] is not sent while the host's session
//! is in safe mode; the sink answers [`SinkError`] `held_in_safe_mode`, so the relay keeps its
//! checkpoint and delivers it again after a backoff, and it is sent once the mode is normal
//! (if it is still pending then).

use mayfly_control_domain::attempt::{
    Attempt, AttemptCommand, DirectiveKind as Decided, Identity, Outcome,
    OwnershipProof as DecidedProof,
};
use mayfly_control_domain::{Actor, Command};
use mayfly_domain::{Deadline, DirectiveId, ImageDigest, Millis, PoolName};
use mayfly_protocol::{
    AttemptRef, BaseName, Directive, DirectiveKind, DirectiveSink as Link, LinkError, Mode, Nonce,
    OwnershipProof, RunOptions, SecretHandle,
};

use super::relay::{DirectiveSink, RelayedDirective, SinkError};
use super::worker::within;
use super::{DeliveryConfig, DeliveryTimer};
use crate::journal::Journal;
use crate::saga::{DirectiveTimeouts, sent_of};
use crate::sessions::HostSessions;
use crate::store::{AggregateStore, Clock, Rng, SaveError, SaveRequest, StoreNotifier};

/// The relay's sink settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkConfig {
    /// Each kind's time from its first send: the deadline the frame carries.
    pub timeouts: DirectiveTimeouts,
    /// The deadline wanted for one send, bounded by [`send_deadline`].
    pub send: Millis,
}

impl Default for SinkConfig {
    /// The default timeouts, and five seconds for a send.
    fn default() -> Self {
        Self {
            timeouts: DirectiveTimeouts::default(),
            send: Millis::new(5_000),
        }
    }
}

/// What a directive needs beyond the attempt's stream: the pool's configuration and the image
/// placement (composition reads them from the config and the image streams), and the guest
/// challenge.
pub trait DirectiveDetails: Send + Sync {
    /// The local image base a clone of `digest` for `pool` is made from (ADR 0039); `None`
    /// while the image is not materialised on the host.
    fn image_base(&self, pool: &PoolName, digest: &ImageDigest) -> Option<BaseName>;

    /// How `pool`'s VMs run; `None` for a pool this build does not know.
    fn run_options(&self, pool: &PoolName) -> Option<RunOptions>;

    /// How long `pool`'s guests may take to shut down on a stop.
    fn stop_grace(&self, pool: &PoolName) -> Millis;

    /// The challenge of the `VerifyGuest` directive `directive_id`: the same on every resend
    /// of that directive, so a repeat executes nothing new on the data plane.
    fn nonce(&self, directive_id: DirectiveId) -> Nonce;
}

/// What the sink did with the directives handed to it, for status and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SinkStats {
    /// Handed to the link.
    pub sent: u64,
    /// Dropped: no longer the attempt's pending, in-flight directive.
    pub superseded: u64,
    /// Not sent because the host's session is in safe mode.
    pub held: u64,
}

/// The directive relay's sink over a link's sending half `L`, reading attempts and recording
/// `DirectiveSent` through `store`.
pub struct LinkSink<'s, J, C, R, N, L, T, D> {
    store: &'s AggregateStore<J, C, R, N>,
    link: L,
    timer: T,
    details: D,
    sessions: HostSessions,
    timeouts: DirectiveTimeouts,
    send_deadline: Millis,
    stats: std::sync::Mutex<SinkStats>,
}

impl<J, C, R, N, L, T, D> std::fmt::Debug for LinkSink<'_, J, C, R, N, L, T, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkSink")
            .field("send_deadline", &self.send_deadline)
            .finish_non_exhaustive()
    }
}

/// The deadline a send gets under `delivery`: `wanted`, at most half the relay's handle budget
/// (and at least 1 ms), so the state read before it and the advance after it still fall inside
/// the budget, and the relay never cuts a send at its own deadline instead (DC-26). Pinned by
/// `tests/store/link_sink.rs::should_bound_the_send_below_the_handle_budget_when_built`.
#[must_use]
pub const fn send_deadline(wanted: Millis, delivery: &DeliveryConfig) -> Millis {
    let ceiling = delivery.handle_budget().get() / 2;
    let bounded = if wanted.get() < ceiling {
        wanted.get()
    } else {
        ceiling
    };
    Millis::new(if bounded == 0 { 1 } else { bounded })
}

fn proof(p: &DecidedProof) -> OwnershipProof {
    OwnershipProof {
        attempt_id: p.attempt_id,
        host_id: p.host_id.clone(),
        slot: p.slot,
        generation: p.generation,
        vm_name: p.vm_name.clone(),
        instance_id: p.instance_id.clone(),
    }
}

/// The protocol's kind for a decided one: the VM name and proof from the attempt's stream
/// (`identity`), the base, run options, stop grace and nonce from `details`. Pinned by
/// `tests/store/link_sink.rs::should_compose_every_kind_from_the_stream_and_the_details_when_sent`.
///
/// # Errors
/// [`SinkError`] `image_base_unknown` for a clone of an image not materialised on the host,
/// `pool_unknown` for a start of a pool the details do not know.
pub fn compose<D: DirectiveDetails>(
    identity: &Identity,
    directive_id: DirectiveId,
    kind: &Decided,
    details: &D,
) -> Result<DirectiveKind, SinkError> {
    let vm_name = identity.vm_name.clone();
    Ok(match kind {
        Decided::CloneVm => DirectiveKind::CloneVm {
            base_name: details
                .image_base(&identity.pool, &identity.image_digest)
                .ok_or(SinkError::new("image_base_unknown"))?,
            vm_name,
        },
        Decided::StartVm => DirectiveKind::StartVm {
            vm_name,
            run_options: details
                .run_options(&identity.pool)
                .ok_or(SinkError::new("pool_unknown"))?,
        },
        Decided::VerifyGuest => DirectiveKind::VerifyGuest {
            vm_name,
            nonce: details.nonce(directive_id),
        },
        Decided::LaunchRunner { handle_id } => DirectiveKind::LaunchRunner {
            vm_name,
            launch: SecretHandle {
                handle_id: *handle_id,
            },
        },
        Decided::ExportDiagnostics => DirectiveKind::ExportDiagnostics { vm_name },
        Decided::StopVm { proof: p } => DirectiveKind::StopVm {
            grace: details.stop_grace(&identity.pool),
            proof: proof(p),
        },
        Decided::DeleteVm { proof: p } => DirectiveKind::DeleteVm { proof: proof(p) },
    })
}

impl<'s, J, C, R, N, L, T, D> LinkSink<'s, J, C, R, N, L, T, D>
where
    J: Journal,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
    L: Link,
    T: DeliveryTimer,
    D: DirectiveDetails,
{
    /// A sink over `store` sending over `link`, each send cut at
    /// [`send_deadline`]`(config.send, delivery)` by `timer`.
    pub fn new(
        store: &'s AggregateStore<J, C, R, N>,
        link: L,
        timer: T,
        details: D,
        sessions: HostSessions,
        config: SinkConfig,
        delivery: &DeliveryConfig,
    ) -> Self {
        Self {
            store,
            link,
            timer,
            details,
            sessions,
            timeouts: config.timeouts,
            send_deadline: send_deadline(config.send, delivery),
            stats: std::sync::Mutex::new(SinkStats::default()),
        }
    }

    /// The deadline each send gets.
    #[must_use]
    pub const fn deadline(&self) -> Millis {
        self.send_deadline
    }

    /// What the sink did so far.
    #[must_use]
    pub fn stats(&self) -> SinkStats {
        *self
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn count(&self, f: impl FnOnce(&mut SinkStats)) {
        f(&mut self
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner));
    }
}

impl<J, C, R, N, L, T, D> DirectiveSink for LinkSink<'_, J, C, R, N, L, T, D>
where
    J: Journal,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
    L: Link,
    T: DeliveryTimer,
    D: DirectiveDetails,
{
    /// Sends `relayed` if it is still the attempt's pending, in-flight directive decided under
    /// the host session's epoch, and drops it otherwise. Its first send fixes its deadline (now
    /// plus the kind's timeout) and, once the link took the frame, records `DirectiveSent` with
    /// it: only then does the saga's timer run, so a directive held unsent never times out. A
    /// resend carries the recorded deadline. The attempt is read through the store, so an event
    /// kind this build does not know raises safe mode as any load does. Pinned by
    /// `tests/store/link_sink.rs::should_drop_a_late_directive_when_a_later_one_is_pending`,
    /// `should_hold_create_and_destroy_kinds_when_the_session_is_in_safe_mode`,
    /// `should_stamp_the_session_epoch_when_a_pending_directive_is_sent` and
    /// `should_fix_the_deadline_at_the_first_send_when_a_directive_was_held`.
    async fn send(&self, relayed: RelayedDirective) -> Result<(), SinkError> {
        let directive = &relayed.directive;
        let instance = relayed
            .stream
            .as_str()
            .strip_prefix("attempt-")
            .ok_or(SinkError::new("attempt_unreadable"))?;
        let state = self
            .store
            .load::<Attempt>(instance)
            .await
            .map_err(|_| SinkError::new("attempt_unreadable"))?;
        let pending = state
            .pending
            .filter(|p| p.directive_id == directive.directive_id && p.outcome == Outcome::InFlight);
        let (Some(pending), Some(identity)) = (pending, &state.identity) else {
            self.count(|s| s.superseded += 1);
            return Ok(());
        };
        let session = self
            .sessions
            .get(&identity.host_id)
            .ok_or(SinkError::new("no_session"))?;
        if session.epoch != state.epoch {
            // decided under another run: sent only once the attempt is adopted in this session
            return Err(SinkError::new("attempt_not_adopted"));
        }
        let kind = compose(
            identity,
            directive.directive_id,
            &directive.kind,
            &self.details,
        )?;
        if session.mode == Mode::Safe && kind.held_in_safe_mode() {
            self.count(|s| s.held += 1);
            return Err(SinkError::new("held_in_safe_mode"));
        }
        let now = self.store.clock().now();
        let deadline = if pending.sent {
            pending.deadline
        } else {
            Deadline::after(now, self.timeouts.timeout(pending.tag))
        };
        let frame = Directive {
            directive_id: directive.directive_id,
            epoch: session.epoch,
            attempt: Some(AttemptRef {
                attempt_id: identity.attempt_id,
                generation: identity.generation,
            }),
            deadline,
            kind,
        };
        match within(self.timer.sleep(self.send_deadline), self.link.send(frame)).await {
            Some(Ok(())) => self.count(|s| s.sent += 1),
            Some(Err(LinkError::Closed)) => return Err(SinkError::new("link_closed")),
            Some(Err(
                LinkError::Refused(..) | LinkError::Version(..) | LinkError::Transport(..),
            )) => {
                return Err(SinkError::new("link_failed"));
            }
            None => return Err(SinkError::new("send_deadline")),
        }
        if pending.sent {
            return Ok(());
        }
        let command = Command::new(
            sent_of(directive.directive_id),
            now,
            session.epoch,
            Actor::Automation {
                policy: "directive_relay",
            },
            AttemptCommand::RecordDirectiveSent {
                directive_id: directive.directive_id,
                deadline,
            },
        );
        // accepted, refused (answered first) or recorded before by another worker: the send
        // is on record; anything else delivers it again, and the data plane dedupes the resend
        match self
            .store
            .execute::<Attempt>(SaveRequest::new(instance, command))
            .await
        {
            Ok(..) | Err(SaveError::CommandIdReused { .. }) => Ok(()),
            Err(..) => Err(SinkError::new("record_sent_failed")),
        }
    }
}
