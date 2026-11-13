//! Each directive kind against its port: what an applied, rejected or failed effect answers.

use std::collections::{BTreeMap, BTreeSet};

use mayfly_data::{
    Ambiguity, Diagnostics, GuestAnswer, ImageListing, LaunchAnswer, MeasuredDisks, RunnerStatus,
    VmError,
};
use mayfly_domain::{ImageDigest, Millis, ProviderInstanceId, SecretHandleId};
use mayfly_protocol::{
    BaseName, DiagnosticsLoss, DirectiveKind, GuestFacts, GuestOs, GuestRejection,
    ImageStoreListing, ListedBase, Nonce, ObservationKind, RefusalReason, ResourceDimension,
    SecretHandle,
};
use mayfly_testkit_data::{FakeImageStore, GuestScript};

use crate::support::{
    Fakes, GIB, answers, answers_to, base, config, described, digest, directive, facts, probe, run,
    run_options, session, vm,
};

fn instance() -> ProviderInstanceId {
    ProviderInstanceId::new("0a:00:00:00:01:01").expect("an instance id")
}

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
            instance_id: Some(instance()),
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
            status: Ok(RunnerStatus::NotLaunched),
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
                    disk_bytes: None,
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
            status: Ok(RunnerStatus::NotLaunched),
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

#[test]
fn should_refuse_proof_mismatch_when_the_launch_identity_check_fails_or_has_no_instance_id() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        fakes.guest.script(GuestScript {
            verify: Ok(GuestAnswer::Rejected(GuestRejection::ChallengeFailed)),
            launch: Ok(LaunchAnswer::IdentityRefused),
            status: Ok(RunnerStatus::NotLaunched),
            export: Ok(Diagnostics::Lost(DiagnosticsLoss::GuestUnreachable)),
        });
        let mut exec = fakes.open(2).await;
        let launch = |instance_id| DirectiveKind::LaunchRunner {
            vm_name: vm(1),
            launch: SecretHandle {
                handle_id: SecretHandleId::from_u128(1),
            },
            instance_id,
        };

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, launch(Some(instance())))).await;
            tap.wait_for(answers(1)).await;
            feed.send(directive(2, launch(None))).await;
            tap.wait_for(answers(2)).await;
            tap
        })
        .await;

        // then
        let mismatch = vec![ObservationKind::DirectiveRefused {
            reason: RefusalReason::ProofMismatch,
        }];
        assert_eq!(answers_to(&seen, 1), mismatch, "the transport refused");
        assert_eq!(answers_to(&seen, 2), mismatch, "no instance id to check");
        assert_eq!(
            fakes.guest.launches(),
            1,
            "a launch without an id never reaches it"
        );
    });
}

#[test]
fn should_measure_the_manifest_before_admitting_a_pull_when_no_ceiling_is_configured() {
    run(async {
        // given: no disk ceiling configured; the registry's manifest of `fresh` states 30 GiB,
        // and `unmeasurable` has none
        let mut fakes = Fakes::new(probe(4));
        fakes.images = FakeImageStore::new(ImageListing {
            images: BTreeSet::from([digest()]),
            bases: BTreeMap::from([(base(), GuestOs::Linux)]),
            disks: MeasuredDisks {
                images: BTreeMap::from([(digest(), 20 * GIB)]),
                bases: BTreeMap::from([(base(), 20 * GIB)]),
                base_images: BTreeMap::from([(base(), digest())]),
            },
        });
        let fresh = ImageDigest::new(&format!("sha256:{}", "c".repeat(64))).expect("a digest");
        let unmeasurable =
            ImageDigest::new(&format!("sha256:{}", "d".repeat(64))).expect("a digest");
        fakes.images.manifest(&fresh, 30 * GIB);
        let mut config = config(2);
        config.sizing.image_disk_bytes = None;
        config.sizing.base_disk_bytes = None;
        config.sizing.clone_disk_bytes = None;
        let mut exec = fakes.open_with(config).await;
        let pulled = fresh.clone();

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, DirectiveKind::PullImage { digest: fresh }))
                .await;
            tap.wait_for(answers(1)).await;
            feed.send(directive(
                2,
                DirectiveKind::PullImage {
                    digest: unmeasurable,
                },
            ))
            .await;
            tap.wait_for(answers(2)).await;
            tap
        })
        .await;

        // then: the measured pull runs; the other is refused unknown and never pulled
        assert_eq!(
            answers_to(&seen, 1),
            [ObservationKind::ImagePulled {
                digest: pulled.clone()
            }]
        );
        assert_eq!(
            answers_to(&seen, 2),
            [ObservationKind::DirectiveRefused {
                reason: RefusalReason::CapacityUnknown {
                    dimension: ResourceDimension::Disk
                }
            }]
        );
        assert_eq!(fakes.images.measured().len(), 2);
        assert_eq!(
            exec.ledger().images.get(&pulled),
            Some(&Some(30 * GIB)),
            "the pulled image is counted at its manifest's disk"
        );
    });
}

fn measured_config() -> mayfly_data::Config {
    let mut config = config(2);
    config.sizing.image_disk_bytes = None;
    config.sizing.base_disk_bytes = None;
    config.sizing.clone_disk_bytes = None;
    config
}

#[test]
fn should_measure_a_listed_image_again_when_its_size_did_not_read() {
    run(async {
        // given: the store lists the image and its base without sizes (a kept manifest that no
        // longer reads), and the registry does not answer for it yet
        let mut fakes = Fakes::new(probe(4));
        fakes.images = FakeImageStore::new(ImageListing {
            images: BTreeSet::from([digest()]),
            bases: BTreeMap::from([(base(), GuestOs::Linux)]),
            disks: MeasuredDisks {
                base_images: BTreeMap::from([(base(), digest())]),
                ..MeasuredDisks::default()
            },
        });
        let mut exec = fakes.open_with(measured_config()).await;
        let unknown_at_start = exec.ledger().unmeasured_listed();

        // when: the registry answers again, and an observer pass runs
        fakes.images.manifest(&digest(), 20 * GIB);
        let _ = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, DirectiveKind::ReportInventory))
                .await;
            tap.wait_for(answers(1)).await;
            tap
        })
        .await;

        // then: unknown, never zero, until measured; then the image and its base are sized
        assert_eq!(unknown_at_start, BTreeSet::from([digest()]));
        assert!(exec.ledger().unmeasured_listed().is_empty());
        assert_eq!(exec.ledger().images.get(&digest()), Some(&Some(20 * GIB)));
        assert_eq!(
            exec.ledger().bases.get(&base()).and_then(|b| b.disk_bytes),
            Some(20 * GIB)
        );
    });
}

#[test]
fn should_fetch_no_manifest_when_a_pull_is_stale() {
    run(async {
        // given: no ceiling configured and a pull of an unmeasured image under an older epoch
        let fakes = Fakes::new(probe(4));
        let fresh = ImageDigest::new(&format!("sha256:{}", "e".repeat(64))).expect("a digest");
        fakes.images.manifest(&fresh, 30 * GIB);
        let mut exec = fakes.open_with(measured_config()).await;
        let asked = fresh.clone();
        let mut stale = directive(1, DirectiveKind::PullImage { digest: fresh });
        stale.epoch = mayfly_domain::Epoch::new(1);

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(stale).await;
            tap.wait_for(answers(1)).await;
            tap
        })
        .await;

        // then: fenced before any registry fetch
        assert_eq!(
            answers_to(&seen, 1),
            [ObservationKind::OutcomeUnknown {
                reason: mayfly_protocol::UnknownReason::StaleEpoch
            }]
        );
        assert!(!fakes.images.measured().contains(&asked));
    });
}

/// The answer to one `MaterialiseImage` of `digest()` into `mfb-two`, the store's listing
/// failing from the session's start when `listing_fails`.
async fn materialised(fakes: &Fakes, listing_fails: bool) -> Vec<ObservationKind> {
    let mut exec = fakes.open(2).await;
    fakes.images.fail_listing(listing_fails);
    let kind = DirectiveKind::MaterialiseImage {
        digest: digest(),
        base_name: BaseName::new("mfb-two").expect("a valid base name"),
        runner_version: "2.330.0".to_owned(),
    };
    let (_, seen) = session(&mut exec, |feed, mut tap| async move {
        feed.send(directive(1, kind)).await;
        tap.wait_for(answers(1)).await;
        tap
    })
    .await;
    answers_to(&seen, 1)
}

#[test]
fn should_answer_the_bases_measured_disk_when_a_materialisation_is_applied() {
    run(async {
        // given: the image's manifest measures 40 GiB, so its base lists at 40 GiB
        let fakes = Fakes::new(probe(4));
        fakes.images.manifest(&digest(), 40 * GIB);

        // when
        let answered = materialised(&fakes, false).await;

        // then: the control plane plans its clones at it (protocol 1.4, CP-07)
        assert_eq!(
            answered,
            vec![ObservationKind::ImageMaterialised {
                digest: digest(),
                base_name: BaseName::new("mfb-two").expect("a valid base name"),
                disk_bytes: Some(40 * GIB),
            }]
        );
    });
}

#[test]
fn should_answer_no_disk_when_the_listing_fails_after_a_materialisation() {
    run(async {
        // given: the image measures, but the store's listing fails once the session is open
        let fakes = Fakes::new(probe(4));
        fakes.images.manifest(&digest(), 40 * GIB);

        // when
        let answered = materialised(&fakes, true).await;

        // then: the base is made, its disk unknown: none, never a default (ADR 0052)
        assert_eq!(
            answered,
            vec![ObservationKind::ImageMaterialised {
                digest: digest(),
                base_name: BaseName::new("mfb-two").expect("a valid base name"),
                disk_bytes: None,
            }]
        );
        assert!(
            fakes
                .images
                .listing()
                .bases
                .contains_key(&BaseName::new("mfb-two").expect("a valid base name"))
        );
    });
}

/// The image-store listing beside the answer to one `ReportInventory`, or `None` when the
/// answer carries none (or is no inventory).
async fn listed_beside_inventory(fakes: &Fakes) -> Option<ImageStoreListing> {
    let mut exec = fakes.open(2).await;
    let (_, seen) = session(&mut exec, |feed, mut tap| async move {
        feed.send(directive(1, DirectiveKind::ReportInventory))
            .await;
        tap.wait_for(answers(1)).await;
        tap
    })
    .await;
    answers_to(&seen, 1)
        .into_iter()
        .find_map(|kind| match kind {
            ObservationKind::Inventory { image_store, .. } => image_store,
            _ => None,
        })
}

#[test]
fn should_send_the_image_listing_beside_the_inventory_when_a_pass_is_clean() {
    run(async {
        // given: the store holds a verified image and a Ready base made from it, the base's own
        // size unread (it takes its image's)
        let mut fakes = Fakes::new(probe(4));
        fakes.images = FakeImageStore::new(ImageListing {
            images: BTreeSet::from([digest()]),
            bases: BTreeMap::from([(base(), GuestOs::Linux)]),
            disks: MeasuredDisks {
                images: BTreeMap::from([(digest(), 20 * GIB)]),
                base_images: BTreeMap::from([(base(), digest())]),
                ..MeasuredDisks::default()
            },
        });

        // when
        let listed = listed_beside_inventory(&fakes).await;

        // then: what the clean pass listed crosses the link beside the inventory (DP-08)
        assert_eq!(
            listed,
            Some(ImageStoreListing {
                images: vec![digest()],
                bases: vec![ListedBase {
                    base_name: base(),
                    digest: digest(),
                    disk_bytes: Some(20 * GIB),
                }],
            })
        );
    });
}

#[test]
fn should_send_no_image_listing_when_the_store_cannot_be_listed() {
    run(async {
        // given: the store holds an image, but its listing fails from the session's start
        let mut fakes = Fakes::new(probe(4));
        fakes.images = FakeImageStore::new(ImageListing {
            images: BTreeSet::from([digest()]),
            ..ImageListing::default()
        });
        fakes.images.fail_listing(true);

        // when
        let listed = listed_beside_inventory(&fakes).await;

        // then: the inventory goes alone: an absent image is never read from a failed listing
        assert_eq!(listed, None);
    });
}
