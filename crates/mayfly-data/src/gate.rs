//! Listings against effects (the KER-08 review's note on `Ledger::apply_inventory`).
//!
//! A complete listing is the truth about our VMs and images: applied, it removes everything it
//! does not show. A listing that was read while a clone, start, stop, delete, pull,
//! materialise or evict ran may predate that effect, so applying it would forget a VM or an
//! image that exists and free its capacity, and reporting it would show the control plane an
//! absence that is not one. The executor therefore starts a listing only while no such effect
//! runs, and a listing during which one started anyway is discarded: never applied, never
//! reported.

/// Counts the effects that change what a listing shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListingGate {
    running: usize,
    started: u64,
}

/// What the gate looked like when a listing started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket {
    idle: bool,
    started: u64,
}

impl ListingGate {
    /// An effect that changes listings starts.
    pub const fn begin_effect(&mut self) {
        self.running += 1;
        self.started = self.started.wrapping_add(1);
    }

    /// That effect ended, whatever its outcome.
    pub const fn end_effect(&mut self) {
        self.running = self.running.saturating_sub(1);
    }

    /// No such effect runs: a listing started now can be clean.
    #[must_use]
    pub const fn idle(&self) -> bool {
        self.running == 0
    }

    /// A listing starts now.
    #[must_use]
    pub const fn begin_listing(&self) -> Ticket {
        Ticket {
            idle: self.idle(),
            started: self.started,
        }
    }

    /// Whether the listing of `ticket`, ending now, overlapped no effect: none ran when it
    /// started and none started since. Pinned by
    /// `tests::should_discard_a_listing_when_an_effect_ran_or_started_during_it` and
    /// `tests/executor/main.rs::should_discard_the_listing_when_it_overlaps_a_clone`.
    #[must_use]
    pub const fn clean(&self, ticket: Ticket) -> bool {
        ticket.idle && ticket.started == self.started
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_discard_a_listing_when_an_effect_ran_or_started_during_it() {
        // given
        let mut gate = ListingGate::default();
        let quiet = gate.begin_listing();
        gate.begin_effect();
        let during = gate.begin_listing();
        gate.end_effect();
        let after = gate.begin_listing();

        // when
        let verdicts = [gate.clean(quiet), gate.clean(during), gate.clean(after)];

        // then
        assert_eq!(verdicts, [false, false, true]);
        assert!(gate.idle());
    }

    #[test]
    fn should_keep_a_listing_when_no_effect_overlapped_it() {
        // given
        let mut gate = ListingGate::default();
        gate.begin_effect();
        gate.end_effect();

        // when
        let ticket = gate.begin_listing();

        // then
        assert!(gate.clean(ticket));
        gate.end_effect();
        assert!(gate.idle(), "an unmatched end never underflows");
    }
}
