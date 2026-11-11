//! The effective operating mode (`doc/domain-model.md` §8): every combination of flags and its
//! mode and cause chain, pinned in `tests/fixtures/matrix/mode.txt`, and the precedence stated
//! independently of the implementation.

use std::fmt::Write as _;

use mayfly_control_domain::operations::{EffectiveMode, ModeFlags, PRECEDENCE, causes, mode};

const GOLDEN: &str = include_str!("fixtures/matrix/mode.txt");

/// Every combination of the seven flags.
fn combinations() -> Vec<ModeFlags> {
    (0_u8..128)
        .map(|bits| ModeFlags {
            safe_mode: bits & 1 != 0,
            recovering: bits & 2 != 0,
            draining: bits & 4 != 0,
            paused: bits & 8 != 0,
            tripped: bits & 16 != 0,
            admission_closed: bits & 32 != 0,
            degraded: bits & 64 != 0,
        })
        .collect()
}

fn flag_names(flags: &ModeFlags) -> String {
    let ModeFlags {
        safe_mode,
        recovering,
        draining,
        paused,
        tripped,
        admission_closed,
        degraded,
    } = *flags;
    let named = [
        (safe_mode, "safe_mode"),
        (recovering, "recovering"),
        (draining, "draining"),
        (paused, "paused"),
        (tripped, "tripped"),
        (admission_closed, "admission_closed"),
        (degraded, "degraded"),
    ];
    let set: Vec<&str> = named
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, n)| *n)
        .collect();
    if set.is_empty() {
        "none".to_owned()
    } else {
        set.join("+")
    }
}

#[test]
fn should_define_every_combination_when_each_flag_meets_each_other() {
    // given
    let all = combinations();
    let mut text =
        String::from("# mode: every combination of flags -> effective mode [cause chain]\n");
    // when
    for flags in &all {
        let _ = writeln!(
            text,
            "{} -> {:?} {:?}",
            flag_names(flags),
            mode(flags),
            causes(flags)
        );
    }
    // then
    assert_eq!(all.len(), 128);
    assert!(
        GOLDEN == text,
        "the table differs from tests/fixtures/matrix/mode.txt; if these outcomes are intended, commit it as:\n{text}"
    );
}

#[test]
fn should_choose_the_highest_precedence_cause_when_flags_combine() {
    // given: the precedence of `AdmissionExplain`, written out as a chain of tests
    let expected = |f: &ModeFlags| {
        if f.safe_mode {
            EffectiveMode::SafeMode
        } else if f.recovering {
            EffectiveMode::Recovering
        } else if f.draining {
            EffectiveMode::Draining
        } else if f.paused {
            EffectiveMode::Paused
        } else if f.tripped {
            EffectiveMode::Tripped
        } else if f.admission_closed {
            EffectiveMode::AdmissionClosed
        } else if f.degraded {
            EffectiveMode::Degraded
        } else {
            EffectiveMode::Operating
        }
    };
    // when
    let wrong: Vec<String> = combinations()
        .iter()
        .filter(|f| {
            mode(f) != expected(f)
                || causes(f)
                    .first()
                    .copied()
                    .unwrap_or(EffectiveMode::Operating)
                    != mode(f)
        })
        .map(flag_names)
        .collect();
    // then
    assert!(wrong.is_empty(), "{wrong:?}");
    assert!(
        PRECEDENCE.windows(2).all(|w| w[0] < w[1]),
        "the precedence is ordered"
    );
    assert!(!PRECEDENCE.contains(&EffectiveMode::Operating));
}
