//! The recovery decision table (plan §10) over every combination of its inputs: the rows of the
//! plan, the rules no row may break, and the whole table pinned in
//! `tests/fixtures/matrix/recovery.txt`, regenerated deliberately from this test's failure text
//! (ver §4.2), so a row that changes shows in review.

use std::fmt::Write as _;

use mayfly_control_domain::attempt::{
    DirectiveTag, Phase, QuarantineReason, RetireReason, Stage, VmCondition,
};
use mayfly_control_domain::operations::recovery::{
    Adoption, AttemptRecord, Decision, GithubView, JournalView, NameClass, ObservedVm, Proposal,
    ProviderView, SafeModeCause, Subject, Trust, VmRun, classify,
};
use mayfly_domain::ProviderInstanceId;

const GOLDEN: &str = include_str!("fixtures/matrix/recovery.txt");

const GITHUB: [GithubView; 7] = [
    GithubView::NotRegistered,
    GithubView::IssuedNotDelivered,
    GithubView::Online,
    GithubView::Busy,
    GithubView::Offline,
    GithubView::Absent,
    GithubView::Unreachable,
];

const CAUSES: [SafeModeCause; 4] = [
    SafeModeCause::EpochBelowFloor,
    SafeModeCause::UnknownEventKind,
    SafeModeCause::VerificationFailed,
    SafeModeCause::Recorded,
];

fn id(text: &str) -> ProviderInstanceId {
    ProviderInstanceId::new(text).unwrap_or_else(|e| panic!("{e}"))
}

fn phases() -> Vec<(&'static str, Phase)> {
    let backoff = |stage| Phase::Backoff { stage, n: 1 };
    vec![
        ("NotStarted", Phase::NotStarted),
        ("Reserved", Phase::Reserved),
        ("Cloning", Phase::Cloning),
        ("Booting", Phase::Booting),
        ("Verifying", Phase::Verifying),
        ("Prepared", Phase::Prepared),
        ("Activating", Phase::Activating),
        ("Listening", Phase::Listening),
        ("Busy", Phase::Busy),
        ("Finished", Phase::Finished),
        ("Retiring", Phase::Retiring),
        ("Deleting", Phase::Deleting),
        ("Deleted", Phase::Deleted),
        ("Lost", Phase::Lost),
        ("Backoff(clone)", backoff(Stage::Clone)),
        ("Backoff(boot)", backoff(Stage::Boot)),
        ("Backoff(verify)", backoff(Stage::Verify)),
        ("Backoff(activate)", backoff(Stage::Activate)),
        (
            "Quarantined(mismatch,running)",
            Phase::Quarantined {
                reason: QuarantineReason::ProviderIdentityMismatch,
                vm: VmCondition::Running,
            },
        ),
        (
            "Quarantined(doubt,stopped)",
            Phase::Quarantined {
                reason: QuarantineReason::OwnershipDoubt,
                vm: VmCondition::Stopped,
            },
        ),
        ("Closed", Phase::Closed),
    ]
}

fn pendings() -> Vec<(&'static str, Option<DirectiveTag>)> {
    vec![
        ("-", None),
        ("clone", Some(DirectiveTag::CloneVm)),
        ("delete", Some(DirectiveTag::DeleteVm)),
    ]
}

fn providers() -> Vec<(String, ProviderView)> {
    let mut out = vec![
        ("incomplete".to_owned(), ProviderView::Incomplete),
        ("absent".to_owned(), ProviderView::Absent),
    ];
    let runs = [
        ("running+shim", VmRun::Running { shim_alive: true }),
        ("running-shim", VmRun::Running { shim_alive: false }),
        ("stopped+exit", VmRun::Stopped { exit_json: true }),
        ("stopped-exit", VmRun::Stopped { exit_json: false }),
    ];
    for instance in ["a", "b"] {
        for (name, run) in runs {
            out.push((
                format!("{instance}:{name}"),
                ProviderView::Present(ObservedVm {
                    instance_id: id(instance),
                    run,
                }),
            ));
        }
    }
    out
}

/// One row's subject and its label.
fn subjects() -> Vec<(String, Subject)> {
    let mut out = Vec::new();
    for (phase_name, phase) in phases() {
        for (pending_name, pending) in pendings() {
            for recorded in [None, Some("a")] {
                out.push((
                    format!(
                        "{phase_name} pending={pending_name} recorded={}",
                        recorded.unwrap_or("-")
                    ),
                    Subject::Attempt(AttemptRecord {
                        phase,
                        instance_id: recorded.map(id),
                        pending,
                    }),
                ));
            }
        }
    }
    for (name, class) in [
        ("unrecorded mf-*", NameClass::ManagedLooking),
        ("unrecorded base", NameClass::ImageBase),
        ("unrecorded foreign", NameClass::Foreign),
    ] {
        out.push((name.to_owned(), Subject::Unrecorded(class)));
    }
    out
}

/// Every input of the table: every subject, provider view and GitHub view, under a trusted
/// journal and under every safe-mode cause.
fn all_rows() -> Vec<(JournalView, ProviderView, GithubView)> {
    let trusts: Vec<Trust> = std::iter::once(Trust::Trusted)
        .chain(CAUSES.into_iter().map(Trust::SafeMode))
        .collect();
    let mut out = Vec::new();
    for trust in trusts {
        for (_, subject) in subjects() {
            for (_, provider) in providers() {
                for github in GITHUB {
                    out.push((
                        JournalView {
                            trust,
                            subject: subject.clone(),
                        },
                        provider.clone(),
                        github,
                    ));
                }
            }
        }
    }
    out
}

fn retire_code(reason: RetireReason) -> &'static str {
    match reason {
        RetireReason::JobDone => "job_done",
        RetireReason::Surplus => "surplus",
        RetireReason::Drain => "drain",
        RetireReason::ImageReplaced => "image_replaced",
        RetireReason::StageFailed => "stage_failed",
        RetireReason::GuestDied => "guest_died",
        RetireReason::VmStopped => "vm_stopped",
        RetireReason::JitLost => "jit_lost",
        RetireReason::JitPresumedLeaked => "jit_presumed_leaked",
        RetireReason::LaunchRefused => "launch_refused",
        RetireReason::QuarantineResolved => "quarantine_resolved",
    }
}

fn quarantine_code(reason: QuarantineReason) -> &'static str {
    match reason {
        QuarantineReason::OwnershipDoubt => "ownership_doubt",
        QuarantineReason::GuestIdentityRefused => "guest_identity_refused",
        QuarantineReason::JitPresumedLeaked => "jit_presumed_leaked",
        QuarantineReason::ProviderIdentityMismatch => "provider_identity_mismatch",
        QuarantineReason::DeleteRefusedRepeatedly => "delete_refused_repeatedly",
        QuarantineReason::RedactionViolation => "redaction_violation",
        QuarantineReason::ForensicRetention => "forensic_retention",
        QuarantineReason::OutcomeUnprovable => "outcome_unprovable",
    }
}

fn proposal_code(proposal: Proposal) -> String {
    match proposal {
        Proposal::WaitForInventory => "wait".to_owned(),
        Proposal::Adopt => "adopt".to_owned(),
        Proposal::MarkLost => "mark_lost".to_owned(),
        Proposal::Quarantine(reason) => format!("quarantine({})", quarantine_code(reason)),
        Proposal::Keep => "keep".to_owned(),
        Proposal::Report => "report".to_owned(),
        Proposal::Ignore => "ignore".to_owned(),
    }
}

fn code(decision: &Decision) -> String {
    match decision {
        Decision::Wait => "wait".to_owned(),
        Decision::Adopt(Adoption::Continue) => "adopt".to_owned(),
        Decision::Adopt(Adoption::Reverify) => "adopt_reverify".to_owned(),
        Decision::Adopt(Adoption::Monitor) => "adopt_monitor".to_owned(),
        Decision::Adopt(Adoption::Clone(instance)) => format!("adopt_clone({instance})"),
        Decision::RetryClone => "retry_clone".to_owned(),
        Decision::Retire(reason) => format!("retire({})", retire_code(*reason)),
        Decision::PresumeLeaked => "presume_leaked".to_owned(),
        Decision::ConfirmDeleted => "confirm_deleted".to_owned(),
        Decision::MarkLost => "mark_lost".to_owned(),
        Decision::DeleteLeaked => "delete_leaked".to_owned(),
        Decision::SuspendCleanup => "suspend_cleanup".to_owned(),
        Decision::Quarantine(reason) => format!("quarantine({})", quarantine_code(*reason)),
        Decision::Keep => "keep".to_owned(),
        Decision::Report => "report".to_owned(),
        Decision::Ignore => "ignore".to_owned(),
        Decision::SafeMode { cause, proposal } => {
            format!("safe({cause:?}, {})", proposal_code(*proposal))
        }
    }
}

fn record_of(journal: &JournalView) -> Option<&AttemptRecord> {
    match &journal.subject {
        Subject::Attempt(record) => Some(record),
        Subject::Unrecorded(_) => None,
    }
}

fn present(provider: &ProviderView) -> Option<&ObservedVm> {
    match provider {
        ProviderView::Present(vm) => Some(vm),
        ProviderView::Incomplete | ProviderView::Absent => None,
    }
}

fn trusted(subject: Subject) -> JournalView {
    JournalView {
        trust: Trust::Trusted,
        subject,
    }
}

fn record(phase: Phase, recorded: Option<&str>, pending: Option<DirectiveTag>) -> Subject {
    Subject::Attempt(AttemptRecord {
        phase,
        instance_id: recorded.map(id),
        pending,
    })
}

fn vm(instance: &str, run: VmRun) -> ProviderView {
    ProviderView::Present(ObservedVm {
        instance_id: id(instance),
        run,
    })
}

const RUNNING: VmRun = VmRun::Running { shim_alive: true };
const STOPPED: VmRun = VmRun::Stopped { exit_json: true };

#[test]
fn should_define_every_row_when_each_view_meets_each_other() {
    // given
    let github: Vec<String> = GITHUB.iter().map(|g| format!("{g:?}")).collect();
    let mut text = format!(
        "# recovery: journal record x provider -> decision per GitHub view ({}), or one \
         decision for all seven | safe-mode proposal\n",
        github.join(" ")
    );
    // when
    for (subject_name, subject) in subjects() {
        for (provider_name, provider) in providers() {
            let decisions: Vec<String> = GITHUB
                .iter()
                .map(|g| code(&classify(&trusted(subject.clone()), &provider, *g)))
                .collect();
            let safe = classify(
                &JournalView {
                    trust: Trust::SafeMode(SafeModeCause::EpochBelowFloor),
                    subject: subject.clone(),
                },
                &provider,
                GithubView::Unreachable,
            );
            let proposal = match safe {
                Decision::SafeMode { proposal, .. } => proposal_code(proposal),
                other => panic!("an untrusted journal decided {}", code(&other)),
            };
            let row = if decisions.iter().all(|d| *d == decisions[0]) {
                format!("all {}", decisions[0])
            } else {
                decisions.join(" ")
            };
            let _ = writeln!(
                text,
                "{subject_name} x {provider_name} -> {row} | safe {proposal}"
            );
        }
    }
    // then
    assert!(
        GOLDEN == text,
        "the table differs from tests/fixtures/matrix/recovery.txt; if these rows are intended, commit it as:\n{text}"
    );
}

#[test]
fn should_decide_each_plan_row_when_its_inputs_are_given() {
    // given: plan §10's rows and dm §11.3's additions, in order
    let rows: Vec<(&str, Subject, ProviderView, GithubView, Decision)> = vec![
        (
            "Cloning | owned clone exists | -: adopt, continue",
            record(Phase::Cloning, None, Some(DirectiveTag::CloneVm)),
            vm("b", STOPPED),
            GithubView::NotRegistered,
            Decision::Adopt(Adoption::Clone(id("b"))),
        ),
        (
            "Cloning | absent (complete) | -: retry same name",
            record(Phase::Cloning, None, Some(DirectiveTag::CloneVm)),
            ProviderView::Absent,
            GithubView::NotRegistered,
            Decision::RetryClone,
        ),
        (
            "any | inventory incomplete | any: wait",
            record(Phase::Busy, Some("a"), None),
            ProviderView::Incomplete,
            GithubView::Busy,
            Decision::Wait,
        ),
        (
            "any | instance id differs | any: quarantine provider_identity_mismatch",
            record(Phase::Retiring, Some("a"), None),
            vm("b", RUNNING),
            GithubView::Offline,
            Decision::Quarantine(QuarantineReason::ProviderIdentityMismatch),
        ),
        (
            "Prepared | running, shim alive | no registration: adopt, re-verify",
            record(Phase::Prepared, Some("a"), None),
            vm("a", RUNNING),
            GithubView::NotRegistered,
            Decision::Adopt(Adoption::Reverify),
        ),
        (
            "Listening | running, shim alive | online: adopt and monitor",
            record(Phase::Listening, Some("a"), None),
            vm("a", RUNNING),
            GithubView::Online,
            Decision::Adopt(Adoption::Monitor),
        ),
        (
            "Busy | running, shim alive | busy: adopt and monitor",
            record(Phase::Busy, Some("a"), None),
            vm("a", RUNNING),
            GithubView::Busy,
            Decision::Adopt(Adoption::Monitor),
        ),
        (
            "Listening | running | unreachable: adopt and monitor, unreachable is not absent",
            record(Phase::Listening, Some("a"), None),
            vm("a", VmRun::Running { shim_alive: false }),
            GithubView::Unreachable,
            Decision::Adopt(Adoption::Monitor),
        ),
        (
            "Busy | running | unreachable: adopt and monitor",
            record(Phase::Busy, Some("a"), None),
            vm("a", VmRun::Running { shim_alive: false }),
            GithubView::Unreachable,
            Decision::Adopt(Adoption::Monitor),
        ),
        (
            "Listening | running | registration absent: retire",
            record(Phase::Listening, Some("a"), None),
            vm("a", RUNNING),
            GithubView::Absent,
            Decision::Retire(RetireReason::JitPresumedLeaked),
        ),
        (
            "Busy | stopped (exit.json) | offline: retire, delete with proof, reap",
            record(Phase::Busy, Some("a"), None),
            vm("a", STOPPED),
            GithubView::Offline,
            Decision::Retire(RetireReason::GuestDied),
        ),
        (
            "Busy | stopped (host rebooted) | absent: retire",
            record(Phase::Busy, Some("a"), None),
            vm("a", VmRun::Stopped { exit_json: false }),
            GithubView::Absent,
            Decision::Retire(RetireReason::GuestDied),
        ),
        (
            "Activating | any | issued, not delivered: presume leaked",
            record(Phase::Activating, Some("a"), None),
            vm("a", RUNNING),
            GithubView::IssuedNotDelivered,
            Decision::PresumeLeaked,
        ),
        (
            "Deleted | owned clone present | any: delete with proof",
            record(Phase::Deleted, Some("a"), None),
            vm("a", RUNNING),
            GithubView::Absent,
            Decision::DeleteLeaked,
        ),
        (
            "Deleting | absent | -: confirm deletion, release slot",
            record(Phase::Deleting, Some("a"), Some(DirectiveTag::DeleteVm)),
            ProviderView::Absent,
            GithubView::Absent,
            Decision::ConfirmDeleted,
        ),
        (
            "Deleting | present | active: suspend cleanup",
            record(Phase::Deleting, Some("a"), Some(DirectiveTag::DeleteVm)),
            vm("a", RUNNING),
            GithubView::Busy,
            Decision::SuspendCleanup,
        ),
        (
            "Quarantined | any | any: keep",
            record(
                Phase::Quarantined {
                    reason: QuarantineReason::OwnershipDoubt,
                    vm: VmCondition::Running,
                },
                None,
                None,
            ),
            vm("a", RUNNING),
            GithubView::Online,
            Decision::Keep,
        ),
        (
            "no record | mf-* name | any: report",
            Subject::Unrecorded(NameClass::ManagedLooking),
            vm("a", RUNNING),
            GithubView::Online,
            Decision::Report,
        ),
    ];
    for (row, subject, provider, github, expected) in rows {
        // when
        let decision = classify(&trusted(subject), &provider, github);
        // then
        assert_eq!(decision, expected, "{row}");
    }
}

#[test]
fn should_enter_safe_mode_when_the_journal_is_below_the_floor_or_does_not_verify() {
    // given
    let subject = record(Phase::Busy, Some("a"), None);
    for cause in CAUSES {
        let journal = JournalView {
            trust: Trust::SafeMode(cause),
            subject: subject.clone(),
        };
        // when
        let decision = classify(&journal, &vm("a", RUNNING), GithubView::Busy);
        // then
        assert_eq!(
            decision,
            Decision::SafeMode {
                cause,
                proposal: Proposal::Adopt
            }
        );
    }
}

#[test]
fn should_release_nothing_when_the_inventory_is_incomplete() {
    // given: every input combination of the table
    for (journal, provider, github) in all_rows() {
        if provider != ProviderView::Incomplete {
            continue;
        }
        // when
        let decision = classify(&journal, &provider, github);
        // then
        assert!(
            matches!(
                decision,
                Decision::Wait
                    | Decision::SafeMode {
                        proposal: Proposal::WaitForInventory,
                        ..
                    }
            ),
            "{journal:?} {github:?} decided {}",
            code(&decision)
        );
        assert!(!decision.releases() && !decision.is_destructive());
    }
}

#[test]
fn should_never_delete_when_an_mf_vm_is_unknown_to_the_journal() {
    // given: every input combination of the table
    for (journal, provider, github) in all_rows() {
        if journal.subject != Subject::Unrecorded(NameClass::ManagedLooking) {
            continue;
        }
        // when
        let decision = classify(&journal, &provider, github);
        // then
        assert!(
            !decision.deletes() && !decision.is_destructive() && !decision.releases(),
            "{provider:?} decided {}",
            code(&decision)
        );
        if present(&provider).is_some() {
            assert!(
                matches!(
                    decision,
                    Decision::Report
                        | Decision::SafeMode {
                            proposal: Proposal::Report,
                            ..
                        }
                ),
                "{provider:?} decided {}",
                code(&decision)
            );
        }
    }
}

#[test]
fn should_never_touch_when_the_vm_is_a_base_or_foreign() {
    // given: every input combination of the table
    for (journal, provider, github) in all_rows() {
        if !matches!(
            journal.subject,
            Subject::Unrecorded(NameClass::ImageBase | NameClass::Foreign)
        ) || provider == ProviderView::Incomplete
        {
            continue;
        }
        // when
        let decision = classify(&journal, &provider, github);
        // then
        assert!(
            matches!(
                decision,
                Decision::Ignore
                    | Decision::SafeMode {
                        proposal: Proposal::Ignore,
                        ..
                    }
            ),
            "{journal:?} {provider:?} decided {}",
            code(&decision)
        );
    }
}

#[test]
fn should_never_delete_when_the_instance_id_differs() {
    // given: every input combination of the table
    for (journal, provider, github) in all_rows() {
        let (Some(record), Some(observed)) = (record_of(&journal), present(&provider)) else {
            continue;
        };
        let Some(recorded) = &record.instance_id else {
            continue;
        };
        if observed.instance_id == *recorded {
            continue;
        }
        // when
        let decision = classify(&journal, &provider, github);
        // then
        assert!(
            !decision.deletes() && !decision.is_destructive() && !decision.releases(),
            "{journal:?} {provider:?} decided {}",
            code(&decision)
        );
        assert!(
            matches!(
                decision,
                Decision::Quarantine(QuarantineReason::ProviderIdentityMismatch)
                    | Decision::Keep
                    | Decision::Report
                    | Decision::SafeMode {
                        proposal: Proposal::Quarantine(QuarantineReason::ProviderIdentityMismatch)
                            | Proposal::Keep
                            | Proposal::Report,
                        ..
                    }
            ),
            "{journal:?} {provider:?} decided {}",
            code(&decision)
        );
    }
}

#[test]
fn should_keep_when_the_attempt_is_quarantined() {
    // given: every input combination of the table
    for (journal, provider, github) in all_rows() {
        let Some(record) = record_of(&journal) else {
            continue;
        };
        if !matches!(record.phase, Phase::Quarantined { .. })
            || provider == ProviderView::Incomplete
        {
            continue;
        }
        // when
        let decision = classify(&journal, &provider, github);
        // then
        assert!(
            matches!(
                decision,
                Decision::Keep
                    | Decision::SafeMode {
                        proposal: Proposal::Keep,
                        ..
                    }
            ),
            "{journal:?} {provider:?} decided {}",
            code(&decision)
        );
    }
}

/// The decisions that read the VM's presence in the listing, and those that read its absence.
fn reads_presence(decision: &Decision) -> bool {
    matches!(
        decision,
        Decision::Adopt(Adoption::Clone(_) | Adoption::Reverify | Adoption::Monitor)
            | Decision::DeleteLeaked
            | Decision::SuspendCleanup
            | Decision::Retire(_)
            | Decision::Quarantine(_)
            | Decision::Report
            | Decision::SafeMode {
                proposal: Proposal::Adopt | Proposal::Quarantine(_) | Proposal::Report,
                ..
            }
    )
}

fn reads_absence(decision: &Decision) -> bool {
    matches!(
        decision,
        Decision::RetryClone
            | Decision::ConfirmDeleted
            | Decision::MarkLost
            | Decision::SafeMode {
                proposal: Proposal::MarkLost,
                ..
            }
    )
}

#[test]
fn should_assert_no_outcome_when_any_row_is_decided() {
    // given: every input combination of the table
    for (journal, provider, github) in all_rows() {
        // when
        let decision = classify(&journal, &provider, github);
        // then
        let label = format!("{journal:?} {provider:?} {github:?} -> {}", code(&decision));
        if reads_presence(&decision) {
            assert!(present(&provider).is_some(), "{label}");
        }
        if reads_absence(&decision) {
            assert_eq!(provider, ProviderView::Absent, "{label}");
        }
        if let Decision::Adopt(Adoption::Clone(instance)) = &decision {
            assert_eq!(
                Some(instance),
                present(&provider).map(|vm| &vm.instance_id),
                "{label}"
            );
        }
    }
}

#[test]
fn should_delete_only_a_leaked_vm_with_proof_when_any_row_is_decided() {
    // given: every input combination of the table
    for (journal, provider, github) in all_rows() {
        // when
        let decision = classify(&journal, &provider, github);
        // then
        if !decision.deletes() {
            continue;
        }
        let record = record_of(&journal).unwrap_or_else(|| panic!("deleted an unrecorded VM"));
        assert!(matches!(record.phase, Phase::Deleted | Phase::Lost));
        assert_eq!(journal.trust, Trust::Trusted);
        assert_eq!(
            record.instance_id.as_ref(),
            present(&provider).map(|vm| &vm.instance_id),
            "a delete carries the recorded instance id of the VM listed"
        );
    }
}

#[test]
fn should_never_retire_when_a_busy_vm_runs() {
    // given: every input combination of the table
    for (journal, provider, github) in all_rows() {
        let Some(record) = record_of(&journal) else {
            continue;
        };
        let Some(observed) = present(&provider) else {
            continue;
        };
        if record.phase != Phase::Busy {
            continue;
        }
        // when
        let decision = classify(&journal, &provider, github);
        // then
        let label = format!("{journal:?} {provider:?} {github:?} -> {}", code(&decision));
        if matches!(observed.run, VmRun::Running { .. }) || github == GithubView::Unreachable {
            assert!(!decision.is_destructive(), "{label}");
        }
    }
}

#[test]
fn should_propose_nothing_destructive_when_the_journal_is_untrusted() {
    // given: every input combination of the table
    for (journal, provider, github) in all_rows() {
        let Trust::SafeMode(cause) = journal.trust else {
            continue;
        };
        // when
        let decision = classify(&journal, &provider, github);
        // then
        let label = format!("{journal:?} {provider:?} {github:?} -> {}", code(&decision));
        assert!(
            matches!(decision, Decision::SafeMode { cause: c, .. } if c == cause),
            "{label}"
        );
        assert!(!decision.is_destructive() && !decision.deletes(), "{label}");
        let twin_decision = classify(
            &JournalView {
                trust: Trust::SafeMode(SafeModeCause::EpochBelowFloor),
                subject: journal.subject.clone(),
            },
            &provider,
            GithubView::NotRegistered,
        );
        let (Decision::SafeMode { proposal, .. }, Decision::SafeMode { proposal: twin, .. }) =
            (&decision, &twin_decision)
        else {
            panic!("{label}");
        };
        assert_eq!(
            proposal, twin,
            "the proposal depends on neither the cause nor GitHub: {label}"
        );
    }
}
