//! The `directive_relay` consumer (`doc/event-store.md` §5.2, §5.3; ADR 0034): delivers every
//! committed `DirectiveIssued` event, at least once, to a [`DirectiveSink`]. A skeleton: the
//! sink that composes the protocol `Directive` and sends it over the link is CP-02's.

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

/// Where the relay hands directives: the link, once CP-02 wires it. The host epoch is stamped by
/// the link adapter at send time, never stored (§5.3).
pub trait DirectiveSink: Send + Sync {
    /// Sends one directive.
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
