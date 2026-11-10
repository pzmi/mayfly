//! The `SecretProvider` contract (ver §5) on every adapter: `file:`, `env:`, `exec:` over test
//! scripts, and `keychain:` over a fake `security`; and the deliberately broken
//! `PermissiveFileProvider` and `ShellExecProvider` failing exactly the cases their faults
//! break. Values are synthetic; every source lives in a tempdir.
//!
//! Every test that writes a script or starts a process holds [`SPAWN`]: a script written while
//! another thread of this binary forks could be inherited open for writing by that child, and
//! executing it would then fail with `ETXTBSY`. Holding the lock runs those tests one at a time.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::task::Poll;
use std::time::Duration;

use mayfly_adapter_secrets::{
    DEFAULT_DEADLINE, EnvProvider, ExecProvider, FileProvider, KeychainProvider,
};
use mayfly_control::secrets::{Refusal, SecretError, SecretProvider, secret_from_bytes};
use mayfly_domain::{Secret, SecretRef};
use mayfly_testkit::contracts::secrets::{
    ExecSourceFactory, FileSourceFactory, SecretContractReport, SecretSourceFactory,
    check_exec_contract, check_file_contract, check_secret_contract,
};
use mayfly_testkit_shared::poll::wait_until;
use tempfile::TempDir;

static SPAWN: Mutex<()> = Mutex::new(());

fn spawn_lock() -> MutexGuard<'static, ()> {
    SPAWN.lock().unwrap_or_else(PoisonError::into_inner)
}

fn run<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

/// A new, empty directory under `root` with mode 0700.
fn new_dir(root: &Path, next: &mut u32) -> PathBuf {
    *next += 1;
    let dir = root.join(format!("d{next}"));
    fs::create_dir(&dir).expect("mkdir");
    set_mode(&dir, 0o700);
    dir
}

fn file_reference(path: PathBuf) -> SecretRef {
    SecretRef::File { path }
}

// ---------------------------------------------------------------------------------- file:

/// `file:` sources in a tempdir, for `FileProvider` or a broken provider.
struct FileFactory<P> {
    root: TempDir,
    next: u32,
    provider: P,
    foreign: P,
}

impl<P> FileFactory<P> {
    fn new(make: impl Fn(u32) -> P) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let uid = fs::metadata(root.path()).expect("stat").uid();
        Self {
            root,
            next: 0,
            provider: make(uid),
            foreign: make(uid.wrapping_add(1)),
        }
    }

    fn dir(&mut self) -> PathBuf {
        new_dir(self.root.path(), &mut self.next)
    }
}

impl<P: SecretProvider + Clone> SecretSourceFactory for FileFactory<P> {
    type Provider = P;

    fn provider(&self) -> &P {
        &self.provider
    }

    fn install(&mut self, value: &str) -> SecretRef {
        self.install_file(value, 0o600, 0o700)
    }

    fn rotate(&mut self, reference: &SecretRef, value: &str) {
        let SecretRef::File { path } = reference else {
            panic!("not a file reference");
        };
        fs::write(path, value).expect("rotate");
    }

    fn absent(&mut self) -> SecretRef {
        file_reference(self.dir().join("missing"))
    }
}

impl<P: SecretProvider + Clone> FileSourceFactory for FileFactory<P> {
    fn install_file(&mut self, value: &str, file_mode: u32, dir_mode: u32) -> SecretRef {
        let dir = self.dir();
        let path = dir.join("secret");
        fs::write(&path, value).expect("write");
        set_mode(&path, file_mode);
        set_mode(&dir, dir_mode);
        file_reference(path)
    }

    fn install_symlink(&mut self, value: &str) -> SecretRef {
        let dir = self.dir();
        let real = dir.join("real");
        fs::write(&real, value).expect("write");
        set_mode(&real, 0o600);
        symlink(&real, dir.join("secret")).expect("symlink");
        file_reference(dir.join("secret"))
    }

    fn install_in_symlinked_directory(&mut self, value: &str) -> SecretRef {
        let dir = self.dir();
        fs::write(dir.join("secret"), value).expect("write");
        set_mode(&dir.join("secret"), 0o600);
        let link = self.root.path().join(format!("link{}", self.next));
        symlink(&dir, &link).expect("symlink");
        file_reference(link.join("secret"))
    }

    fn directory(&mut self) -> SecretRef {
        let dir = self.dir();
        fs::create_dir(dir.join("secret")).expect("mkdir");
        set_mode(&dir.join("secret"), 0o700);
        file_reference(dir.join("secret"))
    }

    fn install_under_open_ancestor(&mut self, value: &str) -> SecretRef {
        let open = self.dir();
        let dir = open.join("inner");
        fs::create_dir(&dir).expect("mkdir");
        fs::write(dir.join("secret"), value).expect("write");
        set_mode(&dir.join("secret"), 0o600);
        set_mode(&dir, 0o700);
        set_mode(&open, 0o777);
        file_reference(dir.join("secret"))
    }

    fn install_under_symlinked_ancestor(&mut self, value: &str) -> SecretRef {
        let real = self.dir();
        let dir = real.join("inner");
        fs::create_dir(&dir).expect("mkdir");
        set_mode(&dir, 0o700);
        fs::write(dir.join("secret"), value).expect("write");
        set_mode(&dir.join("secret"), 0o600);
        let link = self.root.path().join(format!("ancestor{}", self.next));
        symlink(&real, &link).expect("symlink");
        file_reference(link.join("inner").join("secret"))
    }

    fn foreign_owner_provider(&self) -> P {
        self.foreign.clone()
    }
}

/// `PermissiveFileProvider` (ver §5): reads any absolute path, following symlinks, whatever its
/// owner, mode or type.
#[derive(Debug, Clone)]
struct PermissiveFileProvider;

impl SecretProvider for PermissiveFileProvider {
    async fn resolve(&self, reference: &SecretRef) -> Result<Secret<String>, SecretError> {
        let SecretRef::File { path } = reference else {
            return Err(SecretError::new(reference, Refusal::Unsupported));
        };
        if !path.is_absolute() {
            return Err(SecretError::new(reference, Refusal::RelativePath));
        }
        match fs::read(path) {
            Ok(bytes) => secret_from_bytes(reference, bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(SecretError::new(reference, Refusal::NotFound))
            }
            Err(err) => Err(SecretError::new(
                reference,
                Refusal::Io { kind: err.kind() },
            )),
        }
    }
}

#[test]
fn should_pass_file_contract_when_provider_is_file_provider() {
    // given
    let mut factory = FileFactory::new(FileProvider::owned_by);

    // when
    let report = run(check_file_contract(&mut factory));

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_fail_permission_and_symlink_cases_when_file_provider_is_permissive() {
    // given
    let mut factory = FileFactory::new(|_| PermissiveFileProvider);

    // when
    let report = run(check_file_contract(&mut factory));

    // then
    assert_eq!(
        report.failed_ids(),
        [
            "FS-01", "FS-02", "FS-03", "FS-04", "FS-05", "FS-07", "FS-09"
        ],
        "{report}"
    );
}

#[test]
fn should_refuse_without_blocking_when_the_path_is_a_fifo() {
    // given
    let _spawning = spawn_lock();
    let root = tempfile::tempdir().expect("tempdir");
    let uid = fs::metadata(root.path()).expect("stat").uid();
    set_mode(root.path(), 0o700);
    let fifo = root.path().join("secret");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .stdin(Stdio::null())
        .status()
        .expect("mkfifo");
    assert!(made.success());
    set_mode(&fifo, 0o600);

    // when
    let got = run(FileProvider::owned_by(uid).resolve(&file_reference(fifo)));

    // then
    assert_eq!(got.err().map(|e| e.refusal), Some(Refusal::NotRegularFile));
}

// ----------------------------------------------------------------------------------- env:

#[derive(Default)]
struct EnvFactory {
    values: BTreeMap<String, Vec<u8>>,
    provider: EnvProvider,
}

impl EnvFactory {
    fn recapture(&mut self) {
        self.provider = EnvProvider::from_values(self.values.clone());
    }
}

impl SecretSourceFactory for EnvFactory {
    type Provider = EnvProvider;

    fn provider(&self) -> &EnvProvider {
        &self.provider
    }

    fn install(&mut self, value: &str) -> SecretRef {
        let name = format!("SYNTHETIC_SECRET_{}", self.values.len());
        self.values.insert(name.clone(), value.as_bytes().to_vec());
        self.recapture();
        SecretRef::Env { name }
    }

    /// An `env:` value is captured at start: a rotation is seen after the restart that
    /// captures it again.
    fn rotate(&mut self, reference: &SecretRef, value: &str) {
        let SecretRef::Env { name } = reference else {
            panic!("not an env reference");
        };
        self.values.insert(name.clone(), value.as_bytes().to_vec());
        self.recapture();
    }

    fn absent(&mut self) -> SecretRef {
        SecretRef::Env {
            name: "SYNTHETIC_SECRET_ABSENT".to_owned(),
        }
    }
}

#[test]
fn should_pass_secret_contract_when_provider_is_env_provider() {
    // given
    let mut factory = EnvFactory::default();

    // when
    let report = run(check_secret_contract(&mut factory));

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_read_the_daemon_environment_once_when_env_provider_captures() {
    // given
    let present = SecretRef::Env {
        name: "CARGO_PKG_NAME".to_owned(),
    };
    let absent = SecretRef::Env {
        name: "SYNTHETIC_NEVER_SET_7F3A".to_owned(),
    };
    let provider = EnvProvider::capture(["CARGO_PKG_NAME", "SYNTHETIC_NEVER_SET_7F3A"]);

    // when
    let got = run(async {
        (
            provider.resolve(&present).await.map(|s| s.expose().clone()),
            provider.resolve(&absent).await.map(drop),
        )
    });

    // then
    assert_eq!(got.0.ok().as_deref(), Some("mayfly-adapter-secrets"));
    assert_eq!(got.1.err().map(|e| e.refusal), Some(Refusal::NotFound));
}

// ---------------------------------------------------------------------------------- exec:

/// Quotes `value` for a `/bin/sh` single-quoted word.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// `exec:` sources: scripts in a tempdir, for `ExecProvider` or a broken provider.
struct ExecFactory<P> {
    root: TempDir,
    next: u32,
    owner: u32,
    make: fn(u32, Duration) -> P,
    provider: P,
}

impl<P> ExecFactory<P> {
    fn new(make: fn(u32, Duration) -> P) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let owner = fs::metadata(root.path()).expect("stat").uid();
        Self {
            root,
            next: 0,
            owner,
            make,
            provider: make(owner, DEFAULT_DEADLINE),
        }
    }
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    set_mode(path, 0o700);
}

impl<P: SecretProvider> SecretSourceFactory for ExecFactory<P> {
    type Provider = P;

    fn provider(&self) -> &P {
        &self.provider
    }

    fn install(&mut self, value: &str) -> SecretRef {
        self.install_script("get", &format!("printf '%s' {}", sh_quote(value)))
    }

    fn rotate(&mut self, reference: &SecretRef, value: &str) {
        let SecretRef::Exec { path } = reference else {
            panic!("not an exec reference");
        };
        write_script(path, &format!("printf '%s' {}", sh_quote(value)));
    }

    fn absent(&mut self) -> SecretRef {
        SecretRef::Exec {
            path: new_dir(self.root.path(), &mut self.next).join("missing"),
        }
    }
}

impl<P: SecretProvider> ExecSourceFactory for ExecFactory<P> {
    fn install_script(&mut self, name: &str, body: &str) -> SecretRef {
        self.install_script_with_modes(name, body, 0o700, 0o700)
    }

    fn install_script_with_modes(
        &mut self,
        name: &str,
        body: &str,
        file_mode: u32,
        dir_mode: u32,
    ) -> SecretRef {
        let dir = new_dir(self.root.path(), &mut self.next);
        let path = dir.join(name);
        write_script(&path, body);
        set_mode(&path, file_mode);
        set_mode(&dir, dir_mode);
        SecretRef::Exec { path }
    }

    fn provider_with_deadline(&self, deadline: Duration) -> P {
        (self.make)(self.owner, deadline)
    }
}

/// `ShellExecProvider` (ver §5): runs the reference through `sh -c`, with the daemon's whole
/// environment, checking only its own wrapper. Otherwise careful: it reuses `ExecProvider`'s runner (deadline, process group,
/// exit status, byte rule) over a wrapper script per call that hands the path to a shell.
struct ShellExecProvider {
    wrappers: TempDir,
    calls: AtomicU32,
    runner: ExecProvider,
}

impl ShellExecProvider {
    fn new(owner: u32, deadline: Duration) -> Self {
        Self {
            wrappers: tempfile::tempdir().expect("tempdir"),
            calls: AtomicU32::new(0),
            runner: ExecProvider::new(owner, std::env::vars_os().collect(), deadline),
        }
    }
}

impl SecretProvider for ShellExecProvider {
    async fn resolve(&self, reference: &SecretRef) -> Result<Secret<String>, SecretError> {
        let SecretRef::Exec { path } = reference else {
            return Err(SecretError::new(reference, Refusal::Unsupported));
        };
        if !path.is_absolute() {
            return Err(SecretError::new(reference, Refusal::RelativePath));
        }
        if !path.exists() {
            return Err(SecretError::new(reference, Refusal::NotFound));
        }
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        let wrapper = self.wrappers.path().join(format!("wrapper{call}"));
        write_script(
            &wrapper,
            &format!("exec /bin/sh -c \"exec {}\"", path.display()),
        );
        self.runner
            .resolve(&SecretRef::Exec { path: wrapper })
            .await
            .map_err(|err| SecretError::new(reference, err.refusal))
    }
}

fn exec_provider(owner: u32, deadline: Duration) -> ExecProvider {
    ExecProvider::new(owner, Vec::new(), deadline)
}

#[test]
fn should_pass_exec_contract_when_provider_is_exec_provider() {
    // given
    let _spawning = spawn_lock();
    let mut factory = ExecFactory::new(exec_provider);

    // when
    let report = run(check_exec_contract(&mut factory));

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_fail_argv_environment_and_executable_cases_when_exec_runs_through_a_shell() {
    // given
    let _spawning = spawn_lock();
    let mut factory = ExecFactory::new(ShellExecProvider::new);

    // when
    let report: SecretContractReport = run(check_exec_contract(&mut factory));

    // then: the shell interprets the path and inherits the environment; and the checks see
    // only the wrapper, never the command it hands to the shell
    assert_eq!(report.failed_ids(), ["EX-03", "EX-04", "EX-07"], "{report}");
}

#[test]
fn should_kill_what_the_command_forked_when_the_deadline_passes() {
    // given: the command forks a holder of its stdout and exits; only the group kill ends it
    let _spawning = spawn_lock();
    let mut factory = ExecFactory::new(exec_provider);
    let reference = factory.install_script(
        "forks",
        "tail -f /dev/null &\necho $! > \"$(dirname \"$0\")/holder\"\nprintf 'canary'",
    );
    let SecretRef::Exec { path } = &reference else {
        panic!("not an exec reference");
    };
    let holder_file = path.with_file_name("holder");

    // when
    let got = run(factory
        .provider_with_deadline(Duration::from_secs(2))
        .resolve(&reference));

    // then
    assert!(
        matches!(
            got.as_ref().map_err(|e| &e.refusal),
            Err(Refusal::TimedOut { .. })
        ),
        "{got:?}"
    );
    let holder = fs::read_to_string(holder_file).expect("holder pid");
    // The kill is delivered asynchronously; a dead holder may linger as a zombie when the
    // container's init does not reap orphans, and counts as gone.
    wait_until(
        Duration::from_secs(10),
        "the forked holder of stdout dies with its process group",
        || !alive(holder.trim()),
    );
}

#[test]
fn should_kill_what_the_command_forked_when_the_call_is_dropped() {
    // given: a command that forks a holder of its stdout and never ends by itself
    let _spawning = spawn_lock();
    let mut factory = ExecFactory::new(exec_provider);
    let reference = factory.install_script(
        "forks",
        "tail -f /dev/null &\necho $! > \"$(dirname \"$0\")/holder\"\nexec tail -f /dev/null",
    );
    let SecretRef::Exec { path } = &reference else {
        panic!("not an exec reference");
    };
    let holder_file = path.with_file_name("holder");
    let provider = factory.provider_with_deadline(Duration::from_secs(600));
    let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
    let caller = std::thread::spawn(move || {
        run(async {
            let mut call = Box::pin(provider.resolve(&reference));
            let mut cancelled = cancelled;
            // Drives the call until the test cancels it, then drops it.
            std::future::poll_fn(|cx| {
                if Pin::new(&mut cancelled).poll(cx).is_ready() {
                    return Poll::Ready(None);
                }
                call.as_mut().poll(cx).map(Some)
            })
            .await
            .map(|got| got.map(drop).map_err(|e| e.refusal))
        })
    });
    wait_until(
        Duration::from_secs(10),
        "the command forks its holder and records its pid",
        || fs::read_to_string(&holder_file).is_ok_and(|pid| pid.ends_with('\n')),
    );
    let holder = fs::read_to_string(&holder_file).expect("holder pid");

    // when
    cancel.send(()).expect("the call is still running");
    let answered = caller.join().expect("caller thread");

    // then
    assert_eq!(answered, None, "the call answered by itself");
    // a dead holder may linger as a zombie when no init reaps orphans, and counts as gone
    wait_until(
        Duration::from_secs(10),
        "the forked holder dies with its process group when the call is dropped",
        || !alive(holder.trim()),
    );
}

/// Whether `pid` names a live, non-zombie process.
fn alive(pid: &str) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .stderr(Stdio::null())
        .output()
        .map(|out| {
            let state = String::from_utf8_lossy(&out.stdout);
            let state = state.trim();
            !state.is_empty() && !state.starts_with('Z')
        })
        .unwrap_or(true)
}

// ------------------------------------------------------------------------------ keychain:

/// `keychain:` items served by a fake `security` that prints an item file and a line break,
/// as `security find-generic-password -s <service> -a <account> -w` prints the password.
struct KeychainFactory {
    root: TempDir,
    next: u32,
    provider: KeychainProvider,
}

impl KeychainFactory {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let program = root.path().join("security");
        let items = root.path().join("items");
        fs::create_dir(&items).expect("mkdir");
        write_script(
            &program,
            &format!(
                "[ \"$1\" = find-generic-password ] && [ \"$2\" = -s ] && [ \"$4\" = -a ] \
                 && [ \"$6\" = -w ] && [ $# -eq 6 ] || exit 2\n\
                 item={}/\"$3\"/\"$5\"\n[ -f \"$item\" ] || exit 44\ncat \"$item\"\necho",
                sh_quote(&items.display().to_string())
            ),
        );
        Self {
            root,
            next: 0,
            provider: KeychainProvider::with_program(program, Vec::new(), DEFAULT_DEADLINE),
        }
    }

    fn item(&self, reference: &SecretRef) -> PathBuf {
        let SecretRef::Keychain { service, account } = reference else {
            panic!("not a keychain reference");
        };
        self.root.path().join("items").join(service).join(account)
    }
}

impl SecretSourceFactory for KeychainFactory {
    type Provider = KeychainProvider;

    fn provider(&self) -> &KeychainProvider {
        &self.provider
    }

    fn install(&mut self, value: &str) -> SecretRef {
        self.next += 1;
        let reference = SecretRef::Keychain {
            service: format!("mayfly-test-{}", self.next),
            account: "app-key".to_owned(),
        };
        let item = self.item(&reference);
        fs::create_dir_all(item.parent().expect("parent")).expect("mkdir");
        fs::write(item, value).expect("write");
        reference
    }

    fn rotate(&mut self, reference: &SecretRef, value: &str) {
        fs::write(self.item(reference), value).expect("rotate");
    }

    fn absent(&mut self) -> SecretRef {
        SecretRef::Keychain {
            service: "mayfly-test-absent".to_owned(),
            account: "app-key".to_owned(),
        }
    }
}

#[test]
fn should_pass_secret_contract_when_keychain_is_read_through_a_fake_security() {
    // given
    let _spawning = spawn_lock();
    let mut factory = KeychainFactory::new();

    // when
    let report = run(check_secret_contract(&mut factory));

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_refuse_as_locked_when_security_reports_interaction_not_allowed() {
    // given
    let _spawning = spawn_lock();
    let root = tempfile::tempdir().expect("tempdir");
    let program = root.path().join("security");
    write_script(&program, "printf 'canary-locked'\nexit 36");
    let provider = KeychainProvider::with_program(program, Vec::new(), DEFAULT_DEADLINE);
    let reference = SecretRef::Keychain {
        service: "mayfly".to_owned(),
        account: "app-key".to_owned(),
    };

    // when
    let got = run(provider.resolve(&reference));

    // then
    let err = got.expect_err("refused");
    assert_eq!(err.refusal, Refusal::Locked);
    assert!(!format!("{err} {err:?}").contains("canary"));
}
