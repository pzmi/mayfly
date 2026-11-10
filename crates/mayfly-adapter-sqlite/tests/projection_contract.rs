//! Integration level: the projection contract (PC-01..PC-06) against a real SQLite file in a
//! tempdir (`doc/event-store.md` §7.1): fresh, reopened between steps, with two writer
//! connections, and crashed (the process dies holding the file, which is then reopened). A
//! lower level cannot fail for these reasons: the generations, the meta row and the applied
//! revisions are rows of the file, read and written in the unit's transaction.

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_adapter_sqlite::{HookAction, SqliteJournal, SqliteOptions, StoreHooks};
use mayfly_domain::Millis;
use mayfly_testkit::block_on;
use mayfly_testkit::contracts::journal::InjectedFault;
use mayfly_testkit::contracts::projections::{
    ProjectionContractReport, ProjectionFactory, check_projection_contract,
};
use tempfile::TempDir;

/// The fault armed for the next append, shared by every handle a factory opens.
#[derive(Debug, Default)]
struct Faults {
    armed: Mutex<Option<InjectedFault>>,
    crashing: AtomicBool,
}

#[derive(Debug, Clone, Default)]
struct Hooks(Arc<Faults>);

impl StoreHooks for Hooks {
    fn before_commit(&self) -> HookAction {
        let mut armed = self.0.armed.lock().unwrap_or_else(PoisonError::into_inner);
        if *armed == Some(InjectedFault::StorageBeforeCommit) {
            *armed = None;
            return HookAction::Fail;
        }
        HookAction::Proceed
    }

    fn checkpoint_on_close(&self) -> bool {
        !self.0.crashing.load(Ordering::SeqCst)
    }
}

/// What `reopen` does in a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reopen {
    Keep,
    Close,
    Crash,
}

#[derive(Debug, Clone, Copy)]
struct Mode {
    writers: usize,
    reopen: Reopen,
}

type Store = SqliteJournal<Hooks>;

struct SqliteFactory {
    dir: TempDir,
    files: AtomicUsize,
    hooks: Hooks,
    mode: Mode,
}

impl SqliteFactory {
    fn new(mode: Mode) -> Self {
        Self {
            dir: tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}")),
            files: AtomicUsize::new(0),
            hooks: Hooks::default(),
            mode,
        }
    }

    fn open(&self, path: &Path) -> Store {
        let options = SqliteOptions {
            writers: NonZeroUsize::new(self.mode.writers).unwrap_or(NonZeroUsize::MIN),
            ..SqliteOptions::new(Millis::new(1))
        };
        SqliteJournal::open_with_hooks(path, options, self.hooks.clone())
            .unwrap_or_else(|e| panic!("open {}: {e}", path.display()))
    }
}

impl ProjectionFactory for SqliteFactory {
    type Store = Store;

    async fn fresh(&self) -> Store {
        let n = self.files.fetch_add(1, Ordering::SeqCst);
        self.open(&self.dir.path().join(format!("projections-{n}.sqlite")))
    }

    async fn reopen(&self, store: Store) -> Store {
        let path = store.path().to_owned();
        match self.mode.reopen {
            Reopen::Keep => store,
            Reopen::Close => {
                drop(store);
                self.open(&path)
            }
            Reopen::Crash => {
                self.hooks.0.crashing.store(true, Ordering::SeqCst);
                drop(store);
                self.hooks.0.crashing.store(false, Ordering::SeqCst);
                self.open(&path)
            }
        }
    }

    async fn inject(&self, _store: &Store, fault: InjectedFault) -> bool {
        if fault != InjectedFault::StorageBeforeCommit {
            return false;
        }
        *self
            .hooks
            .0
            .armed
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(fault);
        true
    }
}

fn contract(writers: usize, reopen: Reopen) -> ProjectionContractReport {
    block_on(check_projection_contract(&SqliteFactory::new(Mode {
        writers,
        reopen,
    })))
}

#[test]
fn should_pass_projection_contract_when_sqlite_runs_fresh() {
    // given
    let (writers, reopen) = (1, Reopen::Keep);

    // when
    let report = contract(writers, reopen);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_pass_projection_contract_when_sqlite_is_reopened_between_steps() {
    // given
    let (writers, reopen) = (1, Reopen::Close);

    // when
    let report = contract(writers, reopen);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_pass_projection_contract_when_sqlite_has_two_writer_connections() {
    // given
    let (writers, reopen) = (2, Reopen::Keep);

    // when
    let report = contract(writers, reopen);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_pass_projection_contract_when_sqlite_crashes_and_is_reopened() {
    // given
    let (writers, reopen) = (1, Reopen::Crash);

    // when
    let report = contract(writers, reopen);

    // then
    assert!(report.is_green(), "{report}");
}
