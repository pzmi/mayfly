//! The process runner (plan §8.2, D12): one `tart` call as a child process.
//!
//! argv only, never a shell; a cleared environment plus exactly what the caller passes
//! (`ARCH-child-environment`, ADR 0058); `/` as working directory; a null stdin; its own process
//! group, killed whole at the deadline and when the call is dropped, then reaped; stdout kept up
//! to a limit (what passes it is drained and reported as truncated, so the child never blocks on
//! a full pipe); stderr kept bounded and redacted ([`redact`]).
//!
//! A runner future completes only once nothing it started still runs: at the deadline the group
//! is killed and the child reaped before [`Ran::TimedOut`] is returned, so an adapter call built
//! on it is bounded by its deadline plus the kill (`mayfly_data::ports`: the executor never
//! drops an effect's future). Pinned by
//! `tests/tart/runner.rs::should_kill_the_whole_group_and_answer_timed_out_when_the_deadline_passes`
//! and `tests/tart/runner.rs::should_kill_the_group_when_the_call_is_dropped`.

use std::ffi::OsString;
use std::io::{self, ErrorKind};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

/// The most bytes of stderr kept, after redaction.
pub const STDERR_LIMIT: usize = 4096;

/// What masks a redacted value.
pub const REDACTED: &str = "[redacted]";

/// One call.
#[derive(Debug, Clone)]
pub struct Call<'a> {
    /// The executable.
    pub program: &'a Path,
    /// Its arguments.
    pub args: Vec<OsString>,
    /// Exactly the environment it gets.
    pub environment: &'a [(OsString, OsString)],
    /// When it is stopped.
    pub deadline: Duration,
    /// The most bytes of stdout kept.
    pub output_limit: usize,
    /// Values masked in its stderr.
    pub redact: &'a [String],
}

/// How a call ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ran {
    /// It exited (or was killed by someone else) within its deadline.
    Exited(Exited),
    /// The deadline passed: the group was killed and the child reaped.
    TimedOut,
    /// It could not be started: nothing ran.
    SpawnFailed(ErrorKind),
    /// Its output or exit could not be read: it was killed and reaped, its effect unknown.
    Lost(ErrorKind),
}

/// A call that exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exited {
    /// Its exit code; `None` when a signal ended it.
    pub code: Option<i32>,
    /// The signal that ended it.
    pub signal: Option<i32>,
    /// Its stdout, at most the call's limit.
    pub stdout: Vec<u8>,
    /// Whether stdout passed the limit (what was kept is not the whole output).
    pub truncated: bool,
    /// Its stderr, bounded and redacted.
    pub stderr: String,
}

impl Exited {
    /// Whether it exited with code 0.
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs `call` to its end or its deadline.
pub async fn run(call: Call<'_>) -> Ran {
    let mut command = std::process::Command::new(call.program);
    command
        .env_clear()
        .envs(call.environment.iter().map(|(name, value)| (name, value)))
        .args(&call.args)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut command = Command::from(command);
    command.kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return Ran::SpawnFailed(err.kind()),
    };
    let mut group = GroupKill::of(&child);
    let outcome = tokio::time::timeout(call.deadline, finish(&mut child, call.output_limit)).await;
    match outcome {
        Ok(Ok(mut exited)) => {
            // Reaped: its id may be reused from here on, so the group is left alone.
            group.disarm();
            exited.stderr = redact(exited.stderr.as_bytes(), call.redact);
            Ran::Exited(exited)
        }
        Ok(Err(err)) => {
            group.kill();
            let _reaped = child.wait().await;
            Ran::Lost(err.kind())
        }
        Err(_elapsed) => {
            group.kill();
            let _reaped = child.wait().await;
            Ran::TimedOut
        }
    }
}

/// Reads both streams to their end, then reaps the child.
async fn finish(child: &mut Child, limit: usize) -> io::Result<Exited> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (out, err) = tokio::join!(bounded(stdout, limit), bounded(stderr, STDERR_LIMIT));
    let (stdout, truncated) = out?;
    let (stderr, _) = err?;
    let status = child.wait().await?;
    Ok(Exited {
        code: status.code(),
        signal: status.signal(),
        stdout,
        truncated,
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// Reads `stream` to its end, keeping at most `limit` bytes and draining the rest.
async fn bounded(
    stream: Option<impl AsyncRead + Unpin>,
    limit: usize,
) -> io::Result<(Vec<u8>, bool)> {
    let Some(mut stream) = stream else {
        return Ok((Vec::new(), false));
    };
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut chunk = vec![0_u8; 8192];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok((kept, truncated));
        }
        let room = limit.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..read.min(room)]);
        truncated |= read > room;
    }
}

/// `text` as it may be kept or shown: every `secrets` value masked, control characters but
/// newline and tab dropped, at most [`STDERR_LIMIT`] bytes. Pinned by
/// `tests::should_mask_secrets_and_drop_control_characters_when_redacting`.
#[must_use]
pub fn redact(text: &[u8], secrets: &[String]) -> String {
    let mut out = String::from_utf8_lossy(text).into_owned();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        out = out.replace(secret.as_str(), REDACTED);
    }
    out.retain(|c| c == '\n' || c == '\t' || !c.is_control());
    let mut cut = out.len().min(STDERR_LIMIT);
    while !out.is_char_boundary(cut) {
        cut -= 1;
    }
    out.truncate(cut);
    out
}

/// Kills the child's whole process group when told to or when dropped armed (the future was
/// cancelled). Only while the child is not yet reaped, so its id cannot name another group.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_mask_secrets_and_drop_control_characters_when_redacting() {
        // given
        let text = b"Error: token ghs_abc failed\x1b[31m\nagain ghs_abc\0";
        let secrets = vec!["ghs_abc".to_owned(), String::new()];

        // when
        let kept = redact(text, &secrets);

        // then
        assert_eq!(kept, "Error: token [redacted] failed[31m\nagain [redacted]");
    }

    #[test]
    fn should_keep_at_most_the_limit_on_a_char_boundary_when_stderr_is_long() {
        // given
        let text = "é".repeat(STDERR_LIMIT);

        // when
        let kept = redact(text.as_bytes(), &[]);

        // then
        assert!(kept.len() <= STDERR_LIMIT);
        assert!(kept.chars().all(|c| c == 'é'));
    }
}
