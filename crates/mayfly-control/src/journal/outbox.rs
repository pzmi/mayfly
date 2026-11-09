//! Outbox rows: the directives a unit of work asks the relay to send (plan §5.2), and the
//! status machine the journal holds them to.

use std::fmt;

use mayfly_domain::{AttemptId, DirectiveId, Generation, Millis};

use crate::journal::values::Payload;

/// Where a directive is on its way to the data plane.
///
/// ```text
/// Pending ──▶ Sent ──▶ Done            (an observation answered it)
///    │         │ └───▶ Unknown ──▶ Done (only an observation resolves it)
///    │         └─────▶ Pending          (definitely not delivered: retry, same directive id)
///    └──▶ Cancelled                     (never sent; the attempt moved on)
/// ```
///
/// `Done` and `Cancelled` are terminal. `Unknown` leaves only for `Done`, on an observation:
/// a timeout never turns into success or a resend (plan §6 rule 4). Pinned by
/// `should_allow_exactly_the_documented_transitions_when_every_pair_is_tried` and the journal
/// contract's `outbox_status_transitions` case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OutboxStatus {
    /// Waiting for the relay, at or after its `next_attempt_at`.
    Pending,
    /// Handed to the link; no answer yet.
    Sent,
    /// The send's outcome is ambiguous (a timeout, a lost reply).
    Unknown,
    /// An observation answered it.
    Done,
    /// Withdrawn before it was ever sent.
    Cancelled,
}

impl OutboxStatus {
    /// Every status, in declaration order.
    pub const ALL: [Self; 5] = [
        Self::Pending,
        Self::Sent,
        Self::Unknown,
        Self::Done,
        Self::Cancelled,
    ];

    /// Whether a row may move from `self` to `to`. Every pair is decided here, none by a
    /// wildcard.
    #[must_use]
    pub const fn allows(self, to: Self) -> bool {
        match self {
            Self::Pending => matches!(to, Self::Sent | Self::Cancelled),
            Self::Sent => matches!(to, Self::Pending | Self::Unknown | Self::Done),
            Self::Unknown => matches!(to, Self::Done),
            Self::Done | Self::Cancelled => false,
        }
    }

    /// The stable `snake_case` name a store writes and an operator sees.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Sent => "sent",
            Self::Unknown => "unknown",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }

    /// The status named `name` by [`OutboxStatus::as_str`], or `None`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }
}

impl fmt::Display for OutboxStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A directive to enqueue: stored as [`OutboxStatus::Pending`] with no attempts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDirective {
    /// The directive; unique across the outbox.
    pub directive_id: DirectiveId,
    /// The attempt it serves.
    pub attempt_id: AttemptId,
    /// The VM generation it targets (a retry keeps it, plan §6 rule 3).
    pub generation: Generation,
    /// The directive kind's `snake_case` name.
    pub kind: String,
    /// The encoded request. Never a secret: a `SecretHandle` at most (plan §3.2).
    pub request: Payload,
    /// The earliest instant the relay sends it (control-plane `Clock`, ADR 0020).
    pub next_attempt_at: Millis,
}

/// A status change of an existing row, applied only if the row is still in `from`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxTransition {
    /// The row.
    pub directive_id: DirectiveId,
    /// The status the writer read; a row in any other status refuses the change.
    pub from: OutboxStatus,
    /// The new status; must be allowed by [`OutboxStatus::allows`].
    pub to: OutboxStatus,
    /// The row's new `next_attempt_at` (a retry's backoff; otherwise the time it moved).
    pub next_attempt_at: Millis,
}

/// One outbox change in a unit of work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxWrite {
    /// Enqueue a new directive.
    Insert(NewDirective),
    /// Move an existing one.
    Transition(OutboxTransition),
}

impl OutboxWrite {
    /// The directive the write touches.
    #[must_use]
    pub const fn directive_id(&self) -> DirectiveId {
        match self {
            Self::Insert(d) => d.directive_id,
            Self::Transition(t) => t.directive_id,
        }
    }
}

/// An outbox row as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    /// See [`NewDirective::directive_id`].
    pub directive_id: DirectiveId,
    /// See [`NewDirective::attempt_id`].
    pub attempt_id: AttemptId,
    /// See [`NewDirective::generation`].
    pub generation: Generation,
    /// See [`NewDirective::kind`].
    pub kind: String,
    /// See [`NewDirective::request`].
    pub request: Payload,
    /// The current status.
    pub status: OutboxStatus,
    /// How many times it moved to [`OutboxStatus::Sent`].
    pub attempts: u32,
    /// See [`OutboxTransition::next_attempt_at`].
    pub next_attempt_at: Millis,
}

impl OutboxRow {
    /// The row a [`NewDirective`] becomes.
    #[must_use]
    pub fn pending(directive: NewDirective) -> Self {
        Self {
            directive_id: directive.directive_id,
            attempt_id: directive.attempt_id,
            generation: directive.generation,
            kind: directive.kind,
            request: directive.request,
            status: OutboxStatus::Pending,
            attempts: 0,
            next_attempt_at: directive.next_attempt_at,
        }
    }

    /// The row after `transition`, which the caller has checked (`from` matches, `allows`):
    /// the status and `next_attempt_at` change, and a move to `Sent` counts one attempt.
    #[must_use]
    pub fn moved(mut self, transition: &OutboxTransition) -> Self {
        if transition.to == OutboxStatus::Sent {
            self.attempts = self.attempts.saturating_add(1);
        }
        self.status = transition.to;
        self.next_attempt_at = transition.next_attempt_at;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_allow_exactly_the_documented_transitions_when_every_pair_is_tried() {
        // given
        use OutboxStatus::{Cancelled, Done, Pending, Sent, Unknown};
        let legal = [
            (Pending, Sent),
            (Pending, Cancelled),
            (Sent, Pending),
            (Sent, Unknown),
            (Sent, Done),
            (Unknown, Done),
        ];

        // when
        let allowed: Vec<_> = OutboxStatus::ALL
            .into_iter()
            .flat_map(|from| OutboxStatus::ALL.into_iter().map(move |to| (from, to)))
            .filter(|(from, to)| from.allows(*to))
            .collect();

        // then
        assert_eq!(allowed, legal);
    }

    #[test]
    fn should_round_trip_status_name_when_parsed() {
        // given
        let names: Vec<_> = OutboxStatus::ALL.iter().map(|s| s.as_str()).collect();

        // when
        let parsed: Vec<_> = names.iter().map(|n| OutboxStatus::parse(n)).collect();

        // then
        assert_eq!(parsed, OutboxStatus::ALL.map(Some));
        assert_eq!(OutboxStatus::parse("acked"), None);
    }
}
