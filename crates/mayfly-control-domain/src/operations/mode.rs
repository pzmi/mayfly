//! The effective operating mode (`doc/domain-model.md` §5.3, §8 `OperatingMode`): a pure
//! function over the flags the aggregates' states give, never stored as one enum.
//!
//! The precedence is [`PRECEDENCE`], the head of `AdmissionExplain`'s cause chain: safe mode,
//! then recovering, drain, pause, a tripped breaker, admission closed for another cause, and
//! degraded; with no flag set the host is operating. [`mode`] is the first set flag in that
//! order and [`causes`] every set flag in it, so the operator sees the one that wins and those
//! behind it. Every combination is pinned in `tests/fixtures/matrix/mode.txt` by
//! `tests/mode.rs::should_define_every_combination_when_each_flag_meets_each_other`, and the
//! winner is the highest-precedence set flag by
//! `tests/mode.rs::should_choose_the_highest_precedence_cause_when_flags_combine`.

/// What the host's and its pools' states say, flag by flag. The projection fills it from
/// `HostRun` (safe mode, recovering), the `Host` gate and terms (drain, admission closed),
/// the `Pool` (drain, pause, breaker) and the `ScaleSet`'s session (degraded).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModeFlags {
    /// `HostRun.safe_mode` is set.
    pub safe_mode: bool,
    /// The run has not completed its start-up recovery.
    pub recovering: bool,
    /// A host or pool drain is under way or done.
    pub draining: bool,
    /// The pool is paused.
    pub paused: bool,
    /// The pool's breaker for its pin is open (stage failures or an expired runner version).
    pub tripped: bool,
    /// The host gate is closed for another cause (unsupported Tart, a wake, an incomplete
    /// inventory streak, low disk).
    pub admission_closed: bool,
    /// An external dependency is impaired while the journal is trusted (GitHub, provider).
    pub degraded: bool,
}

/// The effective mode, one per flag, and `Operating` when none is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EffectiveMode {
    /// Nothing is executed or minted (I29).
    SafeMode,
    /// The start-up recovery runs.
    Recovering,
    /// Draining.
    Draining,
    /// Paused.
    Paused,
    /// A breaker is open.
    Tripped,
    /// Admission is closed for another cause.
    AdmissionClosed,
    /// Degraded: nothing is decided from missing external data.
    Degraded,
    /// Operating normally.
    Operating,
}

/// The causes, highest precedence first.
pub const PRECEDENCE: [EffectiveMode; 7] = [
    EffectiveMode::SafeMode,
    EffectiveMode::Recovering,
    EffectiveMode::Draining,
    EffectiveMode::Paused,
    EffectiveMode::Tripped,
    EffectiveMode::AdmissionClosed,
    EffectiveMode::Degraded,
];

/// Whether `flags` sets the flag of `mode` (`Operating` is never a flag).
#[must_use]
pub const fn is_set(flags: &ModeFlags, mode: EffectiveMode) -> bool {
    match mode {
        EffectiveMode::SafeMode => flags.safe_mode,
        EffectiveMode::Recovering => flags.recovering,
        EffectiveMode::Draining => flags.draining,
        EffectiveMode::Paused => flags.paused,
        EffectiveMode::Tripped => flags.tripped,
        EffectiveMode::AdmissionClosed => flags.admission_closed,
        EffectiveMode::Degraded => flags.degraded,
        EffectiveMode::Operating => false,
    }
}

/// Every set flag, highest precedence first: the cause chain.
#[must_use]
pub fn causes(flags: &ModeFlags) -> Vec<EffectiveMode> {
    PRECEDENCE
        .into_iter()
        .filter(|mode| is_set(flags, *mode))
        .collect()
}

/// The effective mode: the highest-precedence set flag, or `Operating`.
#[must_use]
pub fn mode(flags: &ModeFlags) -> EffectiveMode {
    PRECEDENCE
        .into_iter()
        .find(|mode| is_set(flags, *mode))
        .unwrap_or(EffectiveMode::Operating)
}
