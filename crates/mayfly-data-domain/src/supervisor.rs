//! The VM supervisor (plan §6, ADR 0037): a VM's state is derived from what is observed, the
//! provider's listing and the shim's lock and `exit.json`, never remembered, so a restarted data
//! plane re-adopts its VMs from the same facts.
//!
//! [`VmState`] is a pure function of [`VmFacts`]; [`step`] folds one observed fact into them;
//! [`signal`] names what a change means to report. The whole table is pinned by
//! `tests/data_plane/matrix.rs::should_derive_every_state_and_signal_when_each_fact_meets_each_observation`
//! (fixture `matrix/supervisor.txt`).

/// What a complete inventory says about one VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// Not listed.
    Absent,
    /// Listed, not running.
    Stopped,
    /// Listed, running.
    Running,
}

/// What the VM's shim files say (ADR 0037).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shim {
    /// No shim was ever started for it: no lock file, no `exit.json`.
    NeverStarted,
    /// The shim holds its lock: its `tart run` child lives.
    Alive,
    /// The shim wrote `exit.json`: the run ended, its status is known.
    Exited,
    /// The shim's lock is free and there is no `exit.json`: the process was lost.
    Lost,
}

/// The facts the state is derived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmFacts {
    /// The last complete inventory's word on it; `None` before any (I25: nothing is absent
    /// until a complete inventory says so).
    pub seen: Option<Seen>,
    /// Its shim.
    pub shim: Shim,
}

impl VmFacts {
    /// Before any observation.
    pub const UNOBSERVED: Self = Self {
        seen: None,
        shim: Shim::NeverStarted,
    };
}

/// One observed fact about a VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmFact {
    /// A complete inventory.
    Inventory(Seen),
    /// An incomplete inventory: changes nothing (I25).
    InventoryIncomplete,
    /// The shim files read now.
    Shim(Shim),
}

/// A VM's derived state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    /// No complete inventory has listed or omitted it, and no shim says more.
    Unobserved,
    /// A complete inventory omits it.
    Absent,
    /// Listed, never started.
    Present,
    /// Running under its live shim.
    Running,
    /// Its run ended and `exit.json` says how.
    Stopped,
    /// It runs without our live shim, or its run ended with no `exit.json`: the process that
    /// owned it is lost (the directive it served is `Unknown{process_lost}`).
    ProcessLost,
}

/// What a change of state means to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// A running VM stopped, not by a directive: `VmObservedStopped`.
    ObservedStopped,
    /// A VM we knew is gone from a complete inventory.
    Vanished,
    /// The process that owned the VM is lost.
    ProcessLost,
}

/// The state the facts show. The provider's listing decides existence; the shim decides
/// whether the run is ours and how it ended. Shim facts alone never make a VM absent.
#[must_use]
pub const fn state(facts: VmFacts) -> VmState {
    match (facts.seen, facts.shim) {
        (Some(Seen::Absent), Shim::NeverStarted | Shim::Alive | Shim::Exited | Shim::Lost) => {
            VmState::Absent
        }
        (None, Shim::NeverStarted) => VmState::Unobserved,
        (Some(Seen::Stopped), Shim::NeverStarted) => VmState::Present,
        (None | Some(Seen::Stopped | Seen::Running), Shim::Alive) => VmState::Running,
        (None | Some(Seen::Stopped), Shim::Exited) => VmState::Stopped,
        (None | Some(Seen::Stopped | Seen::Running), Shim::Lost)
        | (Some(Seen::Running), Shim::NeverStarted | Shim::Exited) => VmState::ProcessLost,
    }
}

/// Folds one fact into the facts: an inventory replaces the listing, an incomplete one is
/// ignored (I25), a shim reading replaces the shim.
#[must_use]
pub const fn step(facts: VmFacts, fact: VmFact) -> VmFacts {
    match fact {
        VmFact::Inventory(seen) => VmFacts {
            seen: Some(seen),
            shim: facts.shim,
        },
        VmFact::InventoryIncomplete => facts,
        VmFact::Shim(shim) => VmFacts {
            seen: facts.seen,
            shim,
        },
    }
}

/// What the change from `before` to `after` means to report, if anything.
#[must_use]
pub const fn signal(before: VmState, after: VmState) -> Option<Signal> {
    match after {
        VmState::ProcessLost => match before {
            VmState::ProcessLost => None,
            VmState::Unobserved
            | VmState::Absent
            | VmState::Present
            | VmState::Running
            | VmState::Stopped => Some(Signal::ProcessLost),
        },
        VmState::Stopped => match before {
            VmState::Running => Some(Signal::ObservedStopped),
            VmState::Unobserved
            | VmState::Absent
            | VmState::Present
            | VmState::Stopped
            | VmState::ProcessLost => None,
        },
        VmState::Absent => match before {
            VmState::Present | VmState::Running | VmState::Stopped | VmState::ProcessLost => {
                Some(Signal::Vanished)
            }
            VmState::Unobserved | VmState::Absent => None,
        },
        VmState::Unobserved | VmState::Present | VmState::Running => None,
    }
}
