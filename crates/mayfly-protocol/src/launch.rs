//! The JIT config on the link (plan §3.2, ADR 0014, ADR 0082; LNK-02, protocol 1.6
//! [`crate::LAUNCH_CONFIG`]): the two stores the link's ends reach when a `LaunchRunner` is sent
//! and received.
//!
//! The journal, the relay and the `Directive` name only the [`SecretHandle`]. The link's
//! control-plane end takes the config from the control plane's single-take broker
//! ([`LaunchConfigSource`]) when it sends the launch, into the one frame that carries it, which
//! is built by hand beside the encoded message and held by no `Serialize` type
//! (`ARCH-secret-serialize`); a resend finds nothing to take and carries nothing. The link's
//! data-plane end puts a config it receives into the data plane's store ([`LaunchConfigSink`])
//! before it hands the directive up, bound to the launch's VM, instance id and deadline, only
//! for its own session's epoch, and empties it when a new session opens. The rules are
//! `formal/link.qnt`'s `relaySendWith`, `dpDeliverWith` and `dpWelcomeWith` (`configSentOnce`,
//! `configDeliveredOnce`, `configOnlyInItsEpoch`).
//!
//! Both are synchronous and in memory: nothing here waits, logs or persists a config.

use mayfly_domain::{AttemptId, Deadline, Epoch, OwnedVmName, ProviderInstanceId, Secret};

use crate::directive::SecretHandle;

/// The control plane's configs held for their launch, as the link's control-plane end takes
/// them (`mayfly_control::registration::JitBroker`).
pub trait LaunchConfigSource: Send + Sync {
    /// The config behind `handle`, removed, when it was held for `attempt` and minted under
    /// `epoch`, the run epoch the launch is sent in. A config held for another attempt or epoch
    /// is discarded and `None` answered, as is a second take: a config is carried at most once
    /// and never into another session (`formal/link.qnt`, `configSentOnce`,
    /// `configOnlyInItsEpoch`).
    fn take(
        &self,
        handle: SecretHandle,
        attempt: AttemptId,
        epoch: Epoch,
    ) -> Option<Secret<String>>;
}

/// What a config the data plane receives is bound to: the launch directive's VM, the instance
/// id it names and its deadline (ADR 0082 decision 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchTarget {
    /// The exact VM the config may be delivered to.
    pub vm_name: OwnedVmName,
    /// The instance id that VM must have.
    pub instance_id: ProviderInstanceId,
    /// The directive's deadline, in the control plane's clock: the config is dropped
    /// undelivered once it passed.
    pub deadline: Deadline,
}

/// A put for a handle held or already used: the config is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PutRefused;

/// The data plane's configs held for delivery, as the link's data-plane end fills them
/// (`mayfly_data::LaunchStore`).
pub trait LaunchConfigSink: Send + Sync {
    /// Holds `config` behind `handle` for `target`.
    ///
    /// # Errors
    /// [`PutRefused`] when the handle is held or was used before: a duplicated frame never
    /// replaces a config, so it is delivered at most once (`configDeliveredOnce`).
    fn put(
        &self,
        handle: SecretHandle,
        target: LaunchTarget,
        config: Secret<String>,
    ) -> Result<(), PutRefused>;

    /// Drops every config held, keeping the handles it used: called when the data plane enters
    /// a new session. None was put for that session, and the store is keyed by the handle alone,
    /// so a launch of the new session would otherwise take a config of another epoch
    /// (`formal/link.qnt`'s `dpWelcomeWith`, `heldConfigNeverCrossesSessionsTest`).
    fn discard_held(&self);
}
