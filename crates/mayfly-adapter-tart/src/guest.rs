//! `TartGuestTransport`: the `GuestTransport` port over `tart exec` only (plan §8.3, ADR 0038,
//! ADR 0082). No SSH, no network address: every call names the exact VM and the launcher's
//! fixed path in the image, and anything else crosses on stdin.
//!
//! - **verify**: `tart get` reads the VM's MAC (its instance id, ADR 0026), then
//!   `tart exec -i <vm> <launcher> verify` writes the nonce on stdin and reads back the
//!   guest's claims ([`claims`]: typed, bounded). Verified only when the claims echo the nonce
//!   and name the MAC Tart lists for that VM: an exec answered by another guest fails
//!   (`challenge_failed`, F39); an exec Tart cannot deliver for want of the guest agent is
//!   `agent_missing` (ADR 0038 decision 5). The claims never decide ownership (I30).
//! - **launch**: right before the delivery, the identity check of ADR 0038 decision 2: the
//!   shim of the VM is alive (ADR 0037), the process holding the VM's run lock (Tart's lock on
//!   `$TART_HOME/vms/<vm>/config.json`, read with `fcntl(F_GETLK)`) is its recorded child, and
//!   `tart get` lists the VM running with the recorded instance id; any mismatch is
//!   `IdentityRefused`. Then the launcher's status (a runner already launched is `Delivered`
//!   without a second delivery). Then the config is taken from the data plane's
//!   [`LaunchSecrets`] (bound to this VM and instance id) and `tart exec -i <vm> <launcher>`
//!   holds a handshake on its own stdin: `<nonce> <mac>` out, and only the answer
//!   `ok <nonce> <mac>` lets the config through, so a guest Tart routed elsewhere, or a VM
//!   re-cloned since the host check, gets nothing (`IdentityRefused`). Once any byte of the
//!   config was written, every failure is `Ambiguous`. The handle is discarded however the
//!   launch ends. The config never reaches argv, the environment, a file or a log (ADR 0014).
//! - **runner_status**: `tart exec <vm> <launcher> status`, the launcher's status file in the
//!   guest, which outlives the daemon (plan §8.3: runner exit is read from it after a restart).
//! - **export_diagnostics**: `tart exec <vm> <launcher> diagnostics`, kept up to its bound
//!   (more is `Lost(over_limit)`), redacted by `mayfly_data::redact_log`, then written `0600`
//!   in a directory this process owns, made `0700`; only its size and SHA-256 leave.
//!
//! Every `tart` child is the runner's (argv only, cleared environment plus the allowlist, own
//! process group, deadline, bounded output). Pinned by the `GuestTransport` contract over
//! `mayfly-fake-tart` (`tests/tart/guest.rs`); real Tart's `exec`, run lock and the in-image
//! launcher are confirmed in the hardware lane (HW-01).

use std::ffi::OsString;
use std::fs::{DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use mayfly_data::{
    Ambiguity, Diagnostics, DiagnosticsLoss, GuestAnswer, GuestFacts, GuestRejection,
    GuestTransport, LaunchAnswer, LaunchSecrets, Nonce, RunnerStatus, SecretHandle, Shim, VmError,
    redact_log,
};
use mayfly_domain::{OwnedVmName, ProviderInstanceId};
use rustix::fs::{Mode, OFlags};
use rustix::process::{Flock, FlockType, fcntl_getlk};
use sha2::{Digest, Sha256};

use crate::config::TartConfig;
use crate::process::runner::{self, Call, Exited, Fed, Handshake, Ran, Stdin};
use crate::shim;
use crate::tart::TartCli;

pub mod claims;

/// The launcher's fixed path in every mayfly image (ADR 0082).
pub const LAUNCHER: &str = "/usr/local/libexec/mayfly/launcher";

/// The launcher's exit code for a launch that found a runner already launched.
pub const ALREADY_LAUNCHED: i32 = 3;

/// What Tart prints when it cannot reach a guest's agent over the VM's control socket.
const NO_AGENT: &str = "Guest Agent";

/// Tart's exit code for a runtime error, the one it exits with when the agent is unreachable.
const TART_RUNTIME_ERROR: i32 = 1;

/// Each guest call's own deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestDeadlines {
    /// The challenge.
    pub verify: Duration,
    /// The launch.
    pub launch: Duration,
    /// A status read.
    pub status: Duration,
    /// A diagnostics export.
    pub diagnostics: Duration,
}

/// The transport's configuration beside the Tart adapter's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestConfig {
    /// Each call's deadline.
    pub deadlines: GuestDeadlines,
    /// The most bytes of a claims or status answer kept; more does not parse.
    pub claims_limit: usize,
    /// The most bytes of a runner log exported; more is `Lost(over_limit)`.
    pub diagnostics_limit: usize,
    /// Where exported artefacts are written: `<dir>/<vm>.log`, `0600` in a `0700` directory.
    pub diagnostics_dir: PathBuf,
}

/// The `GuestTransport` over `tart exec`, taking JIT configs from `S`.
#[derive(Debug)]
pub struct TartGuestTransport<S> {
    tart: TartCli,
    guest: GuestConfig,
    secrets: S,
}

/// Why a `tart exec` did not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExecError {
    /// Tart could not reach the guest's agent: nothing ran in the guest.
    NoAgent,
    /// It exited non-zero with this code.
    Exit(i32),
    /// It did not finish by its deadline.
    TimedOut,
    /// It was lost or its output could not be read.
    Lost,
    /// It could not be started.
    SpawnFailed,
}

impl ExecError {
    /// What an effect that ended so means: nothing happened, unless the outcome is unknown.
    const fn as_vm_error(&self) -> VmError {
        match self {
            Self::TimedOut => VmError::Ambiguous(Ambiguity::Deadline),
            Self::Lost => VmError::Ambiguous(Ambiguity::ProcessLost),
            Self::NoAgent | Self::Exit(_) | Self::SpawnFailed => VmError::Definite,
        }
    }
}

impl<S: LaunchSecrets> TartGuestTransport<S> {
    /// A transport over the Tart adapter's `tart` configuration and `guest`'s, taking JIT
    /// configs from `secrets`.
    #[must_use]
    pub fn new(tart: TartConfig, guest: GuestConfig, secrets: S) -> Self {
        Self {
            tart: TartCli::new(tart),
            guest,
            secrets,
        }
    }

    async fn exec(
        &self,
        vm: &OwnedVmName,
        verb: Option<&str>,
        stdin: Stdin<'_>,
        deadline: Duration,
        limit: usize,
    ) -> Result<Exited, ExecError> {
        let interactive = !matches!(stdin, Stdin::Null);
        let mut args: Vec<OsString> = vec!["exec".into()];
        if interactive {
            args.push("-i".into());
        }
        args.push(vm.to_string().into());
        args.push(LAUNCHER.into());
        args.extend(verb.map(OsString::from));
        let ran = runner::run(Call {
            program: &self.tart.config().tart,
            args,
            environment: self.tart.environment(),
            deadline,
            output_limit: limit,
            redact: &self.tart.config().redact,
            stdin,
        })
        .await;
        match ran {
            Ran::Exited(exited) if exited.success() => Ok(exited),
            Ran::Exited(exited) if no_agent(&exited) => Err(ExecError::NoAgent),
            Ran::Exited(Exited {
                code: Some(code), ..
            }) => Err(ExecError::Exit(code)),
            Ran::Exited(_) | Ran::Lost(_) => Err(ExecError::Lost),
            Ran::TimedOut => Err(ExecError::TimedOut),
            Ran::SpawnFailed(_) => Err(ExecError::SpawnFailed),
        }
    }

    /// `tart get <vm>`: its MAC (lower case) and whether it runs; `None` when it does not exist.
    async fn listed(&self, vm: &OwnedVmName) -> Result<Option<(String, bool)>, VmError> {
        match self.tart.get(&vm.to_string()).await {
            Ok(info) => Ok(info.map(|i| (i.instance_text(), i.running))),
            Err(_) => Err(VmError::Definite),
        }
    }

    /// The identity check of ADR 0038 decision 2, right before a delivery: the VM's shim lives,
    /// the process holding its run lock is the shim's recorded child, and Tart lists it running
    /// with `instance_id`.
    async fn identity_holds(
        &self,
        vm: &OwnedVmName,
        instance_id: &ProviderInstanceId,
    ) -> Result<bool, VmError> {
        let name = vm.to_string();
        let reading = shim::read(&shim::dir(&self.tart.config().shims, &name));
        let child = match (reading.shim, reading.run) {
            (Shim::Alive, Some(run)) if run.vm == name => run.child_pid,
            _ => return Ok(false),
        };
        if run_lock_holder(&self.tart.config().tart_home, &name)? != Some(child) {
            return Ok(false);
        }
        Ok(self
            .listed(vm)
            .await?
            .is_some_and(|(mac, running)| running && mac == instance_id.as_str()))
    }

    fn write_artefact(&self, vm: &OwnedVmName, bytes: &[u8]) -> std::io::Result<()> {
        let dir = &self.guest.diagnostics_dir;
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        // an existing directory is ours and owner-only, or nothing is written there
        let meta = std::fs::symlink_metadata(dir)?;
        if !meta.is_dir() || meta.uid() != rustix::process::geteuid().as_raw() {
            return Err(std::io::Error::other("diagnostics directory not owned"));
        }
        if meta.mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = dir.join(format!("{vm}.log"));
        let tmp = dir.join(format!(".{vm}.log.tmp"));
        if let Err(e) = std::fs::remove_file(&tmp)
            && e.kind() != ErrorKind::NotFound
        {
            return Err(e);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)
    }
}

/// Whether Tart could not reach the guest's agent: its runtime-error exit and its message.
fn no_agent(exited: &Exited) -> bool {
    exited.code == Some(TART_RUNTIME_ERROR) && exited.stderr.contains(NO_AGENT)
}

/// Discards a launch's handle however the launch ends (ADR 0082).
struct Discard<'a, S: LaunchSecrets> {
    secrets: &'a S,
    handle: SecretHandle,
}

impl<S: LaunchSecrets> Drop for Discard<'_, S> {
    fn drop(&mut self) {
        self.secrets.discard(self.handle);
    }
}

/// What a launch's exec answered, read with what reached its stdin (ADR 0082): the config is
/// known undelivered only when the handshake declined it; once any of it was written, a
/// failure is `Ambiguous`.
fn launched(ran: Ran, nonce: &str, mac: &str) -> Result<LaunchAnswer, VmError> {
    let exited = match ran {
        Ran::Exited(exited) => exited,
        Ran::TimedOut => return Err(VmError::Ambiguous(Ambiguity::Deadline)),
        Ran::Lost(_) => return Err(VmError::Ambiguous(Ambiguity::ProcessLost)),
        Ran::SpawnFailed(_) => return Err(VmError::Definite),
    };
    match &exited.fed {
        Fed::Written if exited.success() => Ok(LaunchAnswer::Delivered),
        Fed::Written | Fed::Broken => Err(VmError::Ambiguous(Ambiguity::ProcessLost)),
        Fed::Nothing => Err(VmError::Definite),
        Fed::Declined(ack) => {
            let text = String::from_utf8_lossy(ack);
            let words: Vec<&str> = text.split_whitespace().collect();
            match words.as_slice() {
                [_, n, _] if *n != nonce => Err(VmError::Definite),
                [_, _, m] if *m != mac => Ok(LaunchAnswer::IdentityRefused),
                ["launched", _, _] => Ok(LaunchAnswer::Delivered),
                ["mismatch", _, _] => Ok(LaunchAnswer::IdentityRefused),
                _ => Err(VmError::Definite),
            }
        }
    }
}

/// The pid holding `vm`'s run lock: Tart's lock on `vms/<vm>/config.json` under `tart_home`,
/// read with `fcntl(F_GETLK)`. `None` when nothing holds it or the VM has no directory.
fn run_lock_holder(tart_home: &Path, vm: &str) -> Result<Option<u32>, VmError> {
    let path = tart_home.join("vms").join(vm).join("config.json");
    let fd = match rustix::fs::open(&path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err(VmError::Definite),
    };
    let held =
        fcntl_getlk(&fd, &Flock::from(FlockType::WriteLock)).map_err(|_| VmError::Definite)?;
    Ok(held
        .and_then(|lock| lock.pid)
        .and_then(|pid| u32::try_from(pid.as_raw_nonzero().get()).ok()))
}

impl<S: LaunchSecrets> GuestTransport for TartGuestTransport<S> {
    async fn verify(&self, vm: &OwnedVmName, nonce: Nonce) -> Result<GuestAnswer, VmError> {
        let Some((mac, _)) = self.listed(vm).await? else {
            return Err(VmError::Definite);
        };
        let challenge = format!("{nonce}\n");
        let answered = self
            .exec(
                vm,
                Some("verify"),
                Stdin::Bytes(challenge.as_bytes()),
                self.guest.deadlines.verify,
                self.guest.claims_limit,
            )
            .await;
        let out = match answered {
            Ok(out) => out,
            Err(ExecError::NoAgent) => {
                return Ok(GuestAnswer::Rejected(GuestRejection::AgentMissing));
            }
            Err(ExecError::Exit(_)) => {
                return Ok(GuestAnswer::Rejected(GuestRejection::ChallengeFailed));
            }
            Err(other) => return Err(other.as_vm_error()),
        };
        if out.truncated {
            return Ok(GuestAnswer::Rejected(GuestRejection::ChallengeFailed));
        }
        Ok(match claims::parse_claims(&out.stdout) {
            Ok(c) if c.nonce == nonce && c.mac == mac => GuestAnswer::Verified(GuestFacts {
                os: c.os,
                uptime: c.uptime,
            }),
            Ok(_) | Err(_) => GuestAnswer::Rejected(GuestRejection::ChallengeFailed),
        })
    }

    async fn launch(
        &self,
        vm: &OwnedVmName,
        instance_id: &ProviderInstanceId,
        launch: SecretHandle,
    ) -> Result<LaunchAnswer, VmError> {
        let _discard = Discard {
            secrets: &self.secrets,
            handle: launch,
        };
        if !self.identity_holds(vm, instance_id).await? {
            return Ok(LaunchAnswer::IdentityRefused);
        }
        if self.runner_status(vm).await? != RunnerStatus::NotLaunched {
            return Ok(LaunchAnswer::Delivered);
        }
        let config = self
            .secrets
            .take(launch, vm, instance_id)
            .ok_or(VmError::Definite)?;
        let nonce = launch.handle_id.to_string();
        let mac = instance_id.as_str();
        let hello = format!("{nonce} {mac}\n");
        let expect = format!("ok {nonce} {mac}\n");
        let ran = runner::run(Call {
            program: &self.tart.config().tart,
            args: vec![
                "exec".into(),
                "-i".into(),
                vm.to_string().into(),
                LAUNCHER.into(),
            ],
            environment: self.tart.environment(),
            deadline: self.guest.deadlines.launch,
            output_limit: self.guest.claims_limit,
            redact: &self.tart.config().redact,
            stdin: Stdin::Handshake(Handshake {
                hello: hello.as_bytes(),
                expect: expect.as_bytes(),
                secret: &config,
            }),
        })
        .await;
        launched(ran, &nonce, mac)
    }

    async fn runner_status(&self, vm: &OwnedVmName) -> Result<RunnerStatus, VmError> {
        let out = self
            .exec(
                vm,
                Some("status"),
                Stdin::Null,
                self.guest.deadlines.status,
                self.guest.claims_limit,
            )
            .await
            .map_err(|e| e.as_vm_error())?;
        if out.truncated {
            return Err(VmError::Definite);
        }
        claims::parse_status(&out.stdout).map_err(|_| VmError::Definite)
    }

    async fn export_diagnostics(&self, vm: &OwnedVmName) -> Result<Diagnostics, VmError> {
        let answered = self
            .exec(
                vm,
                Some("diagnostics"),
                Stdin::Null,
                self.guest.deadlines.diagnostics,
                self.guest.diagnostics_limit,
            )
            .await;
        let Ok(out) = answered else {
            return Ok(Diagnostics::Lost(DiagnosticsLoss::GuestUnreachable));
        };
        if out.truncated {
            return Ok(Diagnostics::Lost(DiagnosticsLoss::OverLimit));
        }
        let artefact = redact_log(&out.stdout, &self.tart.config().redact).into_bytes();
        if self.write_artefact(vm, &artefact).is_err() {
            return Ok(Diagnostics::Lost(DiagnosticsLoss::Refused));
        }
        Ok(Diagnostics::Exported {
            bytes: u64::try_from(artefact.len()).unwrap_or(u64::MAX),
            sha256: Sha256::digest(&artefact).into(),
        })
    }
}
