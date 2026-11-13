//! Commands (plan §5.4, §11): the operator's use cases behind `mayfly pool
//! pause|resume|drain|reset-breaker` and `mayfly host drain|resume` (RT-03). Each issues one
//! command to one aggregate through [`AggregateStore::execute`], so it is journaled before it
//! acts; the reconciler and the terms policy read the result from the journal as they read any
//! other change (a drained scope is retired and completed by P5/P23).
//!
//! **The actor is the caller's.** Every use case takes the [`Actor`] it records; only the
//! operator socket builds `Actor::Operator`, from the peer credentials of its connection
//! (`ARCH-operator-actor`, KER-10F), so nothing here can promote a policy to an operator.
//!
//! **Idempotent per revision.** A command's id is derived from what it does (with its
//! arguments, a drain's deadline included), its target, the run epoch and the revision of the
//! target's stream read just before: the same request repeated before the stream moves is
//! answered by its receipt, and the same request after a change (a pool paused, resumed and
//! paused again) is a new command. So a retry after a lost answer is a new command once the
//! first one committed: it is decided against the moved stream and may be refused
//! (`already_paused`), which tells the operator the first one took effect. The aggregates decide; this module only
//! names the command, so a refusal comes back as the aggregate's own code
//! (`already_paused`, `not_draining`, `operator_only`, ...).
//!
//! These functions return no read model (`ARCH-command-isolation`): what changed is read back
//! through `crate::queries` by the caller.

use mayfly_control_domain::host::{Host, HostCommand};
use mayfly_control_domain::pool::{Pool, PoolCommand};
use mayfly_control_domain::{Actor, Aggregate, Command, Rejection};
use mayfly_domain::{CommandId, Deadline, Epoch, HostId, ImageDigest, Millis, PoolName};

use crate::ids::derive;
use crate::journal::{Journal, StreamRevision};
use crate::store::{AggregateStore, Clock, Outcome, Rng, SaveError, SaveRequest, StoreNotifier};

const ID_DOMAIN: &[u8] = b"mayfly-operator-command-v1\n";

/// Why an operator command recorded nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    /// The aggregate refused, by its code (a refusal receipt was recorded).
    Refused(&'static str),
    /// The store gave no outcome.
    Save(SaveError),
}

impl CommandError {
    /// A fixed `snake_case` code for the socket's answer and logs.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Refused(code) => code,
            Self::Save(_) => "save_failed",
        }
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(code) => write!(f, "refused: {code}"),
            Self::Save(e) => write!(f, "not decided: {e}"),
        }
    }
}

impl std::error::Error for CommandError {}

/// What `mayfly pool pause|resume|drain` asks of a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolAction {
    /// `PausePool`: nothing new is admitted for the pool; its VMs are left as they are.
    Pause,
    /// `ResumePool`: a paused, draining or drained pool admits again.
    Resume,
    /// `StartPoolDrain`: the pool's prepared VMs are retired and busy ones finish (P23).
    Drain,
}

impl PoolAction {
    const fn name(self) -> &'static [u8] {
        match self {
            Self::Pause => b"pause_pool",
            Self::Resume => b"resume_pool",
            Self::Drain => b"drain_pool",
        }
    }

    const fn command(self) -> PoolCommand {
        match self {
            Self::Pause => PoolCommand::PausePool,
            Self::Resume => PoolCommand::ResumePool,
            Self::Drain => PoolCommand::StartPoolDrain,
        }
    }
}

/// What `mayfly host drain|resume` asks of a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAction {
    /// `StartHostDrain`: the host admits nothing new; its prepared VMs are retired and busy
    /// ones finish, for `within` from the command's issue (past it an alert, never a force:
    /// P23). The command's id names `within`, not the instant it ends at, so the same request
    /// repeated before the host stream moves is answered by its receipt.
    Drain {
        /// How long busy attempts are given.
        within: Millis,
    },
    /// `CancelHostDrain`: the host admits again.
    Resume,
}

fn revision_bytes(head: Option<StreamRevision>) -> Vec<u8> {
    head.map_or_else(Vec::new, |r| r.get().to_be_bytes().to_vec())
}

fn decided<R: Rejection>(saved: Result<Outcome<R>, SaveError>) -> Result<(), CommandError> {
    match saved {
        Ok(Outcome::Accepted { .. }) => Ok(()),
        Ok(Outcome::Refused { rejection, .. }) => Err(CommandError::Refused(rejection.code())),
        Err(error) => Err(CommandError::Save(error)),
    }
}

/// Runs `body` against `instance` of `A` by `actor` under `run_epoch`, its id derived from
/// `parts` and the stream's revision read now.
async fn issue<A, J, C, R, N>(
    store: &AggregateStore<J, C, R, N>,
    instance: &str,
    parts: &[&[u8]],
    actor: Actor,
    run_epoch: Epoch,
    body: A::Body,
) -> Result<(), CommandError>
where
    A: Aggregate,
    J: Journal,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
{
    let (_, head) = store
        .load_with_head::<A>(instance)
        .await
        .map_err(CommandError::Save)?;
    let revision = revision_bytes(head);
    let epoch = run_epoch.get().to_be_bytes();
    let mut all: Vec<&[u8]> = vec![A::NAME.as_bytes(), instance.as_bytes(), &epoch, &revision];
    all.extend_from_slice(parts);
    let command = Command::new(
        CommandId::from_u128(derive(ID_DOMAIN, &all)),
        store.clock().now(),
        run_epoch,
        actor,
        body,
    );
    decided(
        store
            .execute::<A>(SaveRequest::new(instance, command))
            .await,
    )
}

/// `mayfly pool pause|resume|drain <pool>`. Pinned by
/// `tests/store/operator.rs::should_pause_and_resume_a_pool_when_an_operator_asks` and
/// `should_answer_the_aggregates_code_when_the_pool_is_already_paused`.
///
/// # Errors
/// [`CommandError`]: the `Pool` aggregate's code (`not_configured`, `already_paused`,
/// `not_paused`, `already_draining`, `decommissioned`, `stale_run`).
pub async fn pool_action<J, C, R, N>(
    store: &AggregateStore<J, C, R, N>,
    pool: &PoolName,
    action: PoolAction,
    actor: Actor,
    run_epoch: Epoch,
) -> Result<(), CommandError>
where
    J: Journal,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
{
    issue::<Pool, J, C, R, N>(
        store,
        pool.as_str(),
        &[action.name()],
        actor,
        run_epoch,
        action.command(),
    )
    .await
}

/// Why a breaker reset stopped: the digests reset before it, and the refusal or fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetError {
    /// The digests whose breaker was reset before the error, in order.
    pub reset: Vec<ImageDigest>,
    /// What stopped it.
    pub error: CommandError,
}

/// `mayfly pool reset-breaker <pool> [--digest <digest>]`: resets the breaker of `digest`, or
/// of every digest whose breaker is tripped, and returns the digests reset, in order. The
/// aggregate records the operator's uid with each reset and refuses any other actor
/// (`operator_only`). Pinned by
/// `tests/store/operator.rs::should_reset_every_tripped_breaker_when_no_digest_is_named` and
/// `should_refuse_a_reset_when_no_breaker_is_tripped`.
///
/// # Errors
/// [`ResetError`]: `not_tripped` when nothing is tripped (or the named digest is not), else
/// the aggregate's code; a refusal of one digest stops before the next, and the error carries
/// the digests already reset.
pub async fn reset_breakers<J, C, R, N>(
    store: &AggregateStore<J, C, R, N>,
    pool: &PoolName,
    digest: Option<&ImageDigest>,
    actor: Actor,
    run_epoch: Epoch,
) -> Result<Vec<ImageDigest>, ResetError>
where
    J: Journal,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
{
    let failed = |reset: Vec<ImageDigest>, error| ResetError { reset, error };
    let digests: Vec<ImageDigest> = match digest {
        Some(digest) => vec![digest.clone()],
        None => store
            .load::<Pool>(pool.as_str())
            .await
            .map_err(|e| failed(Vec::new(), CommandError::Save(e)))?
            .breakers
            .iter()
            .filter(|(_, breaker)| breaker.tripped_at.is_some())
            .map(|(digest, _)| digest.clone())
            .collect(),
    };
    if digests.is_empty() {
        return Err(failed(Vec::new(), CommandError::Refused("not_tripped")));
    }
    for (done, digest) in digests.iter().enumerate() {
        issue::<Pool, J, C, R, N>(
            store,
            pool.as_str(),
            &[b"reset_breaker", digest.as_str().as_bytes()],
            actor,
            run_epoch,
            PoolCommand::ResetBreaker {
                digest: digest.clone(),
            },
        )
        .await
        .map_err(|error| failed(digests[..done].to_vec(), error))?;
    }
    Ok(digests)
}

/// `mayfly host drain|resume`. Pinned by
/// `tests/store/operator.rs::should_drain_and_resume_a_host_when_an_operator_asks`.
///
/// # Errors
/// [`CommandError`]: the `Host` aggregate's code (`not_registered`, `already_draining`,
/// `not_draining`, `stale_run`).
pub async fn host_action<J, C, R, N>(
    store: &AggregateStore<J, C, R, N>,
    host: &HostId,
    action: HostAction,
    actor: Actor,
    run_epoch: Epoch,
) -> Result<(), CommandError>
where
    J: Journal,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
{
    let at = match action {
        HostAction::Drain { within } => within.get().to_be_bytes(),
        HostAction::Resume => [0; 8],
    };
    let (name, body): (&[u8], HostCommand) = match action {
        HostAction::Drain { within } => (
            b"start_host_drain",
            HostCommand::StartHostDrain {
                deadline: Deadline::after(store.clock().now(), within),
            },
        ),
        HostAction::Resume => (b"cancel_host_drain", HostCommand::CancelHostDrain),
    };
    issue::<Host, J, C, R, N>(store, host.as_str(), &[name, &at], actor, run_epoch, body).await
}
