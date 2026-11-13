//! `mayflyd --role all` as a process (integration level, RT-02): the built daemon over
//! `mayfly-fake-tart` (first on its `PATH`), a SQLite journal and a static demand (one pool
//! with `min_prepared = 1`, its image manifest beside the config) in a tempdir state directory.
//!
//! Most tests configure a loopback repository nothing serves, so the placement of the pool's
//! pin (CP-07) fails at once, offline; they wait only on the daemon's documented ready line
//! (`mayfly_composition::daemon::Ready`), read from its stdout through a channel with a bound,
//! and read the journal and the fake's state once the daemon exited. The terms policy (P4,
//! CP-08): a pool pinned from its manifest (OPS-02) is open on its host after the run's ready
//! line, at the pool's current terms revision.
//!
//! The placement tests (`mod placement`, VER-09, Linux only) do pull: `mayfly-fake-registry`
//! serves a synthetic Tart image over HTTPS under a CA made for the test, handed to `mayflyd` as
//! `SSL_CERT_FILE` (ver §7.5), and the same image is seeded in fake-tart's registry, so the
//! daemon pulls, verifies and materialises the pinned image's base, the reconciler reserves
//! for the pool and starts the reservation (P2, CP-10), and the attempt's VM is cloned from the
//! base, booted under its shim and its guest verified until it is Prepared. The ready line
//! promises no placement, so these tests poll `mayfly status --json` (RT-03) with a bound for
//! the documented fact they wait for (the pool's place on the host, `pools[].hosts[]`; an
//! attempt Prepared, `attempts.by_phase`), and read the journal once the daemon exited. A VM
//! they made is stopped with `tart stop` once the daemon exited, and its shim waited for
//! (`placement::Reaped`): the daemon leaves VMs running under their shims by design (ADR 0037).
//!
//! The binaries are found next to this test executable's directory, where a workspace build
//! puts them (`cargo build -p mayfly-cli -p mayfly-fake-tart` at least: `mayfly` runs each VM's
//! shim).

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
use mayfly_control::placement::image_instance;
use mayfly_control::store::{AggregateStore, NoNotifier, decode_event, stream_of};
use mayfly_control_domain::ControlAggregates;
use mayfly_control_domain::attempt::GuestOs;
use mayfly_control_domain::host::{Host, HostEvent};
use mayfly_control_domain::image::{Image, ImageEvent, ImageManifest, ImageStatus, RunnerVersion};
use mayfly_control_domain::operations::hostrun::{HostRun, HostRunEvent, RunPhase};
use mayfly_control_domain::pool::{Pool, PoolEvent, terms};
use mayfly_domain::{Epoch, EventId, ImageDigest, Millis};
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
    /// The CA file `mayflyd` trusts through `SSL_CERT_FILE`, when it is given one.
    trust: Option<PathBuf>,
}

const POOL: &str = "lin";

/// The repository the world's config and manifest name: loopback, where nothing serves.
const REPOSITORY: &str = "127.0.0.1/mayfly/runner";

fn digest(hex: char) -> String {
    format!("sha256:{}", hex.to_string().repeat(64))
}

/// A valid manifest of a Linux image `source` (`tests/fixtures/manifest/valid/linux.toml`'s
/// shape).
fn manifest_text(source: &str) -> String {
    format!(
        r#"schema = 1
provider = "tart"
os = "linux"
arch = "arm64"
source = "{source}"
revision = 1

[runner]
version = "2.330.0"
sha256 = "{runner}"

[launcher]
sha256 = "{launcher}"

[capabilities]
guest_agent = true
"#,
        runner = "c".repeat(64),
        launcher = "d".repeat(64),
    )
}

impl World {
    /// A world whose config has `[images]` and `[host]`'s sizing when `complete`, and names
    /// its state directory and `TART_HOME`, made `0700` here.
    fn new(complete: bool) -> Self {
        let world = Self::build(complete, true, None);
        private_dir(&world.state());
        private_dir(&world.tart_home());
        world
    }

    /// A complete world whose config names no place: the daemon takes the platform's defaults
    /// under the world's `HOME`, which nothing creates beforehand.
    fn with_default_places() -> Self {
        Self::build(true, false, None)
    }

    /// `served` replaces a complete world's repository and `[host]` table: `(repository,
    /// host)`.
    fn build(complete: bool, explicit: bool, served: Option<(&str, &str)>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        // `TMPDIR` is the world's own private dir: on macOS, where the tempdir's long path
        // pushes the default socket under `HOME` past the socket-address limit, the daemon falls
        // back to it (RT-03F), as it does on an owner's Mac with a long `HOME`
        let env = PlatformEnv {
            home: Some(dir.path().join("home")),
            tmpdir: Some(dir.path().to_path_buf()),
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
        let world = Self {
            dir,
            env,
            places,
            trust: None,
        };
        private_dir(&world.path("bin"));
        std::os::unix::fs::symlink(sibling("tart"), world.path("bin").join("tart"))
            .expect("the fake tart on PATH");
        let (repository, host) = served.unwrap_or((
            REPOSITORY,
            "[host]\nvm_slots = 2\nmacos_guests = 0\nimage_disk_gib = 50\nclone_disk_gib = 60\n",
        ));
        let (images, host) = if complete {
            (format!("[images]\nrepository = \"{repository}\"\n"), host)
        } else {
            (String::new(), "")
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
        world.write_manifest(&format!("{repository}@{}", digest('a')));
        let manifest = world.manifest().display().to_string();
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
image_manifest = "{manifest}"
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

    fn manifest(&self) -> PathBuf {
        self.path("lin.toml")
    }

    /// Writes the pool's manifest with the image `source`.
    fn write_manifest(&self, source: &str) {
        std::fs::write(self.manifest(), manifest_text(source)).expect("the manifest");
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

    /// `mayfly --config <config> status --json` against the running daemon (RT-03): the
    /// documented status, parsed; `Null` when the CLI did not answer one (exit not 0, output
    /// not JSON), so a poll can try again.
    fn try_status(&self) -> Value {
        let output = Command::new(sibling("mayfly"))
            .env_clear()
            .envs(std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v)))
            .args([
                "--config",
                &self.config().display().to_string(),
                "status",
                "--json",
            ])
            .output()
            .expect("run mayfly");
        if output.status.code() != Some(0) {
            return Value::Null;
        }
        serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
    }

    /// [`World::try_status`], which must answer.
    fn status(&self) -> Value {
        let status = self.try_status();
        assert_ne!(status, Value::Null, "mayfly status --json answered nothing");
        status
    }

    /// The pool's entry on `host` in [`World::try_status`] (`pools[].hosts[]`, VER-09's signal),
    /// `Null` while there is none.
    #[cfg(target_os = "linux")]
    fn pool_on(&self, host: &str) -> Value {
        let status = self.try_status();
        status["pools"]
            .as_array()
            .and_then(|pools| pools.iter().find(|p| p["name"] == POOL))
            .and_then(|pool| pool["hosts"].as_array())
            .and_then(|hosts| hosts.iter().find(|h| h["host"] == host))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn mayflyd(&self, args: &[&str]) -> Command {
        let mut command = Command::new(sibling("mayflyd"));
        command
            .env_clear()
            .env("PATH", self.path("bin"))
            .env("MAYFLY_APP_KEY", SECRET)
            .envs(std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v)))
            .envs(self.trust.as_ref().map(|ca| ("SSL_CERT_FILE", ca)))
            .args(args);
        command
    }

    /// Starts `mayflyd`.
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
fn should_refuse_to_start_when_the_role_or_the_images_are_missing() {
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
    assert!(
        report.contains("images: required to run mayflyd"),
        "{report}"
    );
    assert!(
        !report.contains("host."),
        "no sizing key is required (ADR 0052's amendment): {report}"
    );
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
    // the run owns the report, so its sender is dropped when the daemon stops: a script
    // waiting for a ready line then sees the channel close at once (`next_ready` is `None`)
    // and the test reports the daemon's error instead of waiting out its bound
    let run = async move { run_with(options, shutdown.clone(), &report, cut).await };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(async {
            let (ran, ()) = tokio::join!(run, script);
            ran
        })
}

/// The next ready report, or `None` once the daemon stopped without one.
async fn next_ready(ready: &mut tokio::sync::mpsc::UnboundedReceiver<Ready>) -> Option<Ready> {
    mayfly_testkit_shared::poll::within(BOUND, "the daemon's ready report", ready.recv())
        .await
        .unwrap_or_else(|e| panic!("{e}"))
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
        let Some(first) = next_ready(&mut rx).await else {
            return;
        };
        cut.cut();
        let Some(second) = next_ready(&mut rx).await else {
            return;
        };
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
        if next_ready(&mut rx).await.is_some() {
            shutdown.cancel();
        }
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

#[test]
fn should_start_and_report_each_sizing_source_when_no_sizing_key_is_configured() {
    // given: the world's config without any `[host]` sizing key
    let world = World::new(true);
    let text = std::fs::read_to_string(world.config()).expect("the config");
    let without_sizing = text.replace(
        "[host]\nvm_slots = 2\nmacos_guests = 0\nimage_disk_gib = 50\nclone_disk_gib = 60\n",
        "",
    );
    assert_ne!(text, without_sizing, "the sizing keys were removed");
    std::fs::write(world.config(), without_sizing).expect("the config");

    // when: the status read once the ready line is out (RT-03 took the start-up report over
    // from stderr, RT-06's note)
    let daemon = world.start();
    daemon.ready();
    let status = world.status();
    let code = daemon.terminate();

    // then: it starts, and the status names every value's source
    assert_eq!(code, Some(0));
    let sizing: Vec<String> = status["sizing"]
        .as_array()
        .expect("the sizing")
        .iter()
        .map(|v| {
            format!(
                "{}={} source={}",
                v["key"].as_str().unwrap_or_default(),
                v["value"].as_str().unwrap_or_default(),
                v["source"].as_str().unwrap_or_default()
            )
        })
        .collect();
    for line in [
        "macos_guests=2 source=apple_limit",
        "reserve_cpus=2 source=default",
        "image_disk_gib=per_image source=manifest",
        "clone_disk_gib=per_image source=manifest",
    ] {
        assert!(sizing.iter().any(|l| l == line), "{line} in {sizing:?}");
    }
    assert!(
        sizing
            .iter()
            .any(|l| l.starts_with("vm_slots=") && l.ends_with("source=measured")),
        "{sizing:?}"
    );
    assert_eq!(status["run"]["ready"], Value::Bool(true));
}

/// The terms of pool `lin` that host `host` applied, in order, as `(revision, open)`.
fn applied_terms(journal: &SqliteJournal, host: &str) -> Vec<(u64, bool)> {
    let stream = stream_of::<Host>(host).expect("the host's stream");
    block_on(journal.read_stream(&stream, None, None))
        .expect("the host's events")
        .iter()
        .filter_map(
            |e| match decode_event::<HostEvent>(e).expect("a host event") {
                HostEvent::PoolTermsApplied(t) if t.pool.as_str() == POOL => {
                    Some((t.revision, t.open))
                }
                _ => None,
            },
        )
        .collect()
}

#[test]
fn should_apply_the_pools_terms_to_its_host_when_the_pool_is_pinned() {
    // given: a run pins the pool from its manifest (OPS-02) before its ready line; the image is
    // on no registry, so the placement fails at once and no pull happens (a real
    // materialisation under mayflyd is VER-09's). The pin precedes the run's first terms pass,
    // so the closed terms of an unpinned pool are never applied here: that is
    // `mayfly-control`'s `tests/store/terms.rs`.
    let world = World::new(true);
    let first = world.start();
    first.ready();
    assert_eq!(first.terminate(), Some(0));
    let host = std::fs::read_to_string(world.state().join("host-id")).expect("the host id");
    let host = host.trim_end().to_owned();
    let (_, pool, _) = journal(&world);
    let before = applied_terms(&open_journal(&world), &host);
    let pinned = terms(&pool).expect("the pool's terms");

    // when: the next run moves the pin to another image (a terms bump)
    world.write_manifest(&format!("{REPOSITORY}@{}", digest('b')));
    let second = world.start();
    second.ready();
    let exit = second.terminate();

    // then: open at the pinned pool's terms revision after each run's ready line
    assert_eq!(exit, Some(0));
    let (_, pool, _) = journal(&world);
    let after = applied_terms(&open_journal(&world), &host);
    let repinned = terms(&pool).expect("the pool's terms");
    assert!(
        pinned.open && repinned.open,
        "a configured, active, pinned pool is open"
    );
    assert!(
        repinned.revision > pinned.revision,
        "the repin bumped the terms"
    );
    assert!(
        before.iter().all(|(_, open)| *open),
        "the pin precedes the first terms pass, so no closed terms were applied: {before:?}"
    );
    assert_eq!(before.last(), Some(&(pinned.revision, true)), "{before:?}");
    assert_eq!(after.last(), Some(&(repinned.revision, true)), "{after:?}");
    assert_eq!(after.first(), before.first(), "{after:?}");
}

fn open_journal(world: &World) -> SqliteJournal {
    SqliteJournal::open(
        &world.state().join("journal.db"),
        SqliteOptions::new(Millis::new(1)),
    )
    .expect("the journal")
}

/// An image stream's events with their ids.
type ImageEvents = Vec<(EventId, ImageEvent)>;

/// Every `ImagePinned` of the pool as (digest, previous, verified_ref).
type Pins = Vec<(ImageDigest, Option<ImageDigest>, EventId)>;

/// The events of `digest`'s image stream and the pool's pins, read from the journal once the
/// daemon exited.
fn catalog(world: &World, digest: &ImageDigest) -> (ImageEvents, Pins) {
    let journal = SqliteJournal::open(
        &world.state().join("journal.db"),
        SqliteOptions::new(Millis::new(1)),
    )
    .expect("the journal");
    let image = stream_of::<Image>(image_instance(digest)).expect("the image's stream");
    let images = block_on(journal.read_stream(&image, None, None))
        .expect("the image's events")
        .iter()
        .map(|e| {
            (
                e.event_id,
                decode_event::<ImageEvent>(e).expect("an image event"),
            )
        })
        .collect();
    let pool = stream_of::<Pool>(POOL).expect("the pool's stream");
    let pins = block_on(journal.read_stream(&pool, None, None))
        .expect("the pool's events")
        .iter()
        .filter_map(
            |e| match decode_event::<PoolEvent>(e).expect("a pool event") {
                PoolEvent::ImagePinned(p) => Some((p.digest, p.previous, p.verified_ref)),
                _ => None,
            },
        )
        .collect();
    (images, pins)
}

fn verified_id(events: &[(EventId, ImageEvent)]) -> EventId {
    events
        .iter()
        .find_map(|(id, e)| matches!(e, ImageEvent::ImageVerified(_)).then_some(*id))
        .expect("the image is verified")
}

#[test]
fn should_pin_each_pool_to_its_manifest_image_when_the_daemon_starts() {
    // given
    let world = World::new(true);
    let (a, b) = (
        ImageDigest::new(&digest('a')).expect("a digest"),
        ImageDigest::new(&digest('b')).expect("a digest"),
    );

    // when: a start with the manifest of image a, then one with image b, then one more with b
    let first = world.start();
    first.ready();
    assert_eq!(first.terminate(), Some(0));
    world.write_manifest(&format!("{REPOSITORY}@{b}"));
    let second = world.start();
    second.ready();
    assert_eq!(second.terminate(), Some(0));
    let (image_b_then, pins_then) = catalog(&world, &b);
    let third = world.start();
    third.ready();
    assert_eq!(third.terminate(), Some(0));

    // then: each image registered from its manifest and verified, the pool pinned to a, then
    // moved to b, each pin naming its image's verification
    let (image_a, pins) = catalog(&world, &a);
    let (image_b, _) = catalog(&world, &b);
    let registered: Vec<&ImageManifest> = image_a
        .iter()
        .filter_map(|(_, e)| match e {
            ImageEvent::ImageRegistered(r) => Some(&r.manifest),
            _ => None,
        })
        .collect();
    assert_eq!(
        registered,
        [&ImageManifest {
            os: GuestOs::Linux,
            runner_version: RunnerVersion::new(2, 330, 0),
            revision: 1,
        }]
    );
    assert_eq!(
        pins,
        [
            (a.clone(), None, verified_id(&image_a)),
            (b.clone(), Some(a.clone()), verified_id(&image_b)),
        ],
        "one pin per manifest change"
    );
    assert_eq!(pins, pins_then, "an unchanged manifest pins nothing again");
    assert_eq!(
        image_b, image_b_then,
        "an unchanged manifest registers and verifies nothing again"
    );
    let store = AggregateStore::new::<ControlAggregates>(
        SqliteJournal::open(
            &world.state().join("journal.db"),
            SqliteOptions::new(Millis::new(1)),
        )
        .expect("the journal"),
        FakeClock::at(Millis::new(1)),
        SeededRng::new(1),
        NoNotifier,
    )
    .expect("a store");
    let image = block_on(store.load::<Image>(image_instance(&b))).expect("image b");
    assert_eq!(image.status, ImageStatus::Verified);
    let pool = block_on(store.load::<Pool>(POOL)).expect("the pool");
    assert_eq!(pool.image_pin, Some(b));
}

#[test]
fn should_refuse_to_start_when_a_pool_manifest_names_a_tag() {
    // given
    let world = World::new(true);
    world.write_manifest(&format!("{REPOSITORY}:latest"));

    // when
    let started = world
        .mayflyd(&["--config", &world.config().display().to_string()])
        .output()
        .expect("mayflyd");

    // then
    assert_eq!(started.status.code(), Some(78));
    let report = String::from_utf8_lossy(&started.stderr);
    assert!(
        report.contains("pools[0].image_manifest (source): is a tag-only reference"),
        "{report}"
    );
    assert!(
        !world.state().join("journal.db").exists(),
        "nothing was started"
    );
}

/// A real placement under `mayflyd` (VER-09, ver §7.5). **Linux only, deliberately:** the
/// daemon trusts the registry's CA through `SSL_CERT_FILE` in its environment, which the
/// platform verifier's Linux root loading reads (rustls-native-certs), so no production code or
/// configuration changes for the test; macOS's verifier trusts its keychain alone, which a test
/// cannot write without the owner. CI runs these on Linux.
#[cfg(target_os = "linux")]
mod placement {
    use mayfly_adapter_tart::shim::{self, RunRecord};
    use mayfly_control_domain::attempt::{Attempt, AttemptEvent, GuestOs, RefusalReason};
    use mayfly_control_domain::image::{RunnerVersion, base_name};
    use mayfly_control_domain::placement::{ImagePlacement, PlacementEvent, placement_instance};
    use mayfly_data::Shim;
    use mayfly_domain::{AttemptId, HostId};
    use mayfly_fake_registry::{FakeRegistry, Images, Served, TartImage, seed_tart_home};

    use super::*;

    /// How long the reaping waits for a shim to let go of its lock after its VM stopped.
    const RELEASE: Duration = Duration::from_secs(20);

    /// Stops every VM the daemon left running in the world's `TART_HOME` with `tart stop` and
    /// waits for each shim to exit when dropped (declared before the daemon, so dropped after
    /// it); a `tart run` that does not end is killed by the pid its shim recorded, a process
    /// this test's daemon started. The test support that keeps a child alive stops it.
    struct Reaped<'w>(&'w World);

    impl Drop for Reaped<'_> {
        fn drop(&mut self) {
            let listed = self.0.tart(&["list", "--format", "json"]);
            let entries: Vec<Value> = serde_json::from_slice(&listed.stdout).unwrap_or_default();
            for entry in entries.iter().filter(|e| e["Running"] == true) {
                if let Some(name) = entry["Name"].as_str() {
                    let _stopped = self.0.tart(&["stop", name, "--timeout", "5"]);
                }
            }
            let Ok(dirs) = std::fs::read_dir(self.0.state().join("shims")) else {
                return;
            };
            for dir in dirs.filter_map(Result::ok).map(|e| e.path()) {
                let released = || shim::read(&dir).shim != Shim::Alive;
                let waited = mayfly_testkit_shared::poll::try_wait_until(
                    RELEASE,
                    "a shim to exit after its VM stopped",
                    released,
                );
                if waited.is_ok() {
                    continue;
                }
                if let Some(record) = shim::read_json::<RunRecord>(&dir.join(shim::RECORD))
                    && let Some(pid) = i32::try_from(record.child_pid)
                        .ok()
                        .and_then(rustix::process::Pid::from_raw)
                {
                    let _killed = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                    let _ = mayfly_testkit_shared::poll::try_wait_until(
                        RELEASE,
                        "a shim to exit after its run was killed",
                        released,
                    );
                }
            }
        }
    }

    /// The repository path the registry publishes the image under.
    const IMAGE_NAME: &str = "mayfly/runner";

    /// The runner the pool's manifest pins (`manifest_text`'s; the fake guest installs any
    /// version given a well-formed checksum).
    const RUNNER: RunnerVersion = RunnerVersion::new(2, 330, 0);

    /// The host's sizing: small enough that the data plane's resource gate (I33) admits the
    /// pull, the base and a clone on a test machine's real free disk and memory, which it
    /// measures (an image of 50 GiB, a clone of 60 GiB or the default 50 GiB disk reserve would
    /// be refused `insufficient_resources` on a small CI disk).
    const HOST: &str = "[host]\nvm_slots = 2\nmacos_guests = 0\nimage_disk_gib = 1\nclone_disk_gib = 1\nreserve_disk_gib = 1\nreserve_memory_mib = 512\n";

    fn image() -> TartImage {
        TartImage::synthetic(b"ver09-runner-image", mayfly_fake_registry::GuestOs::Linux)
    }

    /// A Linux Tart image published at [`IMAGE_NAME`] and the registry serving it.
    fn registry() -> (TartImage, FakeRegistry) {
        let image = image();
        let registry =
            FakeRegistry::start(Images::new().publish(IMAGE_NAME, &image)).expect("a registry");
        (image, registry)
    }

    /// A complete world whose `[images]` repository is `registry`'s [`IMAGE_NAME`], whose
    /// pool's manifest (OPS-02) pins `image` there by digest, whose fake-tart registry holds
    /// `image` as `registry` serves it, and whose `mayflyd` trusts `registry`'s CA when
    /// `trusted`.
    fn serving(registry: &FakeRegistry, image: &TartImage, trusted: bool) -> World {
        let repository = format!("{}/{IMAGE_NAME}", registry.host());
        let mut world = World::build(true, true, Some((&repository, HOST)));
        // a VM of 1 GiB, the least a pool may configure, so the data plane's gate (I33), which
        // measures the machine's real available memory, admits the clone beside a busy build
        let config = std::fs::read_to_string(world.config()).expect("the config");
        assert!(config.contains("memory_mib = 4096"), "{config}");
        std::fs::write(
            world.config(),
            config.replace("memory_mib = 4096", "memory_mib = 1024"),
        )
        .expect("the config");
        private_dir(&world.state());
        private_dir(&world.tart_home());
        world.write_manifest(&format!("{repository}@{}", image.digest()));
        seed_tart_home(&world.tart_home(), &repository, image).expect("fake-tart's registry");
        if trusted {
            let ca = world.path("registry-ca.pem");
            std::fs::write(&ca, registry.ca_pem()).expect("the CA file");
            world.trust = Some(ca);
        }
        world
    }

    fn host_of(world: &World) -> HostId {
        let host = std::fs::read_to_string(world.state().join("host-id")).expect("the host id");
        HostId::new(host.trim_end()).expect("a host id")
    }

    /// The pool's reservations on `host`, as `(pool, os)`.
    fn reservations(journal: &SqliteJournal, host: &HostId) -> Vec<(String, GuestOs)> {
        let stream = stream_of::<Host>(host.as_str()).expect("the host's stream");
        block_on(journal.read_stream(&stream, None, None))
            .expect("the host's events")
            .iter()
            .filter_map(
                |e| match decode_event::<HostEvent>(e).expect("a host event") {
                    HostEvent::SlotReserved(r) => Some((r.pool.as_str().to_owned(), r.os)),
                    _ => None,
                },
            )
            .collect()
    }

    /// The attempts the host reserved a slot for, in order.
    fn reserved_attempts(journal: &SqliteJournal, host: &HostId) -> Vec<AttemptId> {
        let stream = stream_of::<Host>(host.as_str()).expect("the host's stream");
        block_on(journal.read_stream(&stream, None, None))
            .expect("the host's events")
            .iter()
            .filter_map(
                |e| match decode_event::<HostEvent>(e).expect("a host event") {
                    HostEvent::SlotReserved(r) => Some(r.attempt_id),
                    _ => None,
                },
            )
            .collect()
    }

    /// Whether the host released the lease of `attempt`.
    fn released(journal: &SqliteJournal, host: &HostId, attempt: AttemptId) -> bool {
        let stream = stream_of::<Host>(host.as_str()).expect("the host's stream");
        block_on(journal.read_stream(&stream, None, None))
            .expect("the host's events")
            .iter()
            .any(
                |e| match decode_event::<HostEvent>(e).expect("a host event") {
                    HostEvent::SlotReleased(r) => r.attempt_id == attempt,
                    _ => false,
                },
            )
    }

    /// The events of `attempt`'s stream (none before it started).
    fn attempt_events(journal: &SqliteJournal, attempt: AttemptId) -> Vec<AttemptEvent> {
        let stream = stream_of::<Attempt>(&attempt.to_string()).expect("the attempt's stream");
        block_on(journal.read_stream(&stream, None, None))
            .map(|events| {
                events
                    .iter()
                    .map(|e| decode_event::<AttemptEvent>(e).expect("an attempt event"))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The first reserved attempt that became Prepared, if any.
    fn prepared(journal: &SqliteJournal, host: &HostId) -> Option<AttemptId> {
        reserved_attempts(journal, host).into_iter().find(|a| {
            attempt_events(journal, *a)
                .iter()
                .any(|e| matches!(e, AttemptEvent::AttemptPrepared(_)))
        })
    }

    /// The events of `digest`'s placement on `host`.
    fn placement(
        journal: &SqliteJournal,
        host: &HostId,
        digest: &ImageDigest,
    ) -> Vec<PlacementEvent> {
        let stream = stream_of::<ImagePlacement>(&placement_instance(host, digest))
            .expect("the placement's stream");
        block_on(journal.read_stream(&stream, None, None))
            .map(|events| {
                events
                    .iter()
                    .map(|e| decode_event::<PlacementEvent>(e).expect("a placement event"))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A run that pins the pool from its manifest at start-up (OPS-02), returned with its
    /// host once ready.
    fn pinned_run(world: &World) -> (Daemon, HostId) {
        let daemon = world.start();
        daemon.ready();
        (daemon, host_of(world))
    }

    #[test]
    fn should_materialise_the_pins_base_and_reserve_for_the_pool_when_the_registry_serves_its_image()
     {
        // given: the registry serves the image over HTTPS under a CA mayflyd trusts, fake-tart's
        // registry holds the same bytes, and the pool's manifest pins the image by digest (OPS-02)
        let (image, registry) = registry();
        let digest = ImageDigest::new(image.digest()).expect("a digest");
        let world = serving(&registry, &image, true);
        let _reaped = Reaped(&world);
        let (daemon, host) = pinned_run(&world);

        // when: the placement manager places the pin and the reconciler reserves. A bounded poll
        // of `mayfly status --json` (RT-03) for the pool on this host, admitted (its terms open,
        // its base materialised) and holding a lease: the documented signal of
        // `pools[].hosts[]` (`mayfly_composition::operator::wire::PoolOnHost`). A poll, because
        // the ready line promises recovery and one terms pass, never a placement, and a
        // placement takes a pull of unknown length.
        let mut last = Value::Null;
        let waited = mayfly_testkit_shared::poll::try_wait_until(
            BOUND,
            "the pool admitted on the host with a lease, in mayfly status",
            || {
                last = world.pool_on(host.as_str());
                last["terms_open"] == Value::Bool(true)
                    && last["placement"] == "materialised"
                    && last["leases"].as_u64().is_some_and(|n| n >= 1)
            },
        );
        if let Err(timed_out) = waited {
            // Name what the placement ended in: a refusal `insufficient_resources` means the
            // test machine has less free disk than `HOST` needs (ver §7.5).
            let journal = open_journal(&world);
            panic!(
                "{timed_out}; status: {last}; the placement: {:?}",
                placement(&journal, &host, &digest)
            );
        }
        let exit = daemon.terminate();

        // then: the manifest was fetched over HTTPS by digest, Tart pulled and the base was
        // materialised at the pinned runner, measured at the image's disk, and the reconciler
        // reserved for the pool on the host
        assert_eq!(exit, Some(0));
        let manifest = format!("/v2/{IMAGE_NAME}/manifests/{digest}");
        assert!(
            registry.served().contains(&Served {
                method: "GET".to_owned(),
                path: manifest,
                status: 200,
            }),
            "{:?}",
            registry.served()
        );
        let journal = open_journal(&world);
        let events = placement(&journal, &host, &digest);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PlacementEvent::ImagePulled(_))),
            "{events:?}"
        );
        let materialised: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                PlacementEvent::ImageMaterialised(m) => Some((m.runner_version, m.disk_bytes)),
                _ => None,
            })
            .collect();
        assert_eq!(
            materialised,
            [(RUNNER, Some(image.disk_bytes()))],
            "{events:?}"
        );
        assert_eq!(
            reservations(&journal, &host).first(),
            Some(&(POOL.to_owned(), GuestOs::Linux))
        );
        assert!(
            world.names().contains(&base_name(&digest, RUNNER)),
            "{:?}",
            world.names()
        );
        let commands = world.commands();
        for command in ["pull", "clone", "exec"] {
            assert!(
                commands.iter().any(|c| c == command),
                "{command}: {commands:?}"
            );
        }
    }

    #[test]
    fn should_prepare_an_attempt_for_the_pool_when_its_reservation_is_started() {
        // given: the registry serves the pool's pinned image (as above), and the pool holds one
        // VM prepared ahead of demand (`min_prepared = 1`)
        let (image, registry) = registry();
        let digest = ImageDigest::new(image.digest()).expect("a digest");
        let world = serving(&registry, &image, true);
        let _reaped = Reaped(&world);
        let (daemon, host) = pinned_run(&world);

        // when: the base is materialised, the reconciler reserves and starts (P2, CP-10), and
        // the saga clones, boots and verifies over fake-tart. A bounded poll of `mayfly status
        // --json` (RT-03) for an attempt in phase `prepared` (`attempts.by_phase`, the attempts
        // projection's phase counts: `mayfly_composition::operator::wire::AttemptStatus`). A
        // poll, because the ready line promises recovery and one terms pass, never a placement
        // or a preparation, and both take a pull and a boot of unknown length.
        let mut last = Value::Null;
        let waited = mayfly_testkit_shared::poll::try_wait_until(
            BOUND,
            "an attempt prepared, in mayfly status",
            || {
                last = world.try_status();
                last["attempts"]["by_phase"]["prepared"]
                    .as_u64()
                    .is_some_and(|n| n >= 1)
            },
        );
        if let Err(timed_out) = waited {
            // Name where it stopped: the placement, then each reserved attempt's events.
            let journal = open_journal(&world);
            let attempts: Vec<_> = reserved_attempts(&journal, &host)
                .into_iter()
                .map(|a| (a, attempt_events(&journal, a)))
                .collect();
            panic!(
                "{timed_out}; status: {last}; the placement: {:?}; the attempts: {attempts:?}",
                placement(&journal, &host, &digest)
            );
        }
        let exit = daemon.terminate();

        // then: started by the reconciler on the pool's pin, prepared, its VM cloned from the
        // base and running, its lease still held
        assert_eq!(exit, Some(0));
        let journal = open_journal(&world);
        let attempt = prepared(&journal, &host).expect("a prepared attempt in the journal");
        let events = attempt_events(&journal, attempt);
        let started: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AttemptEvent::AttemptStarted(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(started.len(), 1, "{events:?}");
        assert_eq!(started[0].image_digest, digest);
        assert_eq!(started[0].pool.as_str(), POOL);
        assert!(!released(&journal, &host, attempt));
        assert!(
            world.names().contains(&started[0].vm_name.to_string()),
            "{:?}",
            world.names()
        );
        let commands = world.commands();
        for command in ["clone", "run", "exec"] {
            assert!(
                commands.iter().any(|c| c == command),
                "{command}: {commands:?}"
            );
        }
    }

    /// Waits for the data plane's refusal of the pull, stops the daemon and checks that the
    /// refusal was definite (its effect failed: the manifest was not accepted, never a resource
    /// shortfall, which the host's sizing rules out), that Tart was never asked to pull and
    /// that nothing was materialised or reserved.
    fn assert_refused_before_any_pull(
        world: &World,
        daemon: Daemon,
        host: &HostId,
        digest: &ImageDigest,
    ) {
        // A bounded poll of `mayfly status --json` (RT-03) for the placement `failed`, which a
        // refused pull records in the same unit as its refusal (`pools[].hosts[].placement`),
        // because the ready line promises no placement.
        mayfly_testkit_shared::poll::wait_until(
            BOUND,
            "the placement failed by the data plane's refusal, in mayfly status",
            || world.pool_on(host.as_str())["placement"] == "failed",
        );
        assert_eq!(daemon.terminate(), Some(0));
        let journal = open_journal(world);
        let events = placement(&journal, host, digest);
        let refusals: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                PlacementEvent::PlacementDirectiveRefused(r) => Some(r.reason),
                _ => None,
            })
            .collect();
        assert_eq!(
            refusals.first(),
            Some(&RefusalReason::EffectFailed),
            "{events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(
                e,
                PlacementEvent::ImagePulled(_) | PlacementEvent::ImageMaterialised(_)
            )),
            "{events:?}"
        );
        assert!(reservations(&journal, host).is_empty());
        assert!(!world.commands().iter().any(|c| c == "pull"));
        assert!(!world.names().contains(&base_name(digest, RUNNER)));
    }

    #[test]
    fn should_pull_nothing_when_mayflyd_does_not_trust_the_registrys_ca() {
        // given: the same registry and pin, but mayflyd trusts only the system's roots
        let (image, registry) = registry();
        let digest = ImageDigest::new(image.digest()).expect("a digest");
        let world = serving(&registry, &image, false);

        // when
        let (daemon, host) = pinned_run(&world);

        // then: the handshake failed before any request, so the pull was refused before Tart
        assert_refused_before_any_pull(&world, daemon, &host, &digest);
        assert!(registry.served().is_empty(), "{:?}", registry.served());
    }

    #[test]
    fn should_pull_nothing_when_the_registry_serves_other_bytes_at_the_pinned_digest() {
        // given: a trusted registry that answers the pinned digest with another image's
        // manifest (a tampered or substituted manifest); fake-tart's registry holds the pinned
        // image, so only the data plane's own check of the bytes against the digest stops it
        let pinned = image();
        let other = TartImage::synthetic(b"ver09-substitute", mayfly_fake_registry::GuestOs::Linux);
        let digest = ImageDigest::new(pinned.digest()).expect("a digest");
        let registry = FakeRegistry::start(Images::new().publish(IMAGE_NAME, &other).serve_at(
            IMAGE_NAME,
            pinned.digest(),
            other.manifest(),
        ))
        .expect("a registry");
        let world = serving(&registry, &pinned, true);

        // when
        let (daemon, host) = pinned_run(&world);

        // then: the manifest was served (200), and refused for its bytes before any pull
        assert_refused_before_any_pull(&world, daemon, &host, &digest);
        let manifest = format!("/v2/{IMAGE_NAME}/manifests/{digest}");
        assert!(
            registry.served().contains(&Served {
                method: "GET".to_owned(),
                path: manifest,
                status: 200,
            }),
            "{:?}",
            registry.served()
        );
    }
}
