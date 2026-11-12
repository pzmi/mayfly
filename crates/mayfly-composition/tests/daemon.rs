//! `mayflyd --role all` as a process (integration level, RT-02): the built daemon over
//! `mayfly-fake-tart` (first on its `PATH`), a SQLite journal and a static demand (one pool
//! with `min_prepared = 1`) in a tempdir state directory. The test waits only on the daemon's
//! documented ready line (`mayfly_composition::daemon::Ready`), read from its stdout through a
//! channel with a bound, and reads the journal and the fake's state once the daemon exited.
//!
//! The binaries are found next to this test executable's directory, where a workspace build
//! puts them (`cargo build -p mayfly-cli -p mayfly-fake-tart` at least).

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use mayfly_adapter_sqlite::{SqliteJournal, SqliteOptions};
use mayfly_adapter_tart::FloorFile;
use mayfly_composition::daemon::{DaemonError, Options, Ready, child_environment, run_with};
use mayfly_composition::paths::{self, Places, PlatformEnv};
use mayfly_composition::supervise::{SessionCut, Shutdown};
use mayfly_control::journal::Journal as _;
use mayfly_control::store::{AggregateStore, NoNotifier, decode_event, stream_of};
use mayfly_control_domain::ControlAggregates;
use mayfly_control_domain::operations::hostrun::{HostRun, HostRunEvent, RunPhase};
use mayfly_control_domain::pool::Pool;
use mayfly_domain::{Epoch, Millis};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::{FakeClock, SeededRng};
use serde_json::Value;

/// The value of the daemon's `env:` secret: it must reach no child (ADR 0058).
const SECRET: &str = "a-test-secret-never-resolved";

/// The longest the test waits for a ready line or an exit.
const BOUND: Duration = Duration::from_secs(60);

fn sibling(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("the test executable");
    let dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>/deps/<test>");
    let path = dir.join(name);
    assert!(
        path.is_file(),
        "{} is not built: build the workspace, or `cargo build -p mayfly-cli -p mayfly-fake-tart`",
        path.display()
    );
    path
}

fn private_dir(path: &Path) {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .expect("a private directory");
}

/// A state directory, a `TART_HOME` in it, a `bin` with the fake `tart`, and a config file.
struct World {
    dir: tempfile::TempDir,
    /// The environment the in-process daemon reads its default places from: `HOME` in the
    /// tempdir, no XDG variables.
    env: PlatformEnv,
    /// The state directory and `TART_HOME` the daemon uses.
    places: Places,
}

const POOL: &str = "lin";

impl World {
    /// A world whose config has `[images]` and `[host]`'s sizing when `complete`, and names
    /// its state directory and `TART_HOME`, made `0700` here.
    fn new(complete: bool) -> Self {
        let world = Self::build(complete, true);
        private_dir(&world.state());
        private_dir(&world.tart_home());
        world
    }

    /// A complete world whose config names no place: the daemon takes the platform's defaults
    /// under the world's `HOME`, which nothing creates beforehand.
    fn with_default_places() -> Self {
        Self::build(true, false)
    }

    fn build(complete: bool, explicit: bool) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = PlatformEnv {
            home: Some(dir.path().join("home")),
            ..PlatformEnv::default()
        };
        let places = if explicit {
            Places {
                state_dir: dir.path().join("state"),
                tart_home: dir.path().join("state").join("tart"),
            }
        } else {
            paths::defaults(&env).expect("a default under HOME")
        };
        let world = Self { dir, env, places };
        private_dir(&world.path("bin"));
        std::os::unix::fs::symlink(sibling("tart"), world.path("bin").join("tart"))
            .expect("the fake tart on PATH");
        let (images, host) = if complete {
            (
                "[images]\nrepository = \"registry.example/mayfly/runner\"\n",
                "[host]\nvm_slots = 2\nmacos_guests = 0\nimage_disk_gib = 50\nclone_disk_gib = 60\n",
            )
        } else {
            ("", "")
        };
        let (state, provider) = if explicit {
            (
                format!("state_dir = \"{}\"\n", world.state().display()),
                format!(
                    "[provider]\ntart_home = \"{}\"\n",
                    world.tart_home().display()
                ),
            )
        } else {
            (String::new(), String::new())
        };
        let config = format!(
            r#"schema_version = 1
{state}

[github]
organisation = "example-org"
app_client_id = "Iv1.0123456789abcdef"
installation_id = 1
private_key = "env:MAYFLY_APP_KEY"

{host}
{provider}
{images}
[[pools]]
name = "{POOL}"
tier = "trusted"
image_manifest = "/etc/mayfly/images/lin.toml"
guest_os = "linux"
cpu = 2
memory_mib = 4096
min_prepared = 1
max_running = 2
"#,
        );
        std::fs::write(world.config(), config).expect("the config");
        world
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn state(&self) -> PathBuf {
        self.places.state_dir.clone()
    }

    fn tart_home(&self) -> PathBuf {
        self.places.tart_home.clone()
    }

    fn config(&self) -> PathBuf {
        self.path("config.toml")
    }

    /// The fake run directly, as an operator's own `tart` with this home would.
    fn tart(&self, args: &[&str]) -> std::process::Output {
        Command::new(sibling("tart"))
            .env_clear()
            .env("TART_HOME", self.tart_home())
            .envs(std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v)))
            .args(args)
            .output()
            .expect("run the fake tart")
    }

    fn names(&self) -> Vec<String> {
        let listed: Value =
            serde_json::from_slice(&self.tart(&["list", "--format", "json"]).stdout)
                .expect("tart list json");
        let mut names: Vec<String> = listed
            .as_array()
            .expect("a list")
            .iter()
            .filter_map(|vm| vm["Name"].as_str().map(str::to_owned))
            .collect();
        names.sort();
        names
    }

    /// Every `tart` subcommand the daemon ran, from the fake's invocation log.
    fn commands(&self) -> Vec<String> {
        std::fs::read_to_string(self.tart_home().join("invocations.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|entry| entry["event"] == "invoke")
            .filter_map(|entry| entry["argv"][0].as_str().map(str::to_owned))
            .collect()
    }

    fn mayflyd(&self, args: &[&str]) -> Command {
        let mut command = Command::new(sibling("mayflyd"));
        command
            .env_clear()
            .env("PATH", self.path("bin"))
            .env("MAYFLY_APP_KEY", SECRET)
            .envs(std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v)))
            .args(args);
        command
    }

    fn start(&self) -> Daemon {
        let mut child = self
            .mayflyd(&["--config", &self.config().display().to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start mayflyd");
        let stdout = child.stdout.take().expect("stdout");
        let (lines, received) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        Daemon { child, received }
    }
}

/// A started `mayflyd`; killed if still running when dropped.
struct Daemon {
    child: Child,
    received: mpsc::Receiver<String>,
}

impl Daemon {
    /// The epoch of the next ready line (the daemon's documented "recovery finished" signal).
    fn ready(&self) -> Epoch {
        let line = self
            .received
            .recv_timeout(BOUND)
            .unwrap_or_else(|e| panic!("no ready line within {BOUND:?}: {e}"));
        assert!(line.starts_with("mayflyd ready "), "{line}");
        assert!(line.ends_with(" mode=normal"), "{line}");
        let epoch = line
            .split(' ')
            .find_map(|part| part.strip_prefix("epoch="))
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no epoch in {line}"));
        Epoch::new(epoch)
    }

    fn kill(mut self) {
        self.child.kill().expect("SIGKILL");
        self.child.wait().expect("reaped");
    }

    fn terminate(mut self) -> Option<i32> {
        let pid = rustix::process::Pid::from_child(&self.child);
        rustix::process::kill_process(pid, rustix::process::Signal::TERM).expect("SIGTERM");
        let mut status = None;
        mayfly_testkit_shared::poll::wait_until(BOUND, "mayflyd to exit after SIGTERM", || {
            status = self.child.try_wait().expect("try_wait");
            status.is_some()
        });
        status.and_then(|s| s.code())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The host run's events and the pool's state, read from the journal once the daemon exited.
fn journal(
    world: &World,
) -> (
    Vec<HostRunEvent>,
    mayfly_control_domain::pool::PoolState,
    u64,
) {
    let journal = SqliteJournal::open(
        &world.state().join("journal.db"),
        SqliteOptions::new(Millis::new(1)),
    )
    .expect("the journal");
    let host = std::fs::read_to_string(world.state().join("host-id")).expect("the host id");
    let host = host.trim_end();
    let stream = stream_of::<HostRun>(host).expect("the host run's stream");
    let events = block_on(journal.read_stream(&stream, None, None))
        .expect("the host run's events")
        .iter()
        .map(|e| decode_event::<HostRunEvent>(e).expect("a host run event"))
        .collect();
    let store = AggregateStore::new::<ControlAggregates>(
        journal,
        FakeClock::at(Millis::new(1)),
        SeededRng::new(1),
        NoNotifier,
    )
    .expect("a store");
    let pool = block_on(store.load::<Pool>(POOL)).expect("the pool");
    let run = block_on(store.load::<HostRun>(host)).expect("the host run");
    assert_eq!(run.phase, RunPhase::Ended, "the last run ended clean");
    (events, pool, run.run_no)
}

#[test]
fn should_converge_on_the_same_inventory_when_the_daemon_is_killed_and_restarted() {
    // given
    let world = World::new(true);
    assert!(world.tart(&["create", "foreign-vm"]).status.success());
    let first = world.start();
    let first_epoch = first.ready();
    assert!(world.tart(&["create", "mf-leftover"]).status.success());
    let refused = world
        .mayflyd(&["--config", &world.config().display().to_string()])
        .output()
        .expect("a second mayflyd");

    // when
    first.kill();
    let second = world.start();
    let second_epoch = second.ready();
    let exit = second.terminate();

    // then
    assert_eq!(
        refused.status.code(),
        Some(69),
        "a second daemon is refused"
    );
    assert_eq!(exit, Some(0), "SIGTERM is a clean stop");
    assert!(second_epoch > first_epoch, "a restart mints a newer epoch");
    assert_eq!(
        FloorFile::in_home(&world.tart_home()).read(),
        Ok(second_epoch),
        "the data plane's floor is the last session's epoch"
    );
    assert_eq!(world.names(), ["foreign-vm", "mf-leftover"]);
    let commands = world.commands();
    assert!(
        commands
            .iter()
            .all(|c| c != "delete" && c != "stop" && c != "clone"),
        "nothing was created, stopped or deleted: {commands:?}"
    );
    let log = std::fs::read_to_string(world.tart_home().join("invocations.jsonl"))
        .expect("the fake's log");
    assert!(
        !log.contains(SECRET) && !log.contains("MAYFLY_APP_KEY"),
        "the daemon's env: secret reached a tart child"
    );
    let (events, pool, runs) = journal(&world);
    assert_eq!(runs, 2);
    let completed: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            HostRunEvent::RecoveryCompleted(c) => Some(c.counts.reported),
            _ => None,
        })
        .collect();
    assert_eq!(
        completed,
        [0, 1],
        "the restart's recovery reported the unrecorded mf-* VM that appeared meanwhile"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, HostRunEvent::PreviousRunEndedUnclean(p) if p.run_no == 1)),
        "the killed run is recorded unclean"
    );
    let config = pool.config.expect("the pool is configured");
    assert_eq!(
        (config.revision, config.min_prepared, config.max_running),
        (1, 1, 2),
        "the static demand is applied once, not again on restart"
    );
    let mode = |path: PathBuf| {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(world.state().join("journal.db")), 0o600);
    assert_eq!(mode(world.state().join("host-id")), 0o600);
}

#[test]
fn should_refuse_to_start_when_the_role_the_images_or_the_sizing_are_missing() {
    // given
    let world = World::new(false);
    let config = world.config().display().to_string();

    // when
    let split = world
        .mayflyd(&["--config", &config, "--role", "control"])
        .output()
        .expect("mayflyd --role control");
    let no_images = world
        .mayflyd(&["--config", &config])
        .output()
        .expect("mayflyd without [images]");
    let no_config = world
        .mayflyd(&[])
        .output()
        .expect("mayflyd without --config");

    // then
    assert_eq!(split.status.code(), Some(64));
    assert_eq!(no_images.status.code(), Some(78));
    let report = String::from_utf8_lossy(&no_images.stderr);
    for field in [
        "images: required to run mayflyd",
        "host.vm_slots: required to run mayflyd",
        "host.macos_guests: required",
        "host.image_disk_gib: required",
        "host.clone_disk_gib: required",
    ] {
        assert!(report.contains(field), "{field} in {report}");
    }
    assert_eq!(no_config.status.code(), Some(64));
    assert!(
        !world.state().join("journal.db").exists(),
        "nothing was started"
    );
}

#[test]
fn should_refuse_to_start_when_the_floor_is_missing_beside_our_vms() {
    // given
    let world = World::new(true);
    assert!(world.tart(&["create", "mf-leftover"]).status.success());

    // when
    let started = world
        .mayflyd(&["--config", &world.config().display().to_string()])
        .output()
        .expect("mayflyd");

    // then
    assert_eq!(started.status.code(), Some(1));
    let report = String::from_utf8_lossy(&started.stderr);
    assert!(
        report.contains("epoch floor") && report.contains("mf-leftover"),
        "{report}"
    );
    assert_eq!(
        FloorFile::in_home(&world.tart_home()).stored(),
        Ok(None),
        "no floor was invented"
    );
    assert_eq!(world.names(), ["mf-leftover"]);
}

/// Runs the daemon in this process (`daemon::run_with`) beside `script`, its sessions ended
/// through `cut`, each ready report sent to `ready`.
fn in_process(
    world: &World,
    cut: &SessionCut,
    shutdown: &Shutdown,
    script: impl std::future::Future<Output = ()>,
    ready: tokio::sync::mpsc::UnboundedSender<Ready>,
) -> Result<(), DaemonError> {
    let options = Options {
        config: world.config(),
        tart: world.path("bin").join("tart"),
        shim_program: sibling("mayfly"),
        environment: child_environment(std::env::vars_os()),
        version: "0.1.0".to_owned(),
        env: world.env.clone(),
    };
    let report = move |r: &Ready| {
        let _ = ready.send(r.clone());
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(async {
            let (ran, ()) = tokio::join!(run_with(options, shutdown.clone(), &report, cut), script);
            ran
        })
}

async fn next_ready(ready: &mut tokio::sync::mpsc::UnboundedReceiver<Ready>) -> Ready {
    mayfly_testkit_shared::poll::within(BOUND, "the daemon's ready report", ready.recv())
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .expect("the daemon is still running")
}

#[test]
fn should_start_a_new_run_with_a_newer_epoch_when_the_link_is_lost_after_recovery() {
    // given
    let world = World::new(true);
    assert!(world.tart(&["create", "foreign-vm"]).status.success());
    let (cut, shutdown) = (SessionCut::default(), Shutdown::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let seen = std::sync::Mutex::new(Vec::new());

    // when
    let script = async {
        let first = next_ready(&mut rx).await;
        cut.cut();
        let second = next_ready(&mut rx).await;
        seen.lock().expect("seen").extend([first, second]);
        shutdown.cancel();
    };
    let ran = in_process(&world, &cut, &shutdown, script, tx);

    // then
    assert!(ran.is_ok(), "{ran:?}");
    let seen = seen.into_inner().expect("seen");
    assert_eq!(seen.len(), 2);
    assert!(seen[1].epoch > seen[0].epoch, "{seen:?}");
    assert_eq!(world.names(), ["foreign-vm"]);
    let commands = world.commands();
    assert!(
        commands.iter().all(|c| c != "delete" && c != "stop"),
        "{commands:?}"
    );
    let (events, _, runs) = journal(&world);
    assert_eq!(runs, 2);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, HostRunEvent::PreviousRunEndedUnclean(p) if p.run_no == 1)),
        "the run whose link was lost is recorded unclean"
    );
}

#[test]
fn should_stop_with_exit_one_when_the_link_is_lost_before_recovery_completes() {
    // given
    let world = World::new(true);
    let (cut, shutdown) = (SessionCut::default(), Shutdown::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    cut.cut();

    // when
    let ran = in_process(&world, &cut, &shutdown, async {}, tx);

    // then
    let error = ran.expect_err("the daemon stops");
    assert!(matches!(error, DaemonError::SessionLost), "{error}");
    assert_eq!(error.exit_code(), 1);
    let readies: Vec<Ready> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(readies.is_empty(), "no run was ready: {readies:?}");
}

#[test]
fn should_create_the_platform_places_private_when_the_config_names_none() {
    // given
    let world = World::with_default_places();
    let (cut, shutdown) = (SessionCut::default(), Shutdown::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    // when
    let script = async {
        let _ready = next_ready(&mut rx).await;
        shutdown.cancel();
    };
    let ran = in_process(&world, &cut, &shutdown, script, tx);

    // then
    assert!(ran.is_ok(), "{ran:?}");
    let mode = |path: PathBuf| {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .permissions()
            .mode()
            & 0o777
    };
    assert!(
        world.state().starts_with(world.path("home")),
        "{:?}",
        world.places
    );
    assert_eq!(mode(world.state()), 0o700);
    assert_eq!(mode(world.tart_home()), 0o700);
    assert!(world.state().join("journal.db").is_file());
    assert!(
        matches!(FloorFile::in_home(&world.tart_home()).stored(), Ok(Some(_))),
        "the floor is in the default TART_HOME"
    );
}

#[test]
fn should_refuse_before_writing_when_an_ancestor_is_open_to_others() {
    // given
    use std::os::unix::fs::PermissionsExt as _;
    let world = World::new(true);
    let open = world.path("open");
    std::fs::create_dir(&open).expect("a directory");
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).expect("0777");
    let config = std::fs::read_to_string(world.config()).expect("the config");
    let moved = config.replace(
        &world.state().display().to_string(),
        &open.join("state").display().to_string(),
    );
    assert_ne!(config, moved);
    std::fs::write(world.config(), moved).expect("the moved config");

    // when
    let started = world
        .mayflyd(&["--config", &world.config().display().to_string()])
        .output()
        .expect("mayflyd");

    // then
    assert_eq!(started.status.code(), Some(73));
    let report = String::from_utf8_lossy(&started.stderr);
    assert!(report.contains("UnsafeAncestor"), "{report}");
    assert_eq!(
        std::fs::read_dir(&open).expect("open").count(),
        0,
        "nothing was created or written below the open directory"
    );
}
