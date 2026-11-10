//! Starting a VM's shim (ADR 0037): the one child the adapter does not wait for.
//!
//! The shim is spawned in the daemon's own process group, not a new one, because it leaves the
//! daemon's session itself (`setsid` fails for a group leader); from then on no signal to the
//! daemon's group reaches it or its `tart run`. It gets a cleared environment plus the allowlist
//! (ADR 0058), a null stdin and stdout, and its stderr in `shim.log`, a file never a pipe. It is
//! not killed when its handle is dropped: it outlives the daemon by design. The adapter keeps the
//! handle only to reap the shim when it exits while the daemon lives ([`Shims::reap`]).

use std::ffi::OsString;
use std::fs::{DirBuilder, File};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::config::ShimProgram;
use crate::process::vm_shim::ShimArgs;
use crate::shim::SHIM_LOG;

/// One shim this process started.
#[derive(Debug)]
struct Entry {
    run_id: String,
    child: Child,
    /// Its exit status, once waited for: kept for the `start` that may still read it.
    status: Option<ExitStatus>,
    /// Whether the `start` that spawned it has returned, so nobody reads `status` any more.
    returned: bool,
}

impl Entry {
    /// Waits for the shim without blocking; once it has exited it is reaped (no zombie) and its
    /// status kept.
    fn poll(&mut self) -> Option<ExitStatus> {
        if self.status.is_none() {
            self.status = self.child.try_wait().ok().flatten();
        }
        self.status
    }
}

/// The shims this process started and has not forgotten, by run id. Every entry is waited for
/// whenever the registry is reaped, so a shim that exits is never left a zombie; an entry is
/// forgotten once its shim has exited and its `start` has returned. Pinned by
/// `tests/tart/shim.rs::should_reap_and_forget_the_shim_when_its_vm_has_stopped`.
#[derive(Debug, Default)]
pub struct Shims {
    entries: Mutex<Vec<Entry>>,
}

impl Shims {
    fn lock(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Spawns the shim for `args.vm` in `args.dir` (created `0700` if needed) with exactly
    /// `environment`, registered under `args.run_id`.
    ///
    /// # Errors
    /// The directory, the log file or the spawn failed: nothing was started.
    pub fn spawn(
        &self,
        program: &ShimProgram,
        args: &ShimArgs,
        environment: &[(OsString, OsString)],
    ) -> io::Result<()> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&args.dir)?;
        let log = File::create(args.dir.join(SHIM_LOG))?;
        let child = Command::new(&program.program)
            .env_clear()
            .envs(environment.iter().map(|(name, value)| (name, value)))
            .args(&program.args)
            .args(args.to_args())
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()?;
        self.reap();
        self.lock().push(Entry {
            run_id: args.run_id.clone(),
            child,
            status: None,
            returned: false,
        });
        Ok(())
    }

    /// The exit status of the shim of `run_id`, once it has exited; `None` while it runs or
    /// when this process did not start it.
    pub fn exited(&self, run_id: &str) -> Option<ExitStatus> {
        self.lock()
            .iter_mut()
            .find(|entry| entry.run_id == run_id)
            .and_then(Entry::poll)
    }

    /// The `start` of `run_id` has returned: its shim's status is no longer needed.
    pub fn returned(&self, run_id: &str) {
        if let Some(entry) = self.lock().iter_mut().find(|e| e.run_id == run_id) {
            entry.returned = true;
        }
        self.reap();
    }

    /// Waits for every shim without blocking, reaping each that has exited, and forgets those
    /// whose `start` has returned.
    pub fn reap(&self) {
        self.lock().retain_mut(|entry| {
            let exited = entry.poll().is_some();
            !(exited && entry.returned)
        });
    }

    /// How many shims this process still holds an entry for.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether it holds none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
