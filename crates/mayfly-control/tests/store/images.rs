//! The image catalog's use cases (OPS-02) over the real `AggregateStore` on a scripted
//! in-memory journal: an image registered and verified, a pool pinned to it naming its
//! `ImageVerified` event, a pin refused unless the image is verified, and a withdrawal that
//! commits between the pin's read and its save refusing the pin at commit.

use mayfly_control::images::{
    CatalogError, PinError, Pinned, pin_image, record_verification, register_image,
};
use mayfly_control::journal::Journal as _;
use mayfly_control::placement::image_instance;
use mayfly_control::store::{AggregateStore, NoNotifier, Outcome, SaveRequest, stream_of};
use mayfly_control_domain::attempt::GuestOs;
use mayfly_control_domain::image::{
    Image, ImageCommand, ImageManifest, ImageVerified, RunnerVersion, Verification,
};
use mayfly_control_domain::pool::{ConfigurePool, Pool, PoolCommand, PoolEvent, TrustTier};
use mayfly_control_domain::{Actor, Command, ControlAggregates, DomainEvent};
use mayfly_domain::{CommandId, Epoch, EventId, ImageDigest, Millis, PoolName, Resources, Sample};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::journal::MemoryJournal;

use crate::recovery_kit::{Kit, NOW, kit, stream};
use crate::scripted::BeforeAppend;

const EPOCH: Epoch = Epoch::FIRST;

fn pool() -> PoolName {
    PoolName::new("lin").unwrap_or_else(|e| panic!("{e}"))
}

fn other() -> ImageDigest {
    ImageDigest::new(&format!("sha256:{}", "b".repeat(64))).unwrap_or_else(|e| panic!("{e}"))
}

fn manifest() -> ImageManifest {
    ImageManifest {
        os: GuestOs::Linux,
        runner_version: RunnerVersion::new(2, 330, 0),
        revision: 1,
    }
}

/// `body` saved on `A`'s `instance` by a second writer on `memory`, accepted.
fn saved_by_another<A: mayfly_control_domain::Aggregate>(
    memory: &MemoryJournal,
    id: u128,
    instance: &str,
    actor: Actor,
    body: A::Body,
) where
    A::Body: Clone,
{
    let store = AggregateStore::new::<ControlAggregates>(
        memory.clone(),
        FakeClock::at(NOW),
        SeededRng::new(9),
        NoNotifier,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let command = Command::new(CommandId::from_u128(id), NOW, EPOCH, actor, body);
    match block_on(store.execute::<A>(SaveRequest::new(instance, command)))
        .unwrap_or_else(|e| panic!("{e}"))
    {
        Outcome::Accepted { .. } => {}
        Outcome::Refused { rejection, .. } => panic!(
            "refused: {}",
            mayfly_control_domain::Rejection::code(&rejection)
        ),
    }
}

fn configured(kit: &Kit) {
    saved_by_another::<Pool>(
        &kit.memory,
        0xC0F1,
        pool().as_str(),
        Actor::Startup,
        PoolCommand::ConfigurePool(ConfigurePool {
            revision: 1,
            tier: TrustTier::Trusted,
            os: GuestOs::Linux,
            vm_resources: Resources::new(4 << 30, 0),
            max_running: 2,
            min_prepared: Some(0),
            breaker_threshold: 3,
            breaker_window: Millis::new(30 * 60_000),
        }),
    );
}

fn registered(kit: &Kit, digest: &ImageDigest) {
    block_on(register_image(
        &kit.store,
        digest,
        manifest(),
        Actor::Startup,
        EPOCH,
    ))
    .unwrap_or_else(|e| panic!("{e}"));
}

fn verified(kit: &Kit, digest: &ImageDigest) {
    registered(kit, digest);
    block_on(record_verification(
        &kit.store,
        digest,
        Verification::Passed,
        Actor::Startup,
        EPOCH,
    ))
    .unwrap_or_else(|e| panic!("{e}"));
}

/// The event id of `digest`'s `ImageVerified`, read from the journal.
fn verified_event(kit: &Kit, digest: &ImageDigest) -> EventId {
    let stream = stream_of::<Image>(image_instance(digest)).unwrap_or_else(|e| panic!("{e}"));
    block_on(kit.memory.read_stream(&stream, None, None))
        .unwrap_or_else(|e| panic!("{e}"))
        .iter()
        .find(|e| e.kind.event_type == ImageVerified::EVENT_TYPE)
        .map(|e| e.event_id)
        .unwrap_or_else(|| panic!("no image_verified"))
}

/// Every `ImagePinned` of the pool, as (digest, previous, verified_ref).
fn pins(kit: &Kit) -> Vec<(ImageDigest, Option<ImageDigest>, EventId)> {
    let stream = stream_of::<Pool>(pool().as_str()).unwrap_or_else(|e| panic!("{e}"));
    block_on(kit.memory.read_stream(&stream, None, None))
        .unwrap_or_else(|e| panic!("{e}"))
        .iter()
        .filter_map(|e| {
            match mayfly_control::store::decode_event::<PoolEvent>(e)
                .unwrap_or_else(|e| panic!("{e:?}"))
            {
                PoolEvent::ImagePinned(p) => Some((p.digest, p.previous, p.verified_ref)),
                _ => None,
            }
        })
        .collect()
}

fn pin(kit: &Kit, digest: &ImageDigest) -> Result<Pinned, PinError> {
    block_on(pin_image(
        &kit.store,
        &pool(),
        digest,
        Actor::Startup,
        EPOCH,
    ))
}

#[test]
fn should_pin_the_pool_naming_the_verified_event_when_the_image_is_verified() {
    // given
    let kit = kit();
    configured(&kit);
    verified(&kit, &ImageDigest::sample());

    // when
    let pinned = pin(&kit, &ImageDigest::sample());

    // then
    let expected = verified_event(&kit, &ImageDigest::sample());
    assert_eq!(
        pinned,
        Ok(Pinned::Moved {
            verified_ref: expected
        })
    );
    assert_eq!(pins(&kit), [(ImageDigest::sample(), None, expected)]);
}

#[test]
fn should_refuse_the_pin_when_the_image_is_not_verified() {
    // given
    let kit = kit();
    configured(&kit);
    registered(&kit, &ImageDigest::sample());

    // when
    let unverified = pin(&kit, &ImageDigest::sample());
    let unregistered = pin(&kit, &other());

    // then
    assert_eq!(unverified, Err(PinError::NotVerified));
    assert_eq!(unregistered, Err(PinError::NotRegistered));
    assert!(pins(&kit).is_empty());
}

#[test]
fn should_refuse_the_pin_when_the_image_is_withdrawn() {
    // given
    let kit = kit();
    configured(&kit);
    verified(&kit, &ImageDigest::sample());
    saved_by_another::<Image>(
        &kit.memory,
        0xD1,
        image_instance(&ImageDigest::sample()),
        Actor::Operator { uid: 501 },
        ImageCommand::WithdrawImage,
    );

    // when
    let pinned = pin(&kit, &ImageDigest::sample());

    // then
    assert_eq!(pinned, Err(PinError::Withdrawn));
    assert!(pins(&kit).is_empty());
}

#[test]
fn should_refuse_the_pin_at_commit_when_the_image_is_withdrawn_after_the_read() {
    // given: the image is verified when the pin reads it, and withdrawn right before the pin's
    // unit commits
    let kit = kit();
    configured(&kit);
    verified(&kit, &ImageDigest::sample());
    let memory = kit.memory.clone();
    kit.journal.before_next_append(BeforeAppend::new(
        stream(&format!("image-{}", image_instance(&ImageDigest::sample()))),
        move || {
            saved_by_another::<Image>(
                &memory,
                0xD2,
                image_instance(&ImageDigest::sample()),
                Actor::Operator { uid: 501 },
                ImageCommand::WithdrawImage,
            );
        },
    ));

    // when
    let pinned = pin(&kit, &ImageDigest::sample());

    // then: the unit read the moved image stream and was refused; the retry found it withdrawn
    let state = block_on(kit.store.load::<Pool>(pool().as_str())).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(pinned, Err(PinError::Withdrawn));
    assert_eq!(state.image_pin, None);
}

#[test]
fn should_record_each_move_when_a_pool_is_pinned_back_to_an_earlier_image() {
    // given
    let kit = kit();
    configured(&kit);
    verified(&kit, &ImageDigest::sample());
    verified(&kit, &other());
    let (a, b) = (
        verified_event(&kit, &ImageDigest::sample()),
        verified_event(&kit, &other()),
    );

    // when
    let moves = [
        pin(&kit, &ImageDigest::sample()),
        pin(&kit, &other()),
        pin(&kit, &ImageDigest::sample()),
        pin(&kit, &ImageDigest::sample()),
    ];

    // then: the return to the first image is a new command, not a replay of the first pin
    assert_eq!(
        moves,
        [
            Ok(Pinned::Moved { verified_ref: a }),
            Ok(Pinned::Moved { verified_ref: b }),
            Ok(Pinned::Moved { verified_ref: a }),
            Ok(Pinned::Already),
        ]
    );
    assert_eq!(
        pins(&kit),
        [
            (ImageDigest::sample(), None, a),
            (other(), Some(ImageDigest::sample()), b),
            (ImageDigest::sample(), Some(other()), a),
        ]
    );
}

#[test]
fn should_refuse_another_manifest_when_a_registered_digest_is_registered_again() {
    // given
    let kit = kit();
    registered(&kit, &ImageDigest::sample());
    let changed = ImageManifest {
        revision: 2,
        ..manifest()
    };

    // when
    let again = block_on(register_image(
        &kit.store,
        &ImageDigest::sample(),
        manifest(),
        Actor::Startup,
        EPOCH,
    ));
    let other_manifest = block_on(register_image(
        &kit.store,
        &ImageDigest::sample(),
        changed,
        Actor::Startup,
        EPOCH,
    ));

    // then
    assert_eq!(again, Ok(()));
    assert_eq!(
        other_manifest,
        Err(CatalogError::Refused("manifest_immutable"))
    );
}
