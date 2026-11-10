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

/// The shims this process started and has not reaped, with their VMs, oldest first.
#[derive(Debug, Default)]
pub struct Shims {
    children: Mutex<Vec<(String, Child)>>,
}

impl Shims {
    fn lock(&self) -> MutexGuard<'_, Vec<(String, Child)>> {
        self.children.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Spawns the shim for `args.vm` in `args.dir` (created `0700` if needed) with exactly
    /// `environment`.
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
        self.lock().push((args.vm.clone(), child));
        Ok(())
    }

    /// The exit status of `vm`'s shim, once it has exited; `None` while it runs or when this
    /// process did not start it.
    pub fn exited(&self, vm: &str) -> Option<ExitStatus> {
        self.lock()
            .iter_mut()
            .rev()
            .find(|(name, _)| name == vm)
            .and_then(|(_, child)| child.try_wait().ok().flatten())
    }

    /// Reaps every shim that has exited and forgets it, but the newest of each VM, whose status
    /// a `start` may still be waiting to read ([`Shims::exited`]).
    pub fn reap(&self) {
        let mut children = self.lock();
        let newest: Vec<bool> = (0..children.len())
            .map(|i| !children[i + 1..].iter().any(|(vm, _)| *vm == children[i].0))
            .collect();
        let mut index = 0;
        children.retain_mut(|(_, child)| {
            let keep = newest[index] || matches!(child.try_wait(), Ok(None));
            index += 1;
            keep
        });
    }
}
