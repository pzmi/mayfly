//! Integration level: the delivery contract (DC-01..DC-25) against a real SQLite file in a
//! tempdir, in the five modes the journal contract runs (`doc/event-store.md` §7.1): fresh,
//! reopened between steps, two writer connections, two handles on one file, and crash-injected.
//! A lower level cannot fail for these reasons: the marks, leases and fences are rows of the
//! file, and a second handle shares nothing with the first but the file.
//!
//! The store clock is a `FakeClock` the factory shares with every ledger it opens, so a lease
//! expires only when a case moves it; no test sleeps.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_adapter_sqlite::{HookAction, SqliteJournal, SqliteLedger, SqliteOptions, StoreHooks};
use mayfly_domain::Millis;
use mayfly_testkit::block_on;
use mayfly_testkit::clock::FakeClock;
use mayfly_testkit::contracts::delivery::{
    ClockHand, DeliveryContractReport, DeliveryFactory, DeliveryStore, check_delivery_contract,
};
use mayfly_testkit::contracts::journal::InjectedFault;
use tempfile::TempDir;

/// The fault armed for the next append, shared by every handle a factory opens.
#[derive(Debug, Default)]
struct Faults {
    armed: Mutex<Option<InjectedFault>>,
    crashing: AtomicBool,
    crashes: AtomicUsize,
}

/// The test hooks: fire the armed fault at its barrier, once.
#[derive(Debug, Clone, Default)]
struct Hooks(Arc<Faults>);

impl Hooks {
    fn take_if(&self, fires: impl Fn(InjectedFault) -> bool) -> Option<InjectedFault> {
        let mut armed = self.0.armed.lock().unwrap_or_else(PoisonError::into_inner);
        armed.filter(|f| fires(*f)).and_then(|_| armed.take())
    }
}

impl StoreHooks for Hooks {
    fn after_statement(&self, statement: u32) -> HookAction {
        match self.take_if(|f| f == InjectedFault::CrashAfterStatement(statement)) {
            Some(_) => {
                self.0.crashes.fetch_add(1, Ordering::SeqCst);
                HookAction::Crash
            }
            None => HookAction::Proceed,
        }
    }

    fn before_commit(&self) -> HookAction {
        match self.take_if(|f| f == InjectedFault::StorageBeforeCommit) {
            Some(_) => HookAction::Fail,
            None => HookAction::Proceed,
        }
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

/// One mode of `doc/event-store.md` §7.1.
#[derive(Debug, Clone, Copy)]
struct Mode {
    writers: usize,
    reopen: Reopen,
    /// Whether `second_handle` opens the file again (as another process) or shares connections.
    open_second: bool,
}

const FRESH: Mode = Mode {
    writers: 1,
    reopen: Reopen::Keep,
    open_second: false,
};
const REOPENED: Mode = Mode {
    writers: 1,
    reopen: Reopen::Close,
    open_second: false,
};
const TWO_CONNECTIONS: Mode = Mode {
    writers: 2,
    reopen: Reopen::Keep,
    open_second: false,
};
const TWO_HANDLES: Mode = Mode {
    writers: 1,
    reopen: Reopen::Close,
    open_second: true,
};
const CRASH_INJECTED: Mode = Mode {
    writers: 1,
    reopen: Reopen::Crash,
    open_second: true,
};

type Journal = SqliteJournal<Hooks>;
type Ledger = SqliteLedger<FakeClock, Hooks>;
type Store = DeliveryStore<Journal, Ledger>;

struct SqliteFactory {
    dir: TempDir,
    files: AtomicUsize,
    hooks: Hooks,
    clock: FakeClock,
    mode: Mode,
}

impl SqliteFactory {
    fn new(mode: Mode) -> Self {
        Self {
            dir: tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}")),
            files: AtomicUsize::new(0),
            hooks: Hooks::default(),
            clock: FakeClock::at(Millis::new(1_000_000)),
            mode,
        }
    }

    fn open(&self, path: &Path) -> Store {
        let options = SqliteOptions {
            writers: NonZeroUsize::new(self.mode.writers).unwrap_or(NonZeroUsize::MIN),
            ..SqliteOptions::new(Millis::new(1))
        };
        let journal = SqliteJournal::open_with_hooks(path, options, self.hooks.clone())
            .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
        let ledger = journal.ledger(self.clock.clone());
        DeliveryStore { journal, ledger }
    }

    fn path_of(store: &Store) -> PathBuf {
        store.journal.path().to_owned()
    }

    fn crash_reopen(&self, store: Store) -> Store {
        let path = Self::path_of(&store);
        self.hooks.0.crashing.store(true, Ordering::SeqCst);
        drop(store);
        self.hooks.0.crashing.store(false, Ordering::SeqCst);
        self.open(&path)
    }
}

impl DeliveryFactory for SqliteFactory {
    type Journal = Journal;
    type Ledger = Ledger;

    async fn fresh(&self) -> Store {
        let n = self.files.fetch_add(1, Ordering::SeqCst);
        self.open(&self.dir.path().join(format!("delivery-{n}.sqlite")))
    }

    async fn reopen(&self, store: Store) -> Store {
        match self.mode.reopen {
            Reopen::Keep => store,
            Reopen::Close => {
                let path = Self::path_of(&store);
                drop(store);
                self.open(&path)
            }
            Reopen::Crash => self.crash_reopen(store),
        }
    }

    async fn second_handle(&self, store: &Store) -> Store {
        if self.mode.open_second {
            self.open(store.journal.path())
        } else {
            store.clone()
        }
    }

    async fn inject(&self, _store: &Store, fault: InjectedFault) -> bool {
        let mut armed = self
            .hooks
            .0
            .armed
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if matches!(*armed, Some(InjectedFault::CrashAfterStatement(_))) {
            // The previous crash point was past the unit's last statement: it never fired.
            *armed = None;
            return false;
        }
        if !matches!(
            fault,
            InjectedFault::StorageBeforeCommit | InjectedFault::CrashAfterStatement(_)
        ) {
            return false;
        }
        *armed = Some(fault);
        true
    }

    async fn crash(&self, store: Store) -> Store {
        self.crash_reopen(store)
    }

    fn clock(&self) -> ClockHand {
        let clock = self.clock.clone();
        Arc::new(move |by| clock.advance(by))
    }
}

fn contract(mode: Mode) -> (DeliveryContractReport, usize) {
    let factory = SqliteFactory::new(mode);
    let report = block_on(check_delivery_contract(&factory));
    (report, factory.hooks.0.crashes.load(Ordering::SeqCst))
}

#[test]
fn should_pass_delivery_contract_when_sqlite_runs_fresh() {
    // given
    let mode = FRESH;

    // when
    let (report, _) = contract(mode);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_pass_delivery_contract_when_sqlite_is_reopened_between_steps() {
    // given
    let mode = REOPENED;

    // when
    let (report, _) = contract(mode);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_pass_delivery_contract_when_sqlite_has_two_writer_connections() {
    // given
    let mode = TWO_CONNECTIONS;

    // when
    let (report, _) = contract(mode);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_pass_delivery_contract_when_sqlite_has_two_handles_on_one_file() {
    // given
    let mode = TWO_HANDLES;

    // when
    let (report, _) = contract(mode);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_pass_delivery_contract_when_sqlite_crashes_after_each_statement() {
    // given
    let mode = CRASH_INJECTED;

    // when
    let (report, crashes) = contract(mode);

    // then
    assert!(report.is_green(), "{report}");
    // DC-03's one-event unit on a new stream with one consumer registered: 2 reads (the head,
    // the id) and 5 writes (the head, the identity, the event, the manifest kind, the mark) and
    // the COMMIT, with the point after BEGIN: 9 crash points, each struck once.
    assert_eq!(crashes, 9);
}
