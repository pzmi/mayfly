//! The emulated in-guest launcher: what `exec` of [`LAUNCHER`] does inside a fake guest.
//!
//! mayfly's images carry a launcher at a fixed path (ADR 0038, ADR 0082); the fake is the
//! oracle of its contract, as it is of Tart's:
//!
//! ```text
//! exec -i <vm> <launcher>              launch: a handshake, then the JIT config. stdin's first
//!                                      line is `<nonce> <expected mac>`; the launcher answers one
//!                                      line `ok <nonce> <its mac>` and reads the config to end of
//!                                      file, then starts the runner (exit 0; an empty config,
//!                                      stdin closed after the answer, exits 4 and starts nothing);
//!                                      a guest whose MAC is not the expected one answers
//!                                      `mismatch <nonce> <its mac>` and exits 5, one that already
//!                                      launched a runner `launched <nonce> <its mac>`, exit 3
//! exec -i <vm> <launcher> verify       stdin is the nonce (32 hex digits); stdout one JSON line
//!                                      {"v":1,"nonce":..,"os":"linux"|"macos","uptime_ms":..,"mac":..}
//! exec <vm> <launcher> status          stdout {"v":1,"state":"none"|"running"} or
//!                                      {"v":1,"state":"exited","code":N}: the launcher's status file
//! exec <vm> <launcher> diagnostics     stdout the runner's log (`$TART_HOME/guest-logs/<vm>.log`)
//! ```
//!
//! The answering guest is the VM's own, or the rule's `route_to` (a misrouted exec, F39): its
//! MAC, uptime, status and log are that VM's. A launch keeps only stdin's length and SHA-256
//! (the `stdin` log event and the VM's `guest_launches`), never the config. The runner's job
//! is the launch rule's `job_ms` and `job_exit_code`; with no rule it runs until the VM stops.
//! A rule's `echo_stdin` makes the launcher print the config it read on stderr (a failing
//! launcher that leaks what it was given, for the transport's redaction).
//! The status lives in the guest's record, so a restarted daemon reads it as it was. Pinned by
//! `tests/cli/guest.rs`.

use std::io::Read;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::commands::{Ctx, Done, digest_stdin, local_vm};
use crate::fail::Fail;
use crate::log::{Event, Record};
use crate::timefmt::now_ms;
use crate::vm::{Launch, MAX_LAUNCHES, Runner, hex};

/// The launcher's fixed path in every mayfly image.
pub(crate) const LAUNCHER: &str = "/usr/local/libexec/mayfly/launcher";

/// The exit code of a launch that found a runner already launched.
pub(crate) const ALREADY_LAUNCHED: i32 = 3;

/// The most bytes a nonce or handshake line read from stdin may have.
const NONCE_LIMIT: u64 = 128;

/// The exit code of a launch whose expected MAC is not this guest's.
pub(crate) const MISMATCH: i32 = 5;

/// The exit code of a launch whose stdin ended before any config.
pub(crate) const NO_CONFIG: i32 = 4;

#[derive(Serialize)]
struct Claims {
    v: u32,
    nonce: String,
    os: &'static str,
    uptime_ms: u64,
    mac: String,
}

#[derive(Serialize)]
struct Status {
    v: u32,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<i32>,
}

fn json<T: Serialize>(value: &T) -> Result<String, Fail> {
    serde_json::to_string(value)
        .map(|line| line + "\n")
        .map_err(|e| Fail::fake(&format!("cannot encode the launcher's answer: {e}")))
}

fn status(runner: Option<&Runner>, now: u64) -> Status {
    let (state, code) = match runner {
        None => ("none", None),
        Some(Runner {
            job: Some((ms, code)),
            started_at_ms,
        }) if now >= started_at_ms.saturating_add(*ms) => ("exited", Some(*code)),
        Some(_) => ("running", None),
    };
    Status { v: 1, state, code }
}

/// Runs the launcher with `args` inside the guest of `guest`.
pub(crate) fn launcher(
    ctx: &mut Ctx<'_>,
    guest: &str,
    interactive: bool,
    args: &[String],
) -> Result<Done, Fail> {
    match args.first().map(String::as_str) {
        None if interactive => launch(ctx, guest),
        Some("verify") if interactive && args.len() == 1 => verify(ctx, guest),
        Some("status") if args.len() == 1 => {
            let vm = local_vm(ctx.home, guest)?;
            Ok(Done::out(json(&status(vm.runner.as_ref(), now_ms()))?))
        }
        Some("diagnostics") if args.len() == 1 => Ok(Done::out(ctx.home.guest_log(guest)?)),
        _ => Ok(Done {
            code: 64,
            stdout: String::new(),
            stderr: "launcher: usage: launcher [verify|status|diagnostics]\n".to_owned(),
            wait: None,
        }),
    }
}

/// Reads one line of stdin, byte by byte so nothing after it is consumed.
fn line(ctx: &mut Ctx<'_>) -> Result<String, Fail> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    while u64::try_from(bytes.len()).unwrap_or(u64::MAX) < NONCE_LIMIT {
        let n = ctx
            .stdin
            .read(&mut byte)
            .map_err(|e| Fail::fake(&format!("cannot read stdin: {e}")))?;
        if n == 0 || byte[0] == b'\n' {
            break;
        }
        bytes.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn answer(ctx: &mut Ctx<'_>, text: &str) -> Result<(), Fail> {
    writeln!(ctx.stdout, "{text}")
        .and_then(|()| ctx.stdout.flush())
        .map_err(|e| Fail::fake(&format!("cannot write stdout: {e}")))
}

fn refused(code: i32, message: &str) -> Done {
    Done {
        code,
        stdout: String::new(),
        stderr: format!("launcher: {message}\n"),
        wait: None,
    }
}

fn launch(ctx: &mut Ctx<'_>, guest: &str) -> Result<Done, Fail> {
    let hello = line(ctx)?;
    let (nonce, expected) = hello.split_once(' ').unwrap_or((hello.as_str(), ""));
    let vm = local_vm(ctx.home, guest)?;
    if vm.runner.is_some() {
        answer(ctx, &format!("launched {nonce} {}", vm.mac))?;
        return Ok(refused(ALREADY_LAUNCHED, "a runner was already launched"));
    }
    if !expected.eq_ignore_ascii_case(&vm.mac) {
        answer(ctx, &format!("mismatch {nonce} {}", vm.mac))?;
        return Ok(refused(MISMATCH, "this guest is not the expected one"));
    }
    answer(ctx, &format!("ok {nonce} {}", vm.mac.to_ascii_lowercase()))?;
    let echo = ctx.rule.is_some_and(|r| r.echo_stdin);
    let (len, digest) = if echo {
        let mut config = Vec::new();
        ctx.stdin
            .read_to_end(&mut config)
            .map_err(|e| Fail::fake(&format!("cannot read stdin: {e}")))?;
        drop(ctx.stderr.write_all(&config));
        drop(ctx.stderr.flush());
        digest_bytes(ctx, &config)?
    } else {
        digest_stdin(ctx)?
    };
    if len == 0 {
        return Ok(refused(NO_CONFIG, "no config on stdin"));
    }
    let _lock = ctx.home.lock()?;
    let mut vm = local_vm(ctx.home, guest)?;
    vm.guest_launches.push(Launch {
        at_ms: now_ms(),
        argv: vec![LAUNCHER.to_owned()],
        stdin_len: Some(len),
        stdin_sha256: Some(digest),
    });
    let excess = vm.guest_launches.len().saturating_sub(MAX_LAUNCHES);
    vm.guest_launches.drain(..excess);
    vm.runner = Some(Runner {
        started_at_ms: now_ms(),
        job: ctx.rule.map(|r| (r.job_ms, r.job_exit_code)),
    });
    ctx.home.write_vm(&vm)?;
    ctx.effect("runner_launched", guest)?;
    Ok(Done::out(String::new()))
}

/// Logs `bytes`' length and digest as [`digest_stdin`] does for a stream.
fn digest_bytes(ctx: &Ctx<'_>, bytes: &[u8]) -> Result<(u64, String), Fail> {
    let digest = hex(Sha256::digest(bytes).as_ref());
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let event = Event::Stdin {
        len,
        sha256: digest.clone(),
    };
    ctx.home
        .append_log(&Record::new(ctx.inv, now_ms(), event))?;
    Ok((len, digest))
}

fn verify(ctx: &mut Ctx<'_>, guest: &str) -> Result<Done, Fail> {
    let mut nonce = String::new();
    (&mut ctx.stdin)
        .take(NONCE_LIMIT)
        .read_to_string(&mut nonce)
        .map_err(|e| Fail::fake(&format!("cannot read stdin: {e}")))?;
    let vm = local_vm(ctx.home, guest)?;
    let started = vm.run.as_ref().map_or(now_ms(), |run| run.started_at_ms);
    let claims = Claims {
        v: 1,
        nonce: nonce.trim().to_owned(),
        os: if vm.os == "darwin" { "macos" } else { "linux" },
        uptime_ms: now_ms().saturating_sub(started),
        mac: vm.mac,
    };
    Ok(Done::out(json(&claims)?))
}
