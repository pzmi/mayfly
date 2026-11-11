//! `mayfly-fake-tart` (test plane): a `tart`-compatible CLI over a state directory, with fault
//! knobs, for the Tart adapter's contract suite, the end-to-end tests and the fault lab.
//!
//! The binary is `tart` (`src/bin/tart.rs`); the whole behaviour is [`run`], so it is testable
//! in-process. The fake imports no workspace crate (D11): it is an independent oracle for what
//! the adapter assumes about Tart, and it is wrong the moment it disagrees with Tart's documented
//! or recorded behaviour; cited sources are in `commands` and `fail`.
//!
//! # Surface
//!
//! `--version`, `clone <src> <name> [--overwrite]`, `set <name> [--random-mac] [--memory N] ...`,
//! `run <name> [--no-graphics ...]`,
//! `ip <name> [--wait N]`, `list [--format json] [--source local|oci] [-q]`, `get <name> [--format
//! json]`, `stop <name> [-t N]`, `delete <name>...`, `exec [-i] <name> [--] <argv>`, plus two
//! seeding commands real Tart also has: `create <name>` (an empty local VM, the base to clone) and
//! `pull <ref>` (an OCI-cache entry, source `OCI`; never reaches a network: with a registry the lab
//! seeded under `TART_HOME/registry/` it serves only the references it holds and writes their
//! manifests into the OCI cache, see `home`). Flags real Tart does not have are refused with exit
//! 64.
//!
//! # State and safety
//!
//! `TART_HOME` is required and must be absolute; a directory with a `vms`, `cache` or `tmp` entry
//! and no fake marker is refused, so the fake cannot be pointed at a real Tart home (exit 99, the
//! fake's own code). See `home` for the layout and `faults` for the fault file. Every invocation
//! and fault hit is appended to `invocations.jsonl` (see `log`), and so is every catchable signal
//! a foreground `run` receives (see `signals`): mayfly never signals `tart run` (ADR 0037), and
//! the log is how a test proves it.
//!
//! # Process model of `run` (owner decision OD1, domain-model §9.2)
//!
//! Real `tart run` is the VM's process: it blocks until the VM stops, and killing it stops the VM.
//! The fake keeps that shape by default (`run_mode = "foreground"`): `run` records its own pid in
//! the VM's record and blocks until `stop` or `delete` removes the record's `run` entry; the VM
//! counts as running while that pid lives, so SIGKILL of the `tart run` process is a VM that is
//! gone. An adapter that spawns `tart run` in its own session (the recommended detached model)
//! therefore keeps the VM alive across a daemon crash with no special support. For tests that
//! must not leave processes behind, `run_mode = "detached"` in `faults.toml` makes `run` mark the
//! VM running in the state directory and exit at once; `stop` ends it. In foreground mode a
//! fault that makes `run` exit (`fail_after_effect`, `crash_after_effect`) also ends the VM, as it
//! would in real Tart; use detached mode to model "the command failed but the VM runs".
//!
//! Pinned by: `tests/cli/commands.rs::should_hold_a_foreground_vm_until_stopped_when_run_stays_alive`,
//! `tests/cli/commands.rs::should_lose_a_foreground_vm_when_the_run_process_is_killed`,
//! `tests/cli/commands.rs::should_keep_a_detached_vm_running_when_run_has_returned`,
//! `tests/cli/commands.rs::should_refuse_to_run_when_tart_home_is_not_set`, and `tests/cli/faults.rs`.

mod cli;
mod commands;
mod fail;
mod faults;
mod guest;
mod home;
mod log;
mod signals;
mod timefmt;
mod vm;
mod wait;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::time::Duration;

use cli::{Command, parse_command};
use commands::{Ctx, Done, execute, finish_wait};
use fail::Fail;
use faults::{Action, Corrupt, FaultFile, Rule};
use home::Home;
use log::{Event, Record};
use timefmt::now_ms;
use wait::pause;

/// What the process must do when [`run`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    /// Exit with this code.
    Exit(i32),
    /// Block until killed (a `hang` fault); the caller parks the thread for ever.
    Hang,
    /// Die by SIGABRT (a `crash_after_effect` fault); the caller calls `std::process::abort`.
    Abort,
}

/// One invocation of the fake: its arguments and the world around it.
pub struct Invocation<'a> {
    /// The arguments after the program name.
    pub argv: Vec<String>,
    /// The whole environment; `TART_HOME` names the state directory.
    pub env: BTreeMap<String, String>,
    /// The invoking process's id (a foreground `run` ties the VM's life to it).
    pub pid: u32,
    /// The standard input, read only by `exec -i`.
    pub stdin: &'a mut dyn Read,
    /// The standard output.
    pub stdout: &'a mut dyn Write,
    /// The standard error.
    pub stderr: &'a mut dyn Write,
    /// Whether a foreground `run` records the catchable signals it receives (`signal` log
    /// records). Only the binary sets it: a handler is process-wide, and an in-process caller's
    /// signals are its own.
    pub record_signals: bool,
}

/// Runs one invocation to its end.
pub fn run(invocation: Invocation<'_>) -> Termination {
    let Invocation {
        argv,
        env,
        pid,
        stdin,
        stdout,
        stderr,
        record_signals,
    } = invocation;
    let home = match Home::open(env.get("TART_HOME").map(String::as_str)) {
        Ok(home) => home,
        Err(fail) => return emit_fail(stderr, &fail),
    };
    let streams = Streams {
        stdin,
        stdout,
        stderr,
    };
    match drive(&home, argv, env, pid, streams, record_signals) {
        Ok(termination) => termination,
        Err(fail) => emit_fail(stderr, &fail),
    }
}

fn emit_fail(stderr: &mut dyn Write, fail: &Fail) -> Termination {
    drop(writeln!(stderr, "{}", fail.message));
    Termination::Exit(fail.code)
}

/// The fault selected for this invocation.
struct Picked {
    file: FaultFile,
    index: Option<usize>,
}

impl Picked {
    fn rule(&self) -> Option<&Rule> {
        self.index.map(|i| &self.file.rules[i])
    }
}

/// Allocates the invocation id and selects its fault under one lock.
fn allocate(
    home: &Home,
    parsed: &Result<Command, Fail>,
) -> Result<(u64, Result<Picked, Fail>), Fail> {
    let _lock = home.lock()?;
    let mut state = home.read_state()?;
    let inv = state.next_inv;
    state.next_inv += 1;
    let picked = match parsed {
        Err(_) => Ok(Picked {
            file: FaultFile::default(),
            index: None,
        }),
        Ok(command) => home.read_faults().map(|file| {
            let index = file.select(&mut state.rules, command);
            Picked { file, index }
        }),
    };
    home.write_state(&state)?;
    Ok((inv, picked))
}

/// The invocation's standard streams.
struct Streams<'a> {
    stdin: &'a mut dyn Read,
    stdout: &'a mut dyn Write,
    stderr: &'a mut dyn Write,
}

fn drive(
    home: &Home,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    pid: u32,
    streams: Streams<'_>,
    record_signals: bool,
) -> Result<Termination, Fail> {
    let Streams {
        stdin,
        stdout,
        stderr,
    } = streams;
    let parsed = parse_command(&argv);
    let (inv, picked) = allocate(home, &parsed)?;
    let record = |event: Event| home.append_log(&Record::new(inv, now_ms(), event));
    record(Event::Invoke {
        pid,
        argv,
        env: loggable_env(env),
    })?;
    let fault = |rule: &Rule, id: String, phase: &str| {
        record(Event::Fault {
            rule: id,
            action: rule.action.as_str().to_owned(),
            phase: phase.to_owned(),
        })
    };
    let exit =
        |code: i32, stderr: &mut dyn Write, message: Option<&str>| -> Result<Termination, Fail> {
            if let Some(text) = message {
                drop(writeln!(stderr, "{text}"));
            }
            record(Event::Exit { code })?;
            Ok(Termination::Exit(code))
        };

    let command = match parsed {
        Ok(command) => command,
        Err(fail) => return exit(fail.code, stderr, Some(&fail.message)),
    };
    let picked = match picked {
        Ok(picked) => picked,
        Err(fail) => return exit(fail.code, stderr, Some(&fail.message)),
    };
    let rule = picked.rule();
    let rule_id = picked
        .index
        .map(|i| rule.map(|r| r.id_at(i)).unwrap_or_default())
        .unwrap_or_default();
    let action = rule.map_or(Action::None, |r| r.action);
    // Watching starts before any fault acts and before the VM is marked running, so a `run`
    // that hangs, and one a caller sees running, records every signal it could be sent. It ends
    // when this invocation does; a hung one keeps it for as long as it hangs.
    let watch = (record_signals && matches!(command, Command::Run { .. }))
        .then(|| signals::watch(home, inv))
        .flatten();
    let hang = |watch: Option<signals::Watch>| {
        std::mem::forget(watch);
        Ok(Termination::Hang)
    };

    if let Some(r) = rule.filter(|r| r.delay_ms > 0) {
        record(Event::Fault {
            rule: rule_id.clone(),
            action: "delay".to_owned(),
            phase: "before_effect".to_owned(),
        })?;
        pause(Duration::from_millis(r.delay_ms));
    }
    if let Some(r) = rule {
        let injected = || {
            r.stderr
                .clone()
                .unwrap_or_else(|| "fake-tart: injected failure".to_owned())
        };
        match action {
            Action::Hang => {
                fault(r, rule_id.clone(), "before_effect")?;
                drop(writeln!(
                    stderr,
                    "fake-tart: hang fault '{rule_id}' is active"
                ));
                return hang(watch);
            }
            Action::FailBeforeEffect => {
                fault(r, rule_id, "before_effect")?;
                return exit(r.exit_code.unwrap_or(1), stderr, Some(&injected()));
            }
            _ => {}
        }
    }

    let mut ctx = Ctx {
        home,
        inv,
        pid,
        run_mode: picked.file.run_mode,
        rule,
        stdin,
        stdout: &mut *stdout,
        stderr: &mut *stderr,
        held: None,
    };
    let executed = execute(&command, &mut ctx);
    // the run lock, if this is a foreground `run`, lives as long as this invocation
    let held = ctx.held.take();
    let done = match executed {
        Ok(done) => done,
        Err(fail) => return exit(fail.code, stderr, Some(&fail.message)),
    };

    if let Some(r) = rule {
        match action {
            Action::HangAfterEffect => {
                // a hung `run` still holds its VM's run lock, as a hung `tart run` would
                std::mem::forget(held);
                fault(r, rule_id.clone(), "after_effect")?;
                drop(writeln!(
                    stderr,
                    "fake-tart: hang fault '{rule_id}' is active"
                ));
                return hang(watch);
            }
            Action::CrashAfterEffect => {
                fault(r, rule_id, "after_effect")?;
                return Ok(Termination::Abort);
            }
            Action::FailAfterEffect => {
                fault(r, rule_id, "after_effect")?;
                let message = r
                    .stderr
                    .clone()
                    .unwrap_or_else(|| "fake-tart: injected failure".to_owned());
                return exit(r.exit_code.unwrap_or(1), stderr, Some(&message));
            }
            _ => {}
        }
    }

    let Done {
        code,
        stdout: mut text,
        stderr: progress,
        wait,
    } = done;
    if !progress.is_empty() {
        drop(stderr.write_all(progress.as_bytes()));
        drop(stderr.flush());
    }
    if let Some(wait) = wait
        && let Err(fail) = finish_wait(home, &wait)
    {
        return exit(fail.code, stderr, Some(&fail.message));
    }
    if let Some(r) = rule.filter(|r| r.action == Action::CorruptOutput) {
        fault(r, rule_id, "after_effect")?;
        text = corrupt(&text, r.corrupt);
    }
    drop(stdout.write_all(text.as_bytes()));
    drop(stdout.flush());
    let exited = exit(code, stderr, None);
    drop(held);
    exited
}

/// Variables whose values the log keeps: the state directory and the fake's own controls.
fn value_is_logged(name: &str) -> bool {
    name == "TART_HOME" || name.starts_with("MAYFLY_FAKE_")
}

/// The environment as the log records it: every name, a value only where [`value_is_logged`].
fn loggable_env(env: BTreeMap<String, String>) -> BTreeMap<String, Option<String>> {
    env.into_iter()
        .map(|(name, value)| {
            let kept = value_is_logged(&name).then_some(value);
            (name, kept)
        })
        .collect()
}

/// Damages `text` the way a failing or garbled Tart would.
fn corrupt(text: &str, how: Corrupt) -> String {
    match how {
        Corrupt::Empty => String::new(),
        Corrupt::Garbage => "\u{fffd}\u{0}<<fake-tart: corrupt output>>\u{fffd}".to_owned(),
        Corrupt::Truncate => {
            let mut cut = text.len() / 2;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text[..cut].to_owned()
        }
    }
}
