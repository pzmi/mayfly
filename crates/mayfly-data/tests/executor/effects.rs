//! Each directive kind against its port: what an applied, rejected or failed effect answers.

use mayfly_data::{Ambiguity, Diagnostics, GuestAnswer, VmError};
use mayfly_domain::{Millis, SecretHandleId};
use mayfly_protocol::{
    BaseName, DiagnosticsLoss, DirectiveKind, GuestFacts, GuestOs, GuestRejection, Nonce,
    ObservationKind, RefusalReason, SecretHandle,
};
use mayfly_testkit_data::GuestScript;

use crate::support::{
    Fakes, answers, answers_to, base, described, digest, directive, facts, probe, run, run_options,
    session, vm,
};

fn every_effect() -> Vec<DirectiveKind> {
    vec![
        DirectiveKind::PullImage { digest: digest() },
        DirectiveKind::MaterialiseImage {
            digest: digest(),
            base_name: BaseName::new("mfb-two").expect("a valid base name"),
            runner_version: "2.330.0".to_owned(),
        },
        DirectiveKind::EvictImage { base_name: base() },
        DirectiveKind::StartVm {
            vm_name: vm(1),
            run_options: run_options(),
        },
        DirectiveKind::VerifyGuest {
            vm_name: vm(1),
            nonce: Nonce::from_u128(1),
        },
        DirectiveKind::LaunchRunner {
            vm_name: vm(1),
            launch: SecretHandle {
                handle_id: SecretHandleId::from_u128(1),
            },
        },
        DirectiveKind::ExportDiagnostics { vm_name: vm(1) },
        DirectiveKind::ReportHostFacts,
    ]
}

/// Sends every kind of [`every_effect`] in turn and returns the answers, one per kind.
async fn answer_each(fakes: &Fakes) -> Vec<Vec<ObservationKind>> {
    let mut exec = fakes.open(2).await;
    let kinds = every_effect();
    let count = kinds.len();
    let (_, seen) = session(&mut exec, |feed, mut tap| async move {
        for (id, kind) in (1..).zip(kinds) {
            feed.send(directive(id, kind)).await;
            tap.wait_for(answers(id)).await;
        }
        tap
    })
    .await;
    (1..).take(count).map(|id| answers_to(&seen, id)).collect()
}

#[test]
fn should_answer_what_each_effect_observed_when_it_is_applied() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        fakes
            .provider
            .seed(&vm(1).to_string(), described("0a:00:00:00:01:01", false));
        fakes.guest.script(GuestScript {
            verify: Ok(GuestAnswer::Verified(GuestFacts {
                os: GuestOs::Linux,
                uptime: Millis::new(5),
            })),
            launch: Err(VmError::Ambiguous(Ambiguity::HostSlept)),
            export: Ok(Diagnostics::Exported {
                bytes: 10,
                sha256: [7; 32],
            }),
        });

        // when
        let answered = answer_each(&fakes).await;

        // then
        assert_eq!(
            answered,
            [
                vec![ObservationKind::ImagePulled { digest: digest() }],
                vec![ObservationKind::ImageMaterialised {
                    digest: digest(),
                    base_name: BaseName::new("mfb-two").expect("a valid base name"),
                }],
                vec![ObservationKind::ImageEvicted { base_name: base() }],
                vec![ObservationKind::VmBooted],
                vec![ObservationKind::GuestVerified {
                    facts: GuestFacts {
                        os: GuestOs::Linux,
                        uptime: Millis::new(5),
                    }
                }],
                vec![ObservationKind::RunnerLaunchUnknown],
                vec![ObservationKind::DiagnosticsExported {
                    bytes: 10,
                    sha256: [7; 32]
                }],
                vec![ObservationKind::HostFacts(facts())],
            ]
        );
        assert!(fakes.provider.vms()[&vm(1).to_string()].running);
        assert!(!fakes.images.listing().bases.contains_key(&base()));
    });
}

#[test]
fn should_refuse_or_report_the_failure_when_an_effect_fails_definitely() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        fakes
            .provider
            .seed(&vm(1).to_string(), described("0a:00:00:00:01:01", false));
        fakes.guest.script(GuestScript {
            verify: Ok(GuestAnswer::Rejected(GuestRejection::ChallengeFailed)),
            launch: Err(VmError::Definite),
            export: Ok(Diagnostics::Lost(DiagnosticsLoss::GuestUnreachable)),
        });
        let mut exec = fakes.open(2).await;
        fakes.images.fail(Some(VmError::Definite));
        fakes.host.fail(true);
        fakes.provider.fail_next(
            mayfly_testkit_data::Op::Start,
            mayfly_testkit_data::Fault::Definite,
        );

        // when
        let kinds = every_effect();
        let count = kinds.len();
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            for (id, kind) in (1..).zip(kinds) {
                feed.send(directive(id, kind)).await;
                tap.wait_for(answers(id)).await;
            }
            tap
        })
        .await;
        let answered: Vec<_> = (1..).take(count).map(|id| answers_to(&seen, id)).collect();

        // then
        let failed = || {
            vec![ObservationKind::DirectiveRefused {
                reason: RefusalReason::EffectFailed,
            }]
        };
        assert_eq!(
            answered,
            [
                failed(),
                failed(),
                failed(),
                failed(),
                vec![ObservationKind::GuestRejected {
                    reason: GuestRejection::ChallengeFailed
                }],
                failed(),
                vec![ObservationKind::DiagnosticsLost {
                    reason: DiagnosticsLoss::GuestUnreachable
                }],
                failed(),
            ]
        );
        assert!(exec.ledger().reservations.is_empty(), "a refusal releases");
    });
}
