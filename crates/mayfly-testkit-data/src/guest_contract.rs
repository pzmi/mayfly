//! The `GuestTransport` contract suite (ver §5, plan §8.3, ADR 0038, ADR 0082): one set of
//! cases every guest transport passes, written once over a [`GuestWorld`] and run from each
//! implementation's crate: [`MemoryGuests`] here ([`MemoryGuestWorld`]), the Tart transport
//! over `mayfly-fake-tart` (`mayfly-adapter-tart/tests/tart/guest.rs`), and over real Tart in
//! the hardware lane (HW-01).
//!
//! Cases are named `GT-01` … ([`GuestCase`]). A world is a transport plus what a case needs
//! around it: booting a VM under its shim, holding a JIT config for a handle, breaking the
//! identity the transport must check (a foreign run-lock holder, a changed MAC, an exec routed
//! to another guest, a guest without its agent), reading what each launcher received and what
//! a diagnostics export wrote (the oracle), rebuilding the transport (a daemon restart), and
//! reporting every child's argv, environment and logged text.
//!
//! [`check_guest_transport_contract`] returns a report of failed cases instead of panicking:
//! the suite's own tests show [`UncheckedLockTransport`] failing GT-05, GT-06 and GT-12
//! (`tests/guest_contract.rs`). A world that cannot build what a case needs fails that case: no
//! case is skipped.

use std::collections::BTreeSet;
use std::fmt::{self, Display};
use std::future::Future;

use mayfly_data::{Diagnostics, GuestAnswer, GuestTransport, LaunchAnswer, RunnerStatus, VmError};
use mayfly_domain::{OwnedVmName, ProviderInstanceId, SecretHandleId};
use mayfly_protocol::{DiagnosticsLoss, GuestRejection, Nonce, SecretHandle};
use mayfly_testkit_shared::child_env::inherited_variable;
use mayfly_testkit_shared::poll::within;
use sha2::{Digest, Sha256};

use mayfly_data::redaction::JIT_VARIABLE;

use crate::guest::{MemoryGuests, MemorySecrets, UncheckedLockTransport};
use crate::provider_contract::{CASE_BOUND, CHILD_ALLOWLIST, vm};

/// The JIT config every case delivers: distinctive, so a leak is found by a substring search.
pub const CANARY: &str = "bWF5Zmx5LWNhbmFyeS1qaXQtY29uZmlnLTdmM2E5";

/// A transport and what a case needs around it.
pub trait GuestWorld {
    /// The transport under test.
    type Transport: GuestTransport;

    /// The transport as it is now.
    fn transport(&self) -> &Self::Transport;

    /// Replaces the transport with a new one over the same VMs and guests and nothing
    /// remembered, as a restarted daemon builds it.
    fn restart(&mut self);

    /// Boots VM `n` ([`vm`]) under its shim with a guest whose launcher is ready, and returns its
    /// instance id.
    fn boot(&self, n: u32) -> impl Future<Output = Result<ProviderInstanceId, String>>;

    /// Holds `config` behind `handle` in the data plane's JIT store, bound to VM `n` and the
    /// instance id its boot answered.
    fn hold(&self, handle: SecretHandle, n: u32, config: &str);

    /// The run lock of VM `n` is held by a process that is not its shim's child.
    fn foreign_lock(&self, n: u32);

    /// VM `n`'s instance id (its MAC) changes under the same name.
    fn change_mac(&self, n: u32);

    /// An exec addressed to VM `n` reaches the guest of VM `to`.
    fn misroute(&self, n: u32, to: u32);

    /// VM `n`'s guest has no guest agent.
    fn without_agent(&self, n: u32);

    /// VM `n`'s runner exits with `code` once launched.
    fn runner_exits(&self, n: u32, code: i32);

    /// VM `n`'s launcher fails after it has read the config, and prints what it read on its
    /// stderr.
    fn fails_after_config(&self, n: u32);

    /// Writes VM `n`'s runner log, which its diagnostics export reads.
    fn write_log(&self, n: u32, text: &str);

    /// The longest runner log an export takes.
    fn diagnostics_limit(&self) -> usize;

    /// SHA-256 (lowercase hex) of each config VM `n`'s launcher received on its stdin: the
    /// oracle.
    fn delivered(&self, n: u32) -> Vec<String>;

    /// The last diagnostics artefact exported for VM `n`, read from where it was written.
    fn artefact(&self, n: u32) -> Option<Vec<u8>>;

    /// Every text a child process the transport ran was given or recorded: its argv, its
    /// environment's values and its logged output; empty when it runs none.
    fn recorded_text(&self) -> Vec<String>;

    /// The variable names each child process the transport ran received; empty when it runs
    /// none.
    fn child_environments(&self) -> Vec<BTreeSet<String>>;
}

/// One case of the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GuestCase {
    /// GT-01: a challenge reaching the VM's own guest is verified.
    VerifyOwnGuest,
    /// GT-02: a challenge answered by another VM's guest is rejected `challenge_failed` (F39).
    VerifyRejectsWrongGuest,
    /// GT-03: a guest without its agent is rejected `agent_missing` (ADR 0038 decision 5).
    VerifyNeedsAgent,
    /// GT-04: a launch delivers the held config to the launcher's stdin once, takes it from
    /// the store, and the launcher's status says the runner runs.
    LaunchDeliversOnce,
    /// GT-05: a launch whose VM's run lock is not held by its shim's child is refused
    /// `IdentityRefused` and delivers nothing (ADR 0038 decision 2, F39).
    ForeignLockRefused,
    /// GT-06: a launch whose VM's instance id is not the recorded one is refused
    /// `IdentityRefused` and delivers nothing (ADR 0026, F39).
    ChangedMacRefused,
    /// GT-07: a launch with no config behind its handle is `Definite` and delivers nothing.
    MissingConfigDefinite,
    /// GT-08: the runner's exit is read from the launcher's status after a restart, and a
    /// launch repeated then is `Delivered` without a second delivery.
    StatusSurvivesRestart,
    /// GT-09: the config never appears in any child's argv, environment or recorded output
    /// (ADR 0014, D13).
    SecretNeverRecorded,
    /// GT-10: a diagnostics export is bounded and redacted: its artefact holds no line naming
    /// the JIT variable, its size and digest are the artefact's, and a log over the bound is
    /// `Lost(over_limit)`.
    DiagnosticsBoundedRedacted,
    /// GT-11: every child gets a cleared environment plus the allowlist (ADR 0058): a variable
    /// of the test's own environment reaches none (the `child_env` probe).
    ChildEnvironmentCleared,
    /// GT-12: a launch whose exec is answered by another VM's guest delivers nothing to either
    /// and is `IdentityRefused`: the launch exec's own handshake checks the guest's MAC (F39).
    LaunchMisroutedRefused,
    /// GT-13: a launcher that fails after the config reached its stdin is `Ambiguous`
    /// (`RunnerLaunchUnknown`, I22), never `Definite` or `Delivered`.
    FailureAfterConfigAmbiguous,
    /// GT-14: a config held for another VM is not delivered: the launch is `Definite` and the
    /// config discarded (ADR 0082).
    ConfigBoundToItsVm,
}

impl GuestCase {
    /// Every case, in id order.
    pub const ALL: [Self; 14] = [
        Self::VerifyOwnGuest,
        Self::VerifyRejectsWrongGuest,
        Self::VerifyNeedsAgent,
        Self::LaunchDeliversOnce,
        Self::ForeignLockRefused,
        Self::ChangedMacRefused,
        Self::MissingConfigDefinite,
        Self::StatusSurvivesRestart,
        Self::SecretNeverRecorded,
        Self::DiagnosticsBoundedRedacted,
        Self::ChildEnvironmentCleared,
        Self::LaunchMisroutedRefused,
        Self::FailureAfterConfigAmbiguous,
        Self::ConfigBoundToItsVm,
    ];

    /// The case's id, `GT-01` …
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::VerifyOwnGuest => "GT-01",
            Self::VerifyRejectsWrongGuest => "GT-02",
            Self::VerifyNeedsAgent => "GT-03",
            Self::LaunchDeliversOnce => "GT-04",
            Self::ForeignLockRefused => "GT-05",
            Self::ChangedMacRefused => "GT-06",
            Self::MissingConfigDefinite => "GT-07",
            Self::StatusSurvivesRestart => "GT-08",
            Self::SecretNeverRecorded => "GT-09",
            Self::DiagnosticsBoundedRedacted => "GT-10",
            Self::ChildEnvironmentCleared => "GT-11",
            Self::LaunchMisroutedRefused => "GT-12",
            Self::FailureAfterConfigAmbiguous => "GT-13",
            Self::ConfigBoundToItsVm => "GT-14",
        }
    }
}

impl Display for GuestCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {self:?}", self.id())
    }
}

/// The failed cases of one run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuestContractReport {
    /// Each failed case and why, in id order.
    pub failures: Vec<(GuestCase, String)>,
}

impl GuestContractReport {
    /// Whether every case passed.
    #[must_use]
    pub fn is_green(&self) -> bool {
        self.failures.is_empty()
    }

    /// The failed cases.
    #[must_use]
    pub fn failed_cases(&self) -> BTreeSet<GuestCase> {
        self.failures.iter().map(|(case, _)| *case).collect()
    }
}

impl Display for GuestContractReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "GuestTransport contract: {} case(s) failed",
            self.failures.len()
        )?;
        for (case, detail) in &self.failures {
            writeln!(f, "- {case}: {detail}")?;
        }
        Ok(())
    }
}

/// A case's outcome.
pub type Check = Result<(), String>;

/// Runs every case, each in a fresh world from `make` on its own runtime.
pub fn check_guest_transport_contract<W: GuestWorld>(make: &dyn Fn() -> W) -> GuestContractReport {
    let mut report = GuestContractReport::default();
    for case in GuestCase::ALL {
        if let Err(detail) = check_case(make, case) {
            report.failures.push((case, detail));
        }
    }
    report
}

/// Runs every case and panics with the report unless all pass.
///
/// # Panics
/// When a case fails; the message lists each failed case and why.
pub fn run_guest_transport_contract<W: GuestWorld>(make: &dyn Fn() -> W) {
    let report = check_guest_transport_contract(make);
    assert!(report.is_green(), "{report}");
}

/// Runs one case in a fresh world on a fresh current-thread runtime with a real clock.
///
/// # Errors
/// What was expected and what the transport did, at the first check that failed.
pub fn check_case<W: GuestWorld>(make: &dyn Fn() -> W, case: GuestCase) -> Check {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("the case's runtime did not build: {e}"))?;
    let mut world = make();
    runtime.block_on(async {
        within(
            CASE_BOUND,
            "the case: a transport future never completed",
            Box::pin(run_case(&mut world, case)),
        )
        .await
        .unwrap_or_else(|timed_out| Err(timed_out.to_string()))
    })
}

async fn run_case<W: GuestWorld>(w: &mut W, case: GuestCase) -> Check {
    match case {
        GuestCase::VerifyOwnGuest => verify_own_guest(w).await,
        GuestCase::VerifyRejectsWrongGuest => verify_rejects_wrong_guest(w).await,
        GuestCase::VerifyNeedsAgent => verify_needs_agent(w).await,
        GuestCase::LaunchDeliversOnce => launch_delivers_once(w).await,
        GuestCase::ForeignLockRefused => identity_refused(w, |w| w.foreign_lock(1)).await,
        GuestCase::ChangedMacRefused => identity_refused(w, |w| w.change_mac(1)).await,
        GuestCase::MissingConfigDefinite => missing_config_definite(w).await,
        GuestCase::StatusSurvivesRestart => status_survives_restart(w).await,
        GuestCase::SecretNeverRecorded => secret_never_recorded(w).await,
        GuestCase::DiagnosticsBoundedRedacted => diagnostics_bounded_redacted(w).await,
        GuestCase::ChildEnvironmentCleared => child_environment_cleared(w).await,
        GuestCase::LaunchMisroutedRefused => launch_misrouted_refused(w).await,
        GuestCase::FailureAfterConfigAmbiguous => failure_after_config_ambiguous(w).await,
        GuestCase::ConfigBoundToItsVm => config_bound_to_its_vm(w).await,
    }
}

/// The handle every case holds its config behind.
#[must_use]
pub const fn handle() -> SecretHandle {
    SecretHandle {
        handle_id: SecretHandleId::from_u128(0x5EC),
    }
}

/// A second handle, for a second config.
#[must_use]
pub const fn other_handle() -> SecretHandle {
    SecretHandle {
        handle_id: SecretHandleId::from_u128(0x5ED),
    }
}

/// SHA-256 of `text`, lowercase hex.
#[must_use]
pub fn sha256_hex(text: &[u8]) -> String {
    Sha256::digest(text)
        .iter()
        .fold(String::new(), |mut out, b| {
            out.push_str(&format!("{b:02x}"));
            out
        })
}

fn expect<T: PartialEq + fmt::Debug>(what: &str, got: &T, want: &T) -> Check {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: expected {want:?}, got {got:?}"))
    }
}

fn nonce() -> Nonce {
    Nonce::from_u128(0x0123_4567_89ab_cdef_0011_2233_4455_6677)
}

async fn verify_own_guest<W: GuestWorld>(w: &mut W) -> Check {
    w.boot(1).await?;
    match w.transport().verify(&vm(1), nonce()).await {
        Ok(GuestAnswer::Verified(_)) => Ok(()),
        other => Err(format!("own guest: expected Verified, got {other:?}")),
    }
}

async fn verify_rejects_wrong_guest<W: GuestWorld>(w: &mut W) -> Check {
    w.boot(1).await?;
    w.boot(2).await?;
    w.misroute(1, 2);
    let got = w.transport().verify(&vm(1), nonce()).await;
    expect(
        "a challenge answered by VM 2's guest",
        &got,
        &Ok(GuestAnswer::Rejected(GuestRejection::ChallengeFailed)),
    )
}

async fn verify_needs_agent<W: GuestWorld>(w: &mut W) -> Check {
    w.boot(1).await?;
    w.without_agent(1);
    let got = w.transport().verify(&vm(1), nonce()).await;
    expect(
        "a guest without its agent",
        &got,
        &Ok(GuestAnswer::Rejected(GuestRejection::AgentMissing)),
    )
}

async fn launch_delivers_once<W: GuestWorld>(w: &mut W) -> Check {
    let id = w.boot(1).await?;
    w.hold(handle(), 1, CANARY);
    let got = w.transport().launch(&vm(1), &id, handle()).await;
    expect("a launch", &got, &Ok(LaunchAnswer::Delivered))?;
    expect(
        "what the launcher received",
        &w.delivered(1),
        &vec![sha256_hex(CANARY.as_bytes())],
    )?;
    let status = w.transport().runner_status(&vm(1)).await;
    expect("the status", &status, &Ok(RunnerStatus::Running))?;
    // the config was taken: holding nothing, a second launch with a new handle has none
    let again = w.transport().launch(&vm(1), &id, other_handle()).await;
    expect("a repeated launch", &again, &Ok(LaunchAnswer::Delivered))?;
    expect("deliveries after a repeat", &w.delivered(1).len(), &1)
}

async fn identity_refused<W: GuestWorld>(w: &mut W, break_identity: impl Fn(&W)) -> Check {
    let id = w.boot(1).await?;
    w.hold(handle(), 1, CANARY);
    break_identity(w);
    let got = w.transport().launch(&vm(1), &id, handle()).await;
    expect("a launch", &got, &Ok(LaunchAnswer::IdentityRefused))?;
    expect("deliveries", &w.delivered(1), &Vec::new())
}

async fn missing_config_definite<W: GuestWorld>(w: &mut W) -> Check {
    let id = w.boot(1).await?;
    let got = w.transport().launch(&vm(1), &id, handle()).await;
    expect("a launch with nothing held", &got, &Err(VmError::Definite))?;
    expect("deliveries", &w.delivered(1), &Vec::new())
}

async fn status_survives_restart<W: GuestWorld>(w: &mut W) -> Check {
    let id = w.boot(1).await?;
    w.runner_exits(1, 3);
    w.hold(handle(), 1, CANARY);
    let got = w.transport().launch(&vm(1), &id, handle()).await;
    expect("a launch", &got, &Ok(LaunchAnswer::Delivered))?;
    w.restart();
    let status = w.transport().runner_status(&vm(1)).await;
    expect(
        "the status after a restart",
        &status,
        &Ok(RunnerStatus::Exited { code: 3 }),
    )?;
    let again = w.transport().launch(&vm(1), &id, handle()).await;
    expect(
        "a launch repeated after a restart",
        &again,
        &Ok(LaunchAnswer::Delivered),
    )?;
    expect("deliveries", &w.delivered(1).len(), &1)
}

async fn secret_never_recorded<W: GuestWorld>(w: &mut W) -> Check {
    let id = w.boot(1).await?;
    let failing = w.boot(2).await?;
    w.hold(handle(), 1, CANARY);
    w.hold(other_handle(), 2, CANARY);
    w.fails_after_config(2);
    let got = w.transport().launch(&vm(1), &id, handle()).await;
    expect("a launch", &got, &Ok(LaunchAnswer::Delivered))?;
    let failed = w.transport().launch(&vm(2), &failing, other_handle()).await;
    if !matches!(failed, Err(VmError::Ambiguous(_))) {
        return Err(format!(
            "a launcher failing after the config: expected Ambiguous, got {failed:?}"
        ));
    }
    let _status = w.transport().runner_status(&vm(1)).await;
    let _exported = w.transport().export_diagnostics(&vm(1)).await;
    let leaked: Vec<String> = w
        .recorded_text()
        .into_iter()
        .filter(|text| text.contains(CANARY))
        .collect();
    expect("recorded text holding the config", &leaked, &Vec::new())
}

async fn diagnostics_bounded_redacted<W: GuestWorld>(w: &mut W) -> Check {
    w.boot(1).await?;
    w.write_log(
        1,
        &format!(
            "runner started\n{JIT_VARIABLE}=c2VjcmV0LWNvbmZpZw\n{CANARY}\n\x1b[0mghs_\x1b[0mtoken\nrunner listening\n"
        ),
    );
    let got = w.transport().export_diagnostics(&vm(1)).await;
    let Ok(Diagnostics::Exported { bytes, sha256 }) = got else {
        return Err(format!("an export: expected Exported, got {got:?}"));
    };
    let artefact = w
        .artefact(1)
        .ok_or_else(|| "no artefact was written".to_owned())?;
    let text = String::from_utf8_lossy(&artefact);
    if text.contains("c2VjcmV0LWNvbmZpZw")
        || text.contains(JIT_VARIABLE)
        || text.contains(CANARY)
        || text.contains("token")
    {
        return Err(format!("the artefact is not redacted: {text:?}"));
    }
    if !text.contains("runner listening") {
        return Err(format!("the artefact lost the log: {text:?}"));
    }
    expect(
        "the artefact's size",
        &bytes,
        &u64::try_from(artefact.len()).unwrap_or(u64::MAX),
    )?;
    expect(
        "the artefact's digest",
        &sha256,
        &<[u8; 32]>::from(Sha256::digest(&artefact)),
    )?;
    w.write_log(1, &"x".repeat(w.diagnostics_limit() + 1));
    let over = w.transport().export_diagnostics(&vm(1)).await;
    expect(
        "an export over the bound",
        &over,
        &Ok(Diagnostics::Lost(DiagnosticsLoss::OverLimit)),
    )
}

async fn launch_misrouted_refused<W: GuestWorld>(w: &mut W) -> Check {
    let id = w.boot(1).await?;
    w.boot(2).await?;
    w.hold(handle(), 1, CANARY);
    w.misroute(1, 2);
    let got = w.transport().launch(&vm(1), &id, handle()).await;
    expect(
        "a launch answered by VM 2's guest",
        &got,
        &Ok(LaunchAnswer::IdentityRefused),
    )?;
    expect("deliveries to VM 1", &w.delivered(1), &Vec::new())?;
    expect("deliveries to VM 2", &w.delivered(2), &Vec::new())
}

async fn failure_after_config_ambiguous<W: GuestWorld>(w: &mut W) -> Check {
    let id = w.boot(1).await?;
    w.hold(handle(), 1, CANARY);
    w.fails_after_config(1);
    let got = w.transport().launch(&vm(1), &id, handle()).await;
    match got {
        Err(VmError::Ambiguous(_)) => Ok(()),
        other => Err(format!(
            "a launcher failing after the config: expected Ambiguous, got {other:?}"
        )),
    }
}

async fn config_bound_to_its_vm<W: GuestWorld>(w: &mut W) -> Check {
    let id = w.boot(1).await?;
    w.boot(2).await?;
    w.hold(handle(), 2, CANARY);
    let got = w.transport().launch(&vm(1), &id, handle()).await;
    expect(
        "a launch of a config held for VM 2",
        &got,
        &Err(VmError::Definite),
    )?;
    expect("deliveries", &w.delivered(1), &Vec::new())
}

async fn child_environment_cleared<W: GuestWorld>(w: &mut W) -> Check {
    let Some(probe) = inherited_variable() else {
        return Err("this process has no variable to probe with".to_owned());
    };
    let id = w.boot(1).await?;
    w.hold(handle(), 1, CANARY);
    let _verified = w.transport().verify(&vm(1), nonce()).await;
    let _launched = w.transport().launch(&vm(1), &id, handle()).await;
    let _status = w.transport().runner_status(&vm(1)).await;
    let _exported = w.transport().export_diagnostics(&vm(1)).await;
    for names in w.child_environments() {
        if names.contains(&probe) {
            return Err(format!("a child inherited {probe}: {names:?}"));
        }
        let extra: Vec<&String> = names
            .iter()
            .filter(|n| !CHILD_ALLOWLIST.contains(&n.as_str()))
            .collect();
        if !extra.is_empty() {
            return Err(format!(
                "a child got names outside the allowlist: {extra:?}"
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The in-memory world

/// The contract's world over [`MemoryGuests`], or over its [`UncheckedLockTransport`] twin.
#[derive(Debug)]
pub struct MemoryGuestWorld<T> {
    guests: MemoryGuests,
    secrets: MemorySecrets,
    transport: T,
    build: fn(&MemoryGuests) -> T,
}

/// The longest runner log the in-memory world exports.
pub const MEMORY_DIAGNOSTICS_LIMIT: usize = 4096;

impl MemoryGuestWorld<MemoryGuests> {
    /// The fake that keeps the contract.
    #[must_use]
    pub fn new() -> Self {
        Self::over(MemoryGuests::reopened)
    }
}

impl Default for MemoryGuestWorld<MemoryGuests> {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryGuestWorld<UncheckedLockTransport> {
    /// The broken transport that skips the identity check.
    #[must_use]
    pub fn unchecked() -> Self {
        Self::over(UncheckedLockTransport::new)
    }
}

impl<T: GuestTransport> MemoryGuestWorld<T> {
    fn over(build: fn(&MemoryGuests) -> T) -> Self {
        let secrets = MemorySecrets::default();
        let guests = MemoryGuests::new(secrets.clone(), MEMORY_DIAGNOSTICS_LIMIT);
        let transport = build(&guests);
        Self {
            guests,
            secrets,
            transport,
            build,
        }
    }

    fn id(n: u32) -> Result<ProviderInstanceId, String> {
        ProviderInstanceId::new(&format!("0a:00:00:00:{:02x}:{:02x}", n >> 8, n & 0xff))
            .map_err(|e| format!("instance id of VM {n}: {e:?}"))
    }
}

fn vm_of(n: u32) -> OwnedVmName {
    vm(n)
}

impl<T: GuestTransport> GuestWorld for MemoryGuestWorld<T> {
    type Transport = T;

    fn transport(&self) -> &T {
        &self.transport
    }

    fn restart(&mut self) {
        self.transport = (self.build)(&self.guests);
    }

    async fn boot(&self, n: u32) -> Result<ProviderInstanceId, String> {
        let id = Self::id(n)?;
        self.guests.boot(&vm_of(n), &id);
        Ok(id)
    }

    fn hold(&self, handle: SecretHandle, n: u32, config: &str) {
        if let Ok(id) = Self::id(n) {
            let _held = self.secrets.put(handle, &vm_of(n), &id, config);
        }
    }

    fn fails_after_config(&self, n: u32) {
        self.guests.fails_after_config(&vm_of(n));
    }

    fn foreign_lock(&self, n: u32) {
        self.guests.foreign_lock(&vm_of(n));
    }

    fn change_mac(&self, n: u32) {
        if let Ok(id) = Self::id(n + 0x100) {
            self.guests.change_mac(&vm_of(n), &id);
        }
    }

    fn misroute(&self, n: u32, to: u32) {
        self.guests.misroute(&vm_of(n), &vm_of(to));
    }

    fn without_agent(&self, n: u32) {
        self.guests.without_agent(&vm_of(n));
    }

    fn runner_exits(&self, n: u32, code: i32) {
        self.guests.runner_exits(&vm_of(n), code);
    }

    fn write_log(&self, n: u32, text: &str) {
        self.guests.write_log(&vm_of(n), text);
    }

    fn diagnostics_limit(&self) -> usize {
        MEMORY_DIAGNOSTICS_LIMIT
    }

    fn delivered(&self, n: u32) -> Vec<String> {
        self.guests
            .delivered(&vm_of(n))
            .iter()
            .map(|c| sha256_hex(c.as_bytes()))
            .collect()
    }

    fn artefact(&self, n: u32) -> Option<Vec<u8>> {
        self.guests.artefact(&vm_of(n))
    }

    fn recorded_text(&self) -> Vec<String> {
        Vec::new()
    }

    fn child_environments(&self) -> Vec<BTreeSet<String>> {
        Vec::new()
    }
}
