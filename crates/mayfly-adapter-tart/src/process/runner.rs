//! The process runner (plan §8.2, D12): one `tart` call as a child process.
//!
//! argv only, never a shell; a cleared environment plus exactly what the caller passes
//! (`ARCH-child-environment`, ADR 0058); `/` as working directory; a null stdin, or the bytes
//! the caller gives written to a pipe and closed (a secret one only from its [`Secret`] and
//! masked in what is kept of stderr, ADR 0038: a JIT config travels only there); its own process
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

use mayfly_domain::Secret;
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// The most bytes of stderr kept, after redaction.
pub const STDERR_LIMIT: usize = 4096;

/// What masks a redacted value.
pub const REDACTED: &str = "[redacted]";

/// What a call's stdin gets.
#[derive(Debug, Clone, Copy)]
pub enum Stdin<'a> {
    /// `/dev/null`.
    Null,
    /// These bytes, then end of file.
    Bytes(&'a [u8]),
    /// This secret's bytes, then end of file; never in argv, the environment or a log, and
    /// masked in the stderr kept (`Debug` shows it redacted).
    Secret(&'a Secret<String>),
    /// A dialogue: `hello`, then the first stdout line is read (at most [`ACK_LIMIT`] bytes);
    /// only when it is exactly `expect` is the secret written and stdin closed, otherwise stdin
    /// is closed with nothing more written ([`Fed::Declined`]). The secret is masked in the
    /// stderr kept, as for [`Stdin::Secret`].
    Handshake(Handshake<'a>),
}

/// The dialogue of [`Stdin::Handshake`].
#[derive(Debug, Clone, Copy)]
pub struct Handshake<'a> {
    /// Written first.
    pub hello: &'a [u8],
    /// The acknowledgement line (with its newline) that lets the secret through.
    pub expect: &'a [u8],
    /// Written only after `expect`.
    pub secret: &'a Secret<String>,
}

/// The most bytes of a handshake's acknowledgement line read.
pub const ACK_LIMIT: usize = 256;

/// What reached the child's stdin. A secret is known undelivered only as [`Fed::Nothing`] or
/// [`Fed::Declined`]; once any of it may have been read ([`Fed::Written`], [`Fed::Broken`]) a
/// failure is ambiguous (pinned by `tests/tart/runner.rs::should_report_what_reached_stdin_when_a_handshake_is_accepted_declined_or_broken`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fed {
    /// Nothing was asked to be written, or nothing of a secret was.
    Nothing,
    /// The handshake's acknowledgement was not the expected one (this line, possibly empty):
    /// the secret was not written.
    Declined(Vec<u8>),
    /// Everything was written and stdin closed.
    Written,
    /// The child closed its stdin while it was being written: some of it may have been read.
    Broken,
}

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
    /// What its stdin gets.
    pub stdin: Stdin<'a>,
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
    /// What reached its stdin.
    pub fed: Fed,
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
        .stdin(match call.stdin {
            Stdin::Null => Stdio::null(),
            Stdin::Bytes(_) | Stdin::Secret(_) | Stdin::Handshake(_) => Stdio::piped(),
        })
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
    let outcome = tokio::time::timeout(
        call.deadline,
        finish(&mut child, call.output_limit, call.stdin),
    )
    .await;
    match outcome {
        Ok(Ok(mut exited)) => {
            // Reaped: its id may be reused from here on, so the group is left alone.
            group.disarm();
            let secret = match call.stdin {
                Stdin::Secret(secret) | Stdin::Handshake(Handshake { secret, .. }) => {
                    Some(secret.expose().as_str())
                }
                Stdin::Null | Stdin::Bytes(_) => None,
            };
            exited.stderr = redact_with(exited.stderr.as_bytes(), call.redact, secret);
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

/// Writes stdin and reads both streams to their end, then reaps the child.
async fn finish(child: &mut Child, limit: usize, input: Stdin<'_>) -> io::Result<Exited> {
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (talked, err) = tokio::join!(
        converse(stdin, stdout, input, limit),
        bounded(stderr, STDERR_LIMIT)
    );
    let (fed, (stdout, truncated)) = talked?;
    let (stderr, _) = err?;
    let status = child.wait().await?;
    Ok(Exited {
        code: status.code(),
        signal: status.signal(),
        stdout,
        truncated,
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        fed,
    })
}

/// Feeds stdin as `input` says while reading stdout up to `limit`.
async fn converse(
    pipe: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    input: Stdin<'_>,
    limit: usize,
) -> io::Result<(Fed, (Vec<u8>, bool))> {
    let Stdin::Handshake(handshake) = input else {
        let (fed, out) = tokio::join!(feed(pipe, input), bounded(stdout, limit));
        return Ok((fed?, out?));
    };
    let Some(stdout) = stdout else {
        return Ok((Fed::Nothing, (Vec::new(), false)));
    };
    let mut reader = BufReader::new(stdout);
    let mut pipe = pipe;
    let greeted = match pipe.as_mut() {
        Some(p) => write(p, handshake.hello).await?,
        None => false,
    };
    let mut ack = Vec::new();
    (&mut reader)
        .take(u64::try_from(ACK_LIMIT).unwrap_or(u64::MAX))
        .read_until(b'\n', &mut ack)
        .await?;
    let fed = match pipe.take() {
        Some(mut p) if greeted && ack == handshake.expect => {
            if write(&mut p, handshake.secret.expose().as_bytes()).await? && close(&mut p).await? {
                Fed::Written
            } else {
                Fed::Broken
            }
        }
        _ => Fed::Declined(ack.clone()),
    };
    let (rest, truncated) = bounded(Some(reader), limit.saturating_sub(ack.len())).await?;
    ack.extend(rest);
    Ok((fed, (ack, truncated)))
}

/// Writes `bytes`; `false` when the child closed its stdin first (a broken pipe).
async fn write(pipe: &mut ChildStdin, bytes: &[u8]) -> io::Result<bool> {
    match pipe.write_all(bytes).await {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == ErrorKind::BrokenPipe => Ok(false),
        Err(err) => Err(err),
    }
}

/// Closes stdin; `false` on a broken pipe.
async fn close(pipe: &mut ChildStdin) -> io::Result<bool> {
    match pipe.shutdown().await {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == ErrorKind::BrokenPipe => Ok(false),
        Err(err) => Err(err),
    }
}

/// Writes `input` to the child's stdin and closes it. A child that closed its stdin before
/// reading it all is [`Fed::Broken`], not an error: its exit and the caller decide.
async fn feed(pipe: Option<ChildStdin>, input: Stdin<'_>) -> io::Result<Fed> {
    let bytes = match input {
        Stdin::Null | Stdin::Handshake(_) => return Ok(Fed::Nothing),
        Stdin::Bytes(bytes) => bytes,
        Stdin::Secret(secret) => secret.expose().as_bytes(),
    };
    let Some(mut pipe) = pipe else {
        return Ok(Fed::Nothing);
    };
    if write(&mut pipe, bytes).await? && close(&mut pipe).await? {
        Ok(Fed::Written)
    } else {
        Ok(Fed::Broken)
    }
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
    redact_with(text, secrets, None)
}

/// [`redact`], masking `also` too (a secret stdin, never copied into the list).
fn redact_with(text: &[u8], secrets: &[String], also: Option<&str>) -> String {
    let mut out = String::from_utf8_lossy(text).into_owned();
    for secret in secrets
        .iter()
        .map(String::as_str)
        .chain(also)
        .filter(|s| !s.is_empty())
    {
        out = out.replace(secret, REDACTED);
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
