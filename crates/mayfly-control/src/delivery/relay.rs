//! The `directive_relay` consumer (`doc/event-store.md` §5.2, §5.3; ADR 0034): delivers every
//! committed `DirectiveIssued` event, at least once, to a [`DirectiveSink`]; the sink over the
//! link, which composes the protocol `Directive`, is [`super::LinkSink`].

use std::fmt;
use std::future::Future;

use mayfly_control_domain::DomainEvent;
use mayfly_control_domain::attempt::{AttemptEvent, DirectiveIssued};
use mayfly_domain::EventId;

use super::Consumer;
use super::values::{ConsumerError, Delivery, LeaseFence};
use crate::journal::{StreamId, StreamRevision};
use crate::store::decode_event;

/// A committed directive handed to the sink, with where it was recorded and the fence of the
/// claim it is delivered under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayedDirective {
    /// The attempt's stream.
    pub stream: StreamId,
    /// The event's revision in it.
    pub revision: StreamRevision,
    /// The event's id.
    pub event_id: EventId,
    /// The claim's fence.
    pub fence: LeaseFence,
    /// The directive as decided: its `directive_id` is what the data plane dedupes by, so a
    /// relay that dies between the send and the advance resends the same id (CP-02).
    pub directive: DirectiveIssued,
}

/// Why the sink did not take a directive: a fixed `snake_case` code (ES-I13). The directive is
/// delivered again after a backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkError {
    code: &'static str,
}

impl SinkError {
    /// A failure named by `code`.
    #[must_use]
    pub const fn new(code: &'static str) -> Self {
        Self { code }
    }

    /// The code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for SinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "directive sink failed: {}", self.code)
    }
}

impl std::error::Error for SinkError {}

/// Where the relay hands directives: the link, through [`super::LinkSink`]. The host epoch is
/// stamped at send time, never stored (§5.3).
///
/// **Order is not promised.** Within one claim the relay sends a stream's directives in
/// revision order, but across a takeover sends may overlap and arrive out of order: a stale
/// worker A whose lease ran out mid-`send` can still put directive 5 on the link after worker B,
/// under a higher fence, sent 5 and 6. The checkpoint stays safe (A's advance is refused, DC-11);
/// the link does not. So the sink, and the data plane behind it, must not rely on per-stream
/// order: they dedupe by `directive_id` (or the event id) and judge each directive on its own
/// (generation, host epoch, the attempt's state), never as "the next one after the last seen".
///
/// **Deadline.** `send` must finish well within the relay's lease (`DeliveryConfig::lease_ttl`):
/// the worker cuts a `handle` past [`super::DeliveryConfig::handle_budget`] and delivers the
/// directive again after a backoff (DC-26). A link that may block puts its own, shorter
/// deadline on the send and answers a [`SinkError`] when it passes.
pub trait DirectiveSink: Send + Sync {
    /// Sends one directive, at least once per committed `DirectiveIssued`; possibly again, and
    /// possibly after a later directive of the same stream (see the trait).
    fn send(
        &self,
        directive: RelayedDirective,
    ) -> impl Future<Output = Result<(), SinkError>> + Send;
}

/// The `directive_relay` consumer: hands each `DirectiveIssued` event to its sink and lets every
/// other event pass. Its status is its checkpoint: `Sent` is "advanced past", and the directive's
/// outcome is the attempt's state, recorded by observation commands (§5.3).
///
/// A `directive_issued` event this build cannot decode (a newer schema version) is an error, so
/// its stream stops for the relay until a build that knows it runs: never skipped (ES-I15). Pinned
/// by `tests/store/relay.rs`.
#[derive(Debug, Clone)]
pub struct DirectiveRelay<S> {
    sink: S,
}

impl<S> DirectiveRelay<S> {
    /// A relay into `sink`.
    pub const fn new(sink: S) -> Self {
        Self { sink }
    }

    /// The sink.
    pub const fn sink(&self) -> &S {
        &self.sink
    }
}

impl<S: DirectiveSink> Consumer for DirectiveRelay<S> {
    const NAME: &'static str = "directive_relay";

    async fn handle(&self, delivery: Delivery<'_>) -> Result<(), ConsumerError> {
        let event = delivery.event;
        if event.kind.event_type != DirectiveIssued::EVENT_TYPE {
            return Ok(());
        }
        let Ok(AttemptEvent::DirectiveIssued(directive)) = decode_event::<AttemptEvent>(event)
        else {
            return Err(ConsumerError::new("directive_undecodable"));
        };
        self.sink
            .send(RelayedDirective {
                stream: event.stream.clone(),
                revision: event.revision,
                event_id: event.event_id,
                fence: delivery.fence,
                directive,
            })
            .await
            .map_err(|e| ConsumerError::new(e.code()))
    }
}
