//! A foreground `run` records the catchable signals it receives.
//!
//! mayfly never signals `tart run` (ADR 0037): it stops a VM only with `tart stop`. The fake makes
//! that checkable: from before a `run` marks its VM running until the invocation ends, a watcher
//! thread appends a `signal` record (`SIGTERM`, `SIGINT`, …) to the invocation log for each
//! signal it receives, then lets the signal do what it does by default (the process dies by it
//! and the VM with it, as a killed `tart run` takes its VM). SIGKILL and SIGSTOP cannot be
//! caught; a run killed by one shows as a `run` with no `exit` record and a dead pid.
//!
//! Pinned by `tests/cli/commands.rs::should_record_a_signal_when_a_foreground_run_receives_one`.

use std::thread::JoinHandle;

use signal_hook::consts::signal::{SIGALRM, SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2};
use signal_hook::iterator::{Handle, Signals};

use crate::home::Home;
use crate::log::{Event, Record};
use crate::timefmt::now_ms;

/// The signals recorded: every catchable one a supervisor might send to stop a process.
const WATCHED: &[i32] = &[SIGTERM, SIGINT, SIGHUP, SIGQUIT, SIGUSR1, SIGUSR2, SIGALRM];

/// The watcher of one `run`; dropping it stops and joins the thread.
pub(crate) struct Watch {
    handle: Handle,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            drop(thread.join());
        }
    }
}

fn name(signal: i32) -> String {
    match signal {
        SIGTERM => "SIGTERM".to_owned(),
        SIGINT => "SIGINT".to_owned(),
        SIGHUP => "SIGHUP".to_owned(),
        SIGQUIT => "SIGQUIT".to_owned(),
        SIGUSR1 => "SIGUSR1".to_owned(),
        SIGUSR2 => "SIGUSR2".to_owned(),
        SIGALRM => "SIGALRM".to_owned(),
        other => format!("signal {other}"),
    }
}

/// Starts recording signals for invocation `inv`; `None` when the handlers cannot be installed
/// (the run then waits unwatched, as before).
pub(crate) fn watch(home: &Home, inv: u64) -> Option<Watch> {
    let mut signals = Signals::new(WATCHED).ok()?;
    let handle = signals.handle();
    let dir = home.dir.clone();
    let thread = std::thread::spawn(move || {
        let home = Home { dir };
        for signal in signals.forever() {
            let event = Event::Signal {
                signal: name(signal),
            };
            drop(home.append_log(&Record::new(inv, now_ms(), event)));
            drop(signal_hook::low_level::emulate_default_handler(signal));
        }
    });
    Some(Watch {
        handle,
        thread: Some(thread),
    })
}
