//! The adapter's one privileged module (ADR 0058): the only place in the control plane that
//! names a process API (`PROCESS_MODULES` in the architecture checker) and the adapter's one
//! blocking hand-off (`SPAWN_MODULES`).
//!
//! The runner, for `exec:` commands and the Keychain tool: argv only, never a shell; a cleared
//! environment plus only what the caller passes (`ARCH-child-environment`); `/` as working
//! directory; a null stdin and a null stderr (a failing command's stderr may carry the secret);
//! its own process group, killed whole at the deadline, as soon as stdout passes the port's
//! limit (refused, never truncated), and when the call is dropped.
//!
//! Pinned by the contract's EX cases (`tests/contract.rs`: EX-02 the deadline, EX-03 argv,
//! EX-04 the environment, EX-08 the limit) and by
//! `tests/contract.rs::should_kill_what_the_command_forked_when_the_call_is_dropped`.

use std::ffi::OsString;
use std::io::{self, ErrorKind};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use mayfly_control::secrets::{MAX_SECRET_BYTES, Refusal, SecretError};
use mayfly_domain::SecretRef;
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdout, Command};

/// A command that finished within its deadline. No `Debug`: `stdout` may be the secret.
pub struct Finished {
    /// How it ended.
    pub status: ExitStatus,
    /// Its output, at most the caller's limit.
    pub stdout: Vec<u8>,
}

/// The user id the daemon runs as: the owner its secret files must have.
pub fn daemon_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Runs `work` (file system checks and reads) on Tokio's blocking pool, off the runtime's
/// workers (foundation.md §4.4 rule 3). Dropping the future lets `work` finish and drops its
/// result.
///
/// # Errors
/// `work`'s refusal, or `Io` when it panicked.
pub async fn blocking<T: Send + 'static>(
    reference: &SecretRef,
    work: impl FnOnce() -> Result<T, Refusal> + Send + 'static,
) -> Result<T, SecretError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(done) => done.map_err(|refusal| SecretError::new(reference, refusal)),
        Err(_) => Err(SecretError::new(
            reference,
            Refusal::Io {
                kind: ErrorKind::Other,
            },
        )),
    }
}

/// Runs `program` with `args`, giving it exactly `environment`, and waits at most `deadline`
/// for at most `limit` bytes of stdout.
///
/// Cancel-safe: dropping the future kills the command's process group before the command is
/// reaped (`GroupKill`).
///
/// # Errors
/// `NotFound` when `program` does not exist, `TimedOut` when the deadline passes, `TooLarge`
/// as soon as stdout passes the limit (both: the process group is killed and reaped), `Io` for
/// any other spawn or read failure.
pub async fn run(
    reference: &SecretRef,
    program: &Path,
    args: &[&str],
    environment: &[(OsString, OsString)],
    deadline: Duration,
    limit: usize,
) -> Result<Finished, SecretError> {
    let mut command = std::process::Command::new(program);
    command
        .env_clear()
        .envs(environment.iter().map(|(name, value)| (name, value)))
        .args(args)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut command = Command::from(command);
    command.kill_on_drop(true);
    let mut child = command.spawn().map_err(|err| {
        let refusal = match err.kind() {
            ErrorKind::NotFound => Refusal::NotFound,
            kind => Refusal::Io { kind },
        };
        SecretError::new(reference, refusal)
    })?;
    let mut group = GroupKill::of(&child);
    let Some(mut stdout) = child.stdout.take() else {
        return Err(SecretError::new(
            reference,
            Refusal::Io {
                kind: ErrorKind::BrokenPipe,
            },
        ));
    };
    let outcome = tokio::time::timeout(deadline, finish(&mut child, &mut stdout, limit)).await;
    let refusal = match outcome {
        Ok(Ok(Some(finished))) => {
            // Reaped: its id may be reused from here on, so the group is left alone.
            group.disarm();
            return Ok(finished);
        }
        // The port's limit, whatever margin a caller's tool needs.
        Ok(Ok(None)) => Refusal::TooLarge {
            limit: MAX_SECRET_BYTES,
        },
        Ok(Err(err)) => Refusal::Io { kind: err.kind() },
        Err(_elapsed) => Refusal::TimedOut {
            after_ms: u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
        },
    };
    group.kill();
    let _reaped = child.wait().await;
    Err(SecretError::new(reference, refusal))
}

/// Reads stdout to its end, then reaps the child; `None` as soon as stdout passes `limit`,
/// with the child still running.
async fn finish(
    child: &mut Child,
    stdout: &mut ChildStdout,
    limit: usize,
) -> io::Result<Option<Finished>> {
    let mut kept = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = stdout.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        kept.extend_from_slice(&chunk[..read]);
        if kept.len() > limit {
            return Ok(None);
        }
    }
    let status = child.wait().await?;
    Ok(Some(Finished {
        status,
        stdout: kept,
    }))
}

/// Kills the child's whole process group (what it forked included) when told to or when
/// dropped armed: the future was cancelled, or an early return left the child running. Only
/// while the child is not yet reaped, so its id cannot name another process's group.
struct GroupKill {
    leader: Option<Pid>,
}

impl GroupKill {
    fn of(child: &Child) -> Self {
        Self {
            leader: child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .and_then(Pid::from_raw),
        }
    }

    fn kill(&mut self) {
        if let Some(leader) = self.leader.take() {
            let _gone = kill_process_group(leader, Signal::KILL);
        }
    }

    fn disarm(&mut self) {
        self.leader = None;
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        self.kill();
    }
}
