//! The host the component tests run on: the data-plane kit's fakes behind the executor.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use mayfly_data::{Config, Diagnostics, Executor, GuestAnswer, ImageListing, Ports, SessionEnd};
use mayfly_data_domain::{HostProbe, Sizing};
use mayfly_domain::{
    AttemptId, Deadline, DirectiveId, Epoch, Generation, HostId, ImageDigest, Millis, OwnedVmName,
    PoolName, ProviderInstanceId, Resources, Sample, Slot,
};
use mayfly_protocol::{
    BaseName, CURRENT, Capacity, Directive, DirectiveKind, GuestFacts, GuestOs, HostFacts, Mode,
    Observation, ObservationKind, OwnershipProof, RunOptions, Welcome,
};
use mayfly_testkit_data::{
    FakeClock, FakeGuest, FakeHostProbe, FakeImageStore, FakeVm, FakeVmProvider, GuestScript,
    MemoryFloor, SequentialIds, StaticWelcome, Tap, directives, observations,
};

pub const GIB: u64 = 1 << 30;
/// The session every test enters.
pub const EPOCH: Epoch = Epoch::new(5);
/// The clock's reading when a test starts.
pub const START: Millis = Millis::new(1_000);
/// Every directive's deadline unless a test says otherwise.
pub const DEADLINE: Millis = Millis::new(60_000);
/// The observer's period.
pub const OBSERVE: Millis = Millis::new(10_000);
/// How long one observer pass may take.
pub const LISTING: Millis = Millis::new(5_000);

pub type Exec =
    Executor<FakeVmProvider, FakeImageStore, FakeGuest, FakeHostProbe, FakeClock, SequentialIds>;

pub fn host() -> HostId {
    HostId::sample()
}

pub fn attempt(n: u128) -> AttemptId {
    AttemptId::from_u128(n)
}

/// The VM of attempt `n` on this host, in slot `n`, generation 1.
pub fn vm(n: u128) -> OwnedVmName {
    let slot = u32::try_from(n).expect("a small slot number");
    OwnedVmName::new(
        PoolName::sample(),
        host(),
        Slot::new(slot),
        Generation::FIRST,
        attempt(n),
    )
}

pub fn instance(text: &str) -> ProviderInstanceId {
    ProviderInstanceId::new(text).expect("a valid instance id")
}

/// The proof of [`vm`]`(n)` with `instance_id`.
pub fn proof(n: u128, instance_id: &ProviderInstanceId) -> OwnershipProof {
    let name = vm(n);
    OwnershipProof {
        attempt_id: attempt(n),
        host_id: host(),
        slot: name.slot(),
        generation: Generation::FIRST,
        vm_name: name,
        instance_id: instance_id.clone(),
    }
}

pub fn base() -> BaseName {
    BaseName::new("mfb-base").expect("a valid base name")
}

pub fn digest() -> ImageDigest {
    ImageDigest::sample()
}

pub fn run_options() -> RunOptions {
    RunOptions {
        resources: Resources::new(8 * GIB, 80 * GIB),
    }
}

pub fn clone(n: u128) -> DirectiveKind {
    DirectiveKind::CloneVm {
        base_name: base(),
        vm_name: vm(n),
    }
}

/// Directive `id` of the session, without an attempt (the proof is checked against its own
/// name), due at [`DEADLINE`].
pub fn directive(id: u128, kind: DirectiveKind) -> Directive {
    due(id, kind, DEADLINE)
}

/// Directive `id` due at `deadline`.
pub fn due(id: u128, kind: DirectiveKind, deadline: Millis) -> Directive {
    Directive {
        directive_id: DirectiveId::from_u128(id),
        epoch: EPOCH,
        attempt: None,
        deadline: Deadline::at(deadline),
        kind,
    }
}

/// Whether an observation answers directive `id`.
pub fn answers(id: u128) -> impl Fn(&Observation) -> bool {
    move |o| {
        o.answers
            .is_some_and(|a| a.directive_id == DirectiveId::from_u128(id))
    }
}

pub fn capacity(vm_slots: u32, macos_guests: u32, memory_gib: u64, disk_gib: u64) -> Capacity {
    Capacity {
        vm_slots,
        macos_guests,
        resources: Resources::new(memory_gib * GIB, disk_gib * GIB),
    }
}

/// A host with `vm_slots` slots, two macOS guests, 64 GiB and 1000 GiB, nothing foreign.
pub fn probe(vm_slots: u32) -> HostProbe {
    HostProbe {
        totals: capacity(vm_slots, 2, 64, 1000),
        reserve: Capacity::default(),
        foreign: Capacity::default(),
        foreign_unsized: BTreeSet::new(),
        disk_low_water: false,
        memory_pressure: false,
        hypervisor_limit: false,
    }
}

pub fn facts() -> HostFacts {
    HostFacts {
        disk_free_bytes: 500 * GIB,
        disk_low_water: false,
        woke: false,
        tart_version: "2.0.0".to_owned(),
        external_guests: 0,
    }
}

/// A Linux VM the listing describes fully, with `instance_id`.
pub fn described(instance_id: &str, running: bool) -> FakeVm {
    FakeVm {
        instance_id: Some(instance(instance_id)),
        running,
        guest: Some(GuestOs::Linux),
        memory_bytes: running.then_some(8 * GIB),
        disk_bytes: Some(80 * GIB),
    }
}

pub fn sizing() -> Sizing {
    Sizing {
        image_disk_bytes: 50 * GIB,
        base_disk_bytes: 60 * GIB,
        clone_disk_bytes: 80 * GIB,
        vm_memory_bytes: 8 * GIB,
    }
}

pub fn config(concurrency: usize) -> Config {
    Config {
        host_id: host(),
        sizing: sizing(),
        concurrency,
        observe_every: OBSERVE,
        listing_timeout: LISTING,
        boot_id: "boot-1".to_owned(),
        dp_version: "0.1.0".to_owned(),
    }
}

/// A welcome of a control plane older than protocol 1.2: no clock reading, so deadlines are
/// read by the wall clock at receipt.
pub fn welcome(epoch: Epoch) -> Welcome {
    Welcome {
        epoch,
        mode: Mode::Normal,
        accepted_version: CURRENT,
        cp_clock: None,
    }
}

/// A welcome whose control plane's clock read `cp_clock` when it wrote it.
pub fn welcome_at(epoch: Epoch, cp_clock: Millis) -> Welcome {
    Welcome {
        cp_clock: Some(cp_clock),
        ..welcome(epoch)
    }
}

/// Every fake of one host; clones of these handles are what the executor owns.
pub struct Fakes {
    pub provider: FakeVmProvider,
    pub images: FakeImageStore,
    pub guest: FakeGuest,
    pub host: FakeHostProbe,
    pub clock: FakeClock,
    pub floor: MemoryFloor,
}

impl Fakes {
    /// A host probed as `reading`, with an empty provider whose clones are described Linux
    /// VMs, and an image store holding the image and the Linux base.
    pub fn new(reading: HostProbe) -> Self {
        Self {
            provider: FakeVmProvider::new(Some(GuestOs::Linux), Some(80 * GIB)),
            images: FakeImageStore::new(ImageListing {
                images: BTreeSet::from([digest()]),
                bases: BTreeMap::from([(base(), GuestOs::Linux)]),
            }),
            guest: FakeGuest::new(GuestScript {
                verify: Ok(GuestAnswer::Verified(GuestFacts {
                    os: GuestOs::Linux,
                    uptime: Millis::new(5),
                })),
                launch: Ok(()),
                export: Ok(Diagnostics::Exported {
                    bytes: 10,
                    sha256: [7; 32],
                }),
            }),
            host: FakeHostProbe::new(reading, facts()),
            clock: FakeClock::new(START),
            floor: MemoryFloor::new(Epoch::new(0)),
        }
    }

    pub fn ports(
        &self,
    ) -> Ports<FakeVmProvider, FakeImageStore, FakeGuest, FakeHostProbe, FakeClock, SequentialIds>
    {
        Ports {
            provider: self.provider.clone(),
            images: self.images.clone(),
            guest: self.guest.clone(),
            host: self.host.clone(),
            clock: self.clock.clone(),
            ids: SequentialIds::default(),
        }
    }

    /// An executor started over these fakes with `concurrency`, not yet in a session.
    pub async fn start(&self, concurrency: usize) -> Exec {
        Exec::start(config(concurrency), self.ports(), self.floor.floor())
            .await
            .expect("the host can be probed")
    }

    /// An executor started and entered into the session at [`EPOCH`].
    pub async fn open(&self, concurrency: usize) -> Exec {
        let mut exec = self.start(concurrency).await;
        let mut link = StaticWelcome::new(Ok(welcome(EPOCH)));
        let entered = exec.enter(&mut link, &self.floor).await;
        assert_eq!(entered, Ok(EPOCH));
        exec
    }
}

/// Runs `test` on a current-thread runtime: the executor and the test's script are polled
/// together on one task, so nothing is spawned.
pub fn run<F: Future>(test: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime")
        .block_on(test)
}

/// Serves one session of `exec` while `script` feeds it directives and reads its observations;
/// the session ends when the script returns (its directive feed is dropped). Returns how the
/// session ended and every observation sent.
pub async fn session<S, F>(exec: &mut Exec, script: S) -> (SessionEnd, Vec<Observation>)
where
    S: FnOnce(mayfly_testkit_data::DirectiveFeed, Tap) -> F,
    F: Future<Output = Tap>,
{
    let (feed, mut source) = directives(16);
    let (sink, tap) = observations(1024);
    let (end, mut tap) = tokio::join!(exec.serve(&mut source, &sink), script(feed, tap));
    (end, tap.drain())
}

/// The kinds of the observations that answer `id`, in order.
pub fn answers_to(seen: &[Observation], id: u128) -> Vec<ObservationKind> {
    seen.iter()
        .filter(|o| answers(id)(o))
        .map(|o| o.kind.clone())
        .collect()
}
