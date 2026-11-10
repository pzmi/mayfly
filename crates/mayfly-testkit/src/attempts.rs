//! Attempts in a real `AggregateStore`, for the saga's and the directive relay's component
//! tests: the commands that start one and record what the data plane answered, and fixed
//! [`DirectiveDetails`] for the sink.

use mayfly_control::delivery::DirectiveDetails;
use mayfly_control::journal::Journal;
use mayfly_control::saga::SAGA;
use mayfly_control::store::{AggregateStore, Clock, Outcome, Rng, SaveRequest, StoreNotifier};
use mayfly_control_domain::attempt::{
    Attempt, AttemptCommand, AttemptRejection, Fact, Observation, StartAttempt,
};
use mayfly_control_domain::{Actor, Command};
use mayfly_domain::{
    AttemptId, CommandId, DirectiveId, Epoch, Generation, HostId, ImageDigest, Millis,
    ObservationId, PoolName, Resources, Sample, Slot,
};
use mayfly_protocol::{BaseName, Nonce, RunOptions};

/// What every attempt here gives its VM.
pub const GIB: u64 = 1 << 30;

/// The `StartAttempt` of attempt `n` on the sample host, in slot `n`, generation 1, with a
/// retry budget of 2.
///
/// # Panics
/// When `n` does not fit a slot number.
#[must_use]
pub fn start(n: u128) -> AttemptCommand {
    AttemptCommand::StartAttempt(StartAttempt {
        attempt_id: AttemptId::from_u128(n),
        host_id: HostId::sample(),
        slot: Slot::new(u32::try_from(n).unwrap_or_else(|e| panic!("{e}"))),
        generation: Generation::FIRST,
        pool: PoolName::sample(),
        image_digest: ImageDigest::sample(),
        retry_budget: 2,
    })
}

/// The stream instance of attempt `n` (`attempt-<instance>`).
#[must_use]
pub fn instance(n: u128) -> String {
    AttemptId::from_u128(n).to_string()
}

/// Saves `body` for attempt `n` as `actor` under `epoch`, issued by the store's clock, with
/// command id `command_id`.
///
/// # Panics
/// When the store fails: the test's setup is wrong.
pub async fn save<J: Journal, C: Clock, R: Rng, N: StoreNotifier>(
    store: &AggregateStore<J, C, R, N>,
    n: u128,
    command_id: u128,
    epoch: Epoch,
    body: AttemptCommand,
) -> Outcome<AttemptRejection> {
    let command = Command::new(
        CommandId::from_u128(command_id),
        store.clock().now(),
        epoch,
        Actor::Automation { policy: SAGA },
        body,
    );
    store
        .execute::<Attempt>(SaveRequest::new(instance(n), command))
        .await
        .unwrap_or_else(|e| panic!("{e}"))
}

/// The data plane's answer `fact` as the inbox would record it (CP-04 builds the real
/// mapping), observed in `epoch`, generation 1.
#[must_use]
pub fn observe(observation: u128, epoch: Epoch, fact: Fact) -> AttemptCommand {
    AttemptCommand::RecordObservation(Observation {
        observation_id: ObservationId::from_u128(observation),
        epoch,
        generation: Generation::FIRST,
        fact,
    })
}

/// Fixed details: one base for every image while `base` is set, 8 GiB and 80 GiB per VM, a
/// 10 s stop grace, and a nonce derived from the directive id.
#[derive(Debug, Clone)]
pub struct FixedDetails {
    /// The base every clone is made from; `None` for an image not materialised.
    pub base: Option<BaseName>,
}

impl Default for FixedDetails {
    fn default() -> Self {
        Self {
            base: Some(BaseName::new("mfb-base").unwrap_or_else(|e| panic!("{e}"))),
        }
    }
}

impl DirectiveDetails for FixedDetails {
    fn image_base(&self, _pool: &PoolName, _digest: &ImageDigest) -> Option<BaseName> {
        self.base.clone()
    }

    fn run_options(&self, _pool: &PoolName) -> Option<RunOptions> {
        Some(RunOptions {
            resources: Resources::new(8 * GIB, 80 * GIB),
        })
    }

    fn stop_grace(&self, _pool: &PoolName) -> Millis {
        Millis::new(10_000)
    }

    fn nonce(&self, directive_id: DirectiveId) -> Nonce {
        Nonce::from_u128(directive_id.as_u128() ^ 0x5a5a)
    }
}
