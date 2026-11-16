//! The walking skeleton (E2E-01, plan §14 phase 3, ver §2's e2e level): the built `mayflyd`
//! and `mayfly`, `mayfly-fake-tart` first on the daemon's `PATH` with its fake guest runner,
//! `mayfly-fake-github` and `mayfly-fake-registry`, black-box. The e2e level, because the
//! promise is the shipped binaries' own: one job through the whole loop, the limit held across a
//! backlog, and a SIGKILL and restart of the process, observed only from outside it. Three runs:
//!
//! 1. a static demand of one prepared VM and one job: the job runs to its end, the VM is
//!    deleted and its slot released;
//! 2. a demand of three on two slots, three jobs run through it: never more than two VMs or
//!    leases at any point;
//! 3. `mayflyd` killed (SIGKILL) while a clone is in flight, then started again: the restart
//!    adopts the cloning attempt and its VM, and nothing has duplicated them by the time
//!    recovery completed (a snapshot; the test's own comment says what CP-04D adds).
//!
//! What the tests read is what an operator or GitHub could: the daemon's ready line (its
//! documented "recovery finished" signal), `mayfly status --json` and `mayfly events --since
//! --json` (RT-03; the journal's history, every stream), fake-tart's invocation log
//! (`$TART_HOME/invocations.jsonl`, documented append-only, its `effect` records written when a
//! command changed state and its `fault` record when an injected fault took effect) and `tart
//! list`, and the fake GitHub's own world. Nothing reads the journal's file. Every wait is a
//! bounded poll (`mayfly_testkit_shared::poll`) for a fact one of those sources documents, never
//! a log line or a sleep.
//!
//! The test plays GitHub's side through the fake's control methods: it hands a queued job to the
//! runner registered for a VM once that VM's runner is launched (`runner_launched`), and
//! completes the job once mayfly recorded the runner's exit (`runner_exited`), in the order a
//! real runner reports them. The fake guest runner runs its job for [`JOB_MS`] after its launch
//! (a fake-tart rule on the guest launcher's `exec`), then exits 0.
//!
//! The registry serves the pool's image over HTTPS under a CA made for the test, handed to
//! `mayflyd` as `SSL_CERT_FILE` (ver §7.5): Linux only, like the integration tests' placement
//! module (the trust roots of macOS are not read from that file). Each world is a tempdir; the
//! daemon's VMs, left running under their shims by design (ADR 0037), are stopped with `tart
//! stop` when the world is dropped and their shims waited for ([`Reaped`]).
//!
//! The binaries are found next to this test executable's directory, where a workspace build
//! puts them (`cargo build -p mayfly-cli -p mayfly-fake-tart` at least).
#![cfg(target_os = "linux")]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mayfly_fake_github::{FakeConfig, FakeGithub, FakeServer, JobSpec, Scope};
use mayfly_fake_registry::{FakeRegistry, GuestOs, Images, TartImage, seed_tart_home};
use mayfly_testkit_shared::poll;
use serde_json::Value;

/// The longest the fake GitHub's thread may take to report its port.
const BOUND: Duration = Duration::from_secs(60);

/// What one test may take in all, its cleanup included, within the test runner's own bound
/// (nextest's `ci` profile ends a test after four 30 s periods).
const WHOLE_TEST: Duration = Duration::from_secs(115);

/// The part of [`WHOLE_TEST`] kept for the cleanup (stopping the daemon, its VMs and a hung
/// clone), so a test whose waits ran out still cleans up before the runner kills it.
const CLEANUP: Duration = Duration::from_secs(20);

/// A test's time, from when its world was made: every wait takes what is left of it, so a
/// failing test ends inside [`WHOLE_TEST`] and leaks no process.
#[derive(Debug, Clone, Copy)]
struct Budget(Instant);

impl Budget {
    /// What is left for the test's waits: [`WHOLE_TEST`] less [`CLEANUP`].
    fn waits(self) -> Duration {
        WHOLE_TEST
            .saturating_sub(CLEANUP)
            .saturating_sub(self.0.elapsed())
    }

    /// What is left for the cleanup: the rest of [`WHOLE_TEST`].
    fn cleanup(self) -> Duration {
        WHOLE_TEST.saturating_sub(self.0.elapsed())
    }
}

/// How long the fake guest runner runs its job after its launch, then exits 0: long enough for
/// the test to hand it the job before it ends, which a poll does within milliseconds of the
/// launch's record. A runner that ended first would only leave its job to the next runner: each
/// test waits for its jobs to finish, not for a number of runners.
const JOB_MS: u64 = 5_000;

const POOL: &str = "lin";
const ORGANISATION: &str = "example-org";
const APP_CLIENT_ID: &str = "Iv1.0123456789abcdef";

/// The repository path the registry publishes the image under.
const IMAGE_NAME: &str = "mayfly/runner";

/// The guest launcher's path in a runner image (the guest contract of DP-05,
/// `mayfly_adapter_tart::guest::LAUNCHER`): the program a launch `exec`s, which fake-tart's rule
/// gives a job.
const LAUNCHER: &str = "/usr/local/libexec/mayfly/launcher";

/// What the world's installation is granted (GitHub's implied metadata, the self-hosted runners,
/// the run lookups and the fork check).
const GRANTED: &[(&str, &str)] = &[
    ("metadata", "read"),
    ("organization_self_hosted_runners", "write"),
    ("actions", "write"),
    ("organization_administration", "read"),
];

fn sibling(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("the test executable");
    let path = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>/deps/<test>")
        .join(name);
    assert!(
        path.is_file(),
        "{} is not built: `cargo build -p mayfly-cli -p mayfly-fake-tart`",
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

/// The coverage profile's location, passed through the cleared environments when a coverage
/// run set it, so the children's lines are counted.
fn profile_env() -> Option<(&'static str, std::ffi::OsString)> {
    std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v))
}

// ---- the fake GitHub ------------------------------------------------------------------------

/// `mayfly-fake-github` served on a loopback port by a thread of its own, its service URL its
/// own base (so `mayflyd` reaches every route over the socket), GHES's offer flow with a poll
/// held one second. Dropped, it stops the server and joins the thread: test support that owns a
/// live thread stops it.
struct Github {
    fake: FakeGithub,
    base: String,
    private_pem: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Github {
    fn start() -> Self {
        let keys = mayfly_fake_github::generate_app_key();
        let (public, private) = (keys.public_pem.clone(), keys.private_pem.clone());
        let knobs = mayfly_fake_github::Knobs {
            flow: mayfly_fake_github::Flow::Acquire,
            poll_hold: Duration::from_secs(1),
            ..mayfly_fake_github::Knobs::default()
        };
        let (made, received) = mpsc::channel();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("the fake's runtime");
            runtime.block_on(async move {
                let mut fake = None;
                let server = FakeServer::start_with(|base| {
                    let built = FakeGithub::with_knobs(
                        FakeConfig {
                            app_client_id: APP_CLIENT_ID.to_owned(),
                            app_public_key_pem: public,
                            installation_id: 1,
                            scope: Scope::Organization(ORGANISATION.to_owned()),
                            web_base: "https://github.com".to_owned(),
                            service_url: format!("{base}/service"),
                            admin_token_lifetime_secs: 3600,
                            admin_token_key_pem: private,
                            permissions: GRANTED
                                .iter()
                                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                                .collect(),
                        },
                        knobs,
                        std::sync::Arc::new(|| {
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                        }),
                    );
                    fake = Some(built.clone());
                    built
                })
                .await
                .expect("the fake's port");
                let _ = made.send((fake.expect("the fake"), server.base_url()));
                let _ = stopped.await;
                let _ = server.stop().await;
            });
        });
        let (fake, base) = received.recv_timeout(BOUND).expect("the fake serving");
        Self {
            fake,
            base,
            private_pem: keys.private_pem,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    /// Queues a job of the world's organisation for the pool's scale set (labelled with its
    /// name, ADR 0040); returns its id.
    fn queue_job(&self, run_id: u64) -> u64 {
        self.fake.queue_job(JobSpec {
            owner: ORGANISATION.to_owned(),
            run_id,
            labels: vec![POOL.to_owned()],
            ..JobSpec::default()
        })
    }

    /// The id of the runner registered under `name` (mayfly names a runner after its VM).
    fn runner_named(&self, name: &str) -> Option<u64> {
        self.fake
            .world()
            .runners
            .values()
            .find(|r| r.name == name)
            .map(|r| r.id)
    }

    /// Whether the fake holds job `job` finished with `result`.
    fn finished(&self, job: u64, result: &str) -> bool {
        self.fake.world().jobs.get(&job).is_some_and(|j| {
            matches!(&j.state, mayfly_fake_github::JobState::Finished { result: r } if r == result)
        })
    }

    /// Asserts that no secret of the fake's reached `out`: the App's private key, any token or
    /// JIT config the fake issued (ADR 0014).
    fn assert_absent_from(&self, out: &str, what: &str) {
        let key_body: String = self
            .private_pem
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .take(2)
            .collect();
        assert!(!key_body.is_empty());
        assert!(!out.contains(&key_body), "the App key in {what}");
        assert!(!out.contains("PRIVATE KEY"), "a PEM in {what}");
        for token in self.fake.issued_tokens() {
            assert!(!out.contains(&token), "a token in {what}");
        }
    }
}

impl Drop for Github {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ---- the world --------------------------------------------------------------------------------

/// The pool's demand and the host's slots.
struct Demand {
    vm_slots: u32,
    min_prepared: u32,
    max_running: u32,
}

/// A tempdir holding the state directory, fake-tart's `TART_HOME` in it, a `bin` with the fake
/// `tart`, the pool's manifest and the config; the registry serving the pool's pinned image
/// (seeded in fake-tart's registry too, so its pull and the data plane's verification agree);
/// the fake GitHub the config points at.
struct World {
    budget: Budget,
    dir: tempfile::TempDir,
    registry: FakeRegistry,
    github: Github,
}

impl World {
    /// A world for `demand`, with fake-tart's `faults.toml` holding the runner's job rule and
    /// `faults`.
    fn new(demand: &Demand, faults: &str) -> Self {
        let budget = Budget(Instant::now());
        let image = TartImage::synthetic(b"e2e01-runner-image", GuestOs::Linux);
        let registry =
            FakeRegistry::start(Images::new().publish(IMAGE_NAME, &image)).expect("a registry");
        let world = Self {
            budget,
            dir: tempfile::tempdir().expect("tempdir"),
            registry,
            github: Github::start(),
        };
        private_dir(&world.state());
        private_dir(&world.tart_home());
        private_dir(&world.path("bin"));
        std::os::unix::fs::symlink(sibling("tart"), world.path("bin").join("tart"))
            .expect("the fake tart on PATH");
        let repository = format!("{}/{IMAGE_NAME}", world.registry.host());
        seed_tart_home(&world.tart_home(), &repository, &image).expect("fake-tart's registry");
        std::fs::write(world.path("registry-ca.pem"), world.registry.ca_pem()).expect("the CA");
        std::fs::write(
            world.manifest(),
            format!(
                "schema = 1\nprovider = \"tart\"\nos = \"linux\"\narch = \"arm64\"\nsource = \"{repository}@{digest}\"\nrevision = 1\n\n[runner]\nversion = \"2.330.0\"\nsha256 = \"{runner}\"\n\n[launcher]\nsha256 = \"{launcher}\"\n\n[capabilities]\nguest_agent = true\n",
                digest = image.digest(),
                runner = "c".repeat(64),
                launcher = "d".repeat(64),
            ),
        )
        .expect("the manifest");
        // The host's sizing is small enough that the data plane's resource gate (I33), which
        // measures the machine's real free disk and memory, admits the pull, the base and the
        // clones on a CI runner (ver §7.5).
        std::fs::write(
            world.config(),
            format!(
                r#"schema_version = 1
state_dir = "{state}"

[github]
organisation = "{ORGANISATION}"
app_client_id = "{APP_CLIENT_ID}"
installation_id = 1
private_key = "env:MAYFLY_APP_KEY"
api_url = "{api}"

[host]
vm_slots = {slots}
macos_guests = 0
image_disk_gib = 1
clone_disk_gib = 1
reserve_disk_gib = 1
reserve_memory_mib = 512

[provider]
tart_home = "{tart_home}"

[images]
repository = "{repository}"

[[pools]]
name = "{POOL}"
tier = "trusted"
image_manifest = "{manifest}"
guest_os = "linux"
cpu = 2
memory_mib = 1024
min_prepared = {min_prepared}
max_running = {max_running}
"#,
                state = world.state().display(),
                api = world.github.base,
                slots = demand.vm_slots,
                tart_home = world.tart_home().display(),
                manifest = world.manifest().display(),
                min_prepared = demand.min_prepared,
                max_running = demand.max_running,
            ),
        )
        .expect("the config");
        std::fs::write(
            world.tart_home().join("faults.toml"),
            format!(
                "[[rule]]\nid = \"runner-job\"\ncommand = \"exec\"\narg0 = \"{LAUNCHER}\"\njob_ms = {JOB_MS}\n{faults}"
            ),
        )
        .expect("fake-tart's rules");
        world
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn state(&self) -> PathBuf {
        self.path("state")
    }

    fn tart_home(&self) -> PathBuf {
        self.state().join("tart")
    }

    fn config(&self) -> PathBuf {
        self.path("config.toml")
    }

    fn manifest(&self) -> PathBuf {
        self.path("lin.toml")
    }

    /// Where [`World::start`] writes the daemon's stderr.
    fn log(&self) -> PathBuf {
        self.path("mayflyd.stderr")
    }

    /// Starts `mayflyd` (the built binary) with a cleared environment: the fake `tart` on
    /// `PATH`, the App key behind the config's `env:` reference, the registry's CA as its trust,
    /// and waits for its ready line within the test's budget. Its stdout (ready lines only) goes
    /// to a channel through a reader thread the [`Daemon`] joins, its stderr to [`World::log`].
    fn start(&self) -> Daemon {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log())
            .expect("the daemon's log");
        let mut child = Command::new(assert_cmd::cargo::cargo_bin("mayflyd"))
            .env_clear()
            .env("PATH", self.path("bin"))
            .env("MAYFLY_APP_KEY", &self.github.private_pem)
            .env("SSL_CERT_FILE", self.path("registry-ca.pem"))
            .envs(profile_env())
            .arg("--config")
            .arg(self.config())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .expect("start mayflyd");
        let stdout = child.stdout.take().expect("stdout");
        let (lines, received) = mpsc::channel();
        let (done, finished) = mpsc::channel::<()>();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    break;
                }
            }
            let _ = done.send(());
        });
        let mut daemon = Daemon {
            child,
            received,
            reader: Some((reader, finished)),
            stdout: Vec::new(),
            budget: self.budget,
        };
        daemon.ready(self.budget.waits());
        daemon
    }

    /// `mayfly --config <config> <args>` (the built binary), with a cleared environment.
    fn mayfly(&self, args: &[&str]) -> std::process::Output {
        Command::new(assert_cmd::cargo::cargo_bin("mayfly"))
            .env_clear()
            .envs(profile_env())
            .arg("--config")
            .arg(self.config())
            .args(args)
            .output()
            .expect("run mayfly")
    }

    /// `mayfly status --json`, parsed; `Null` when it did not answer one (a poll tries again).
    fn status(&self) -> Value {
        let output = self.mayfly(&["status", "--json"]);
        if output.status.code() != Some(0) {
            return Value::Null;
        }
        serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
    }

    /// Every event the journal recorded in the last hour, every stream, as `mayfly events
    /// --since 1h --json` answers them (RT-03's `EventLine`s), in stream-revision order per
    /// stream. Empty when the daemon did not answer.
    fn events(&self) -> Events {
        let output = self.mayfly(&["events", "--since", "1h", "--json"]);
        let mut lines: Vec<Value> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|l| l["stream"].is_string() && l["revision"].is_u64())
            .collect();
        lines.sort_by(|a, b| {
            (a["stream"].as_str(), a["revision"].as_u64())
                .cmp(&(b["stream"].as_str(), b["revision"].as_u64()))
        });
        Events(lines)
    }

    /// The fake's invocation log, parsed line by line.
    fn invocations(&self) -> Vec<Value> {
        std::fs::read_to_string(self.tart_home().join("invocations.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    }

    /// The fake's `effect` records of `kind` on an `mf-*` VM (an attempt's, never the base's),
    /// in log order: the VMs' names.
    fn effects(&self, kind: &str) -> Vec<String> {
        self.invocations()
            .iter()
            .filter(|r| r["event"] == "effect" && r["kind"] == kind)
            .filter_map(|r| r["vm"].as_str().filter(|vm| vm.starts_with("mf-")))
            .map(str::to_owned)
            .collect()
    }

    /// The most `mf-*` VMs that existed at once in fake-tart, from its `vm_cloned` and
    /// `vm_deleted` effects in log order (each written when the command changed state).
    fn peak_vms(&self) -> usize {
        let mut live = BTreeSet::new();
        let mut peak = 0;
        for record in self.invocations() {
            let Some(vm) = record["vm"].as_str().filter(|vm| vm.starts_with("mf-")) else {
                continue;
            };
            match record["kind"].as_str() {
                Some("vm_cloned") => {
                    live.insert(vm.to_owned());
                }
                Some("vm_deleted") => {
                    live.remove(vm);
                }
                _ => {}
            }
            peak = peak.max(live.len());
        }
        peak
    }

    /// The `mf-*` VMs `tart list` shows now, as an operator's own `tart` would list them.
    fn vms(&self) -> BTreeSet<String> {
        let listed = Command::new(sibling("tart"))
            .env_clear()
            .env("TART_HOME", self.tart_home())
            .envs(profile_env())
            .args(["list", "--format", "json"])
            .output()
            .expect("run the fake tart");
        let listed: Value = serde_json::from_slice(&listed.stdout).expect("tart list json");
        listed
            .as_array()
            .expect("a list")
            .iter()
            .filter_map(|vm| vm["Name"].as_str())
            .filter(|name| name.starts_with("mf-"))
            .map(str::to_owned)
            .collect()
    }

    fn daemon_output(&self) -> String {
        std::fs::read_to_string(self.log()).unwrap_or_default()
    }

    /// Waits, by a bounded poll of `mayfly events`, until `done` holds of the journal's history;
    /// on a timeout names what the status and the history showed.
    fn await_events(
        &self,
        bound: Duration,
        what: &str,
        mut done: impl FnMut(&Events) -> bool,
    ) -> Events {
        let mut last = Events(Vec::new());
        let waited = poll::try_wait_until(bound, what, || {
            last = self.events();
            done(&last)
        });
        if let Err(timed_out) = waited {
            panic!(
                "{timed_out}; status: {}; attempts: {:?}",
                self.status(),
                last.attempts()
            );
        }
        last
    }
}

/// Stops every VM the daemon left running in the world's `TART_HOME` with `tart stop` and waits
/// for each shim to exit, when dropped (declared after the world and before the daemon, so
/// dropped after the daemon and before the world). A `tart run` that does not end is killed by
/// the pid its shim recorded, a process this test's daemon started.
struct Reaped<'w>(&'w World);

impl Drop for Reaped<'_> {
    fn drop(&mut self) {
        let tart = |args: &[&str]| {
            Command::new(sibling("tart"))
                .env_clear()
                .env("TART_HOME", self.0.tart_home())
                .envs(profile_env())
                .args(args)
                .output()
        };
        let entries: Vec<Value> = tart(&["list", "--format", "json"])
            .ok()
            .and_then(|out| serde_json::from_slice(&out.stdout).ok())
            .unwrap_or_default();
        for entry in entries.iter().filter(|e| e["Running"] == true) {
            if let Some(name) = entry["Name"].as_str() {
                let _stopped = tart(&["stop", name, "--timeout", "5"]);
            }
        }
        let Ok(dirs) = std::fs::read_dir(self.0.state().join("shims")) else {
            return;
        };
        for dir in dirs.filter_map(Result::ok).map(|e| e.path()) {
            // the shim's record names the `tart run` child it started
            let record = || -> Value {
                std::fs::read(dir.join("shim.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or(Value::Null)
            };
            let released = || shim_released(&dir);
            let bound = self.0.budget.cleanup();
            if poll::try_wait_until(bound, "a shim to exit after its VM stopped", released).is_ok()
            {
                continue;
            }
            if let Some(pid) = record()["child_pid"]
                .as_u64()
                .and_then(|p| i32::try_from(p).ok())
                .and_then(rustix::process::Pid::from_raw)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                let _ = poll::try_wait_until(
                    self.0.budget.cleanup(),
                    "a shim to exit after its run was killed",
                    || shim_released(&dir),
                );
            }
        }
    }
}

/// Whether the shim of `dir` let go of its lock (its process exited): a shim holds an exclusive
/// `flock` on `shim.lock` for as long as it lives (ADR 0037, `mayfly_adapter_tart::shim`), so a
/// non-blocking lock of ours succeeds only once it is gone (and is dropped at once).
fn shim_released(dir: &Path) -> bool {
    let Ok(file) = std::fs::File::open(dir.join("shim.lock")) else {
        return true;
    };
    file.try_lock_shared().is_ok()
}

/// A started `mayflyd`; killed if still running when dropped, and its stdout reader joined
/// once it exited. The reader ends at the pipe's end, which the daemon's exit closes because
/// no child of `mayflyd` inherits its stdout (every child it starts gets its own pipes or
/// `/dev/null`); the join still waits only for the reader's done signal, bounded by the budget.
struct Daemon {
    child: Child,
    received: mpsc::Receiver<String>,
    /// The stdout reader and its done signal, until joined.
    reader: Option<(std::thread::JoinHandle<()>, mpsc::Receiver<()>)>,
    /// Every stdout line read so far.
    stdout: Vec<String>,
    budget: Budget,
}

/// How a [`Daemon`] stopped on SIGTERM.
struct Stopped {
    code: Option<i32>,
    /// Everything it wrote to stdout.
    stdout: String,
}

impl Daemon {
    /// Waits up to `bound` for the next ready line, the daemon's documented "recovery finished"
    /// signal.
    fn ready(&mut self, bound: Duration) {
        let line = self
            .received
            .recv_timeout(bound)
            .unwrap_or_else(|e| panic!("no ready line within {bound:?}: {e}"));
        assert!(line.starts_with("mayflyd ready "), "{line}");
        assert!(line.ends_with(" mode=normal"), "{line}");
        self.stdout.push(line);
    }

    /// Joins the stdout reader of an exited daemon once it signalled its end, within the
    /// cleanup budget, and keeps what it read last. `Err` names a reader still blocked: some
    /// process holds the daemon's stdout open, against the invariant on [`Daemon`].
    fn join_reader(&mut self) -> Result<(), String> {
        if let Some((reader, finished)) = self.reader.take() {
            let bound = self.budget.cleanup();
            if finished.recv_timeout(bound).is_err() {
                return Err(format!(
                    "mayflyd's stdout still open {bound:?} after its exit: a process inherited it"
                ));
            }
            let _ = reader.join();
        }
        self.stdout.extend(self.received.try_iter());
        Ok(())
    }

    /// SIGKILL, then reaped: the crash the third test injects.
    fn kill(mut self) {
        self.child.kill().expect("SIGKILL");
        self.child.wait().expect("reaped");
        self.join_reader().unwrap_or_else(|why| panic!("{why}"));
    }

    /// SIGTERM to this test's own daemon, by its pid; its exit code and its stdout once it
    /// exited.
    fn terminate(mut self) -> Stopped {
        let pid = rustix::process::Pid::from_child(&self.child);
        rustix::process::kill_process(pid, rustix::process::Signal::TERM).expect("SIGTERM");
        let mut status = None;
        poll::wait_until(
            self.budget.cleanup(),
            "mayflyd to exit after SIGTERM",
            || {
                status = self.child.try_wait().expect("try_wait");
                status.is_some()
            },
        );
        self.join_reader().unwrap_or_else(|why| panic!("{why}"));
        Stopped {
            code: status.and_then(|s| s.code()),
            stdout: self.stdout.join("\n"),
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // never a panic in a drop (it may run while unwinding): a reader still blocked is left
        let _ = self.join_reader();
    }
}

/// The clone a killed `mayflyd` left behind, hung by fake-tart's `hang_after_effect` (which
/// never ends by itself): SIGKILLed by its pid, a process this test's daemon started, when
/// [`HungClone::end`] runs or the guard is dropped (a test that panicked first), then waited for
/// until it is gone or a zombie its new parent reaps, within the test's cleanup budget. Test
/// support that keeps a live child stops it.
struct HungClone(Option<rustix::process::Pid>, Budget);

impl HungClone {
    fn end(&mut self) {
        let Some(pid) = self.0.take() else {
            return;
        };
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        let stat = format!("/proc/{}/stat", pid.as_raw_nonzero());
        let bound = self.1.cleanup();
        let _ = poll::try_wait_until(bound, "the hung clone to end after SIGKILL", || {
            std::fs::read_to_string(&stat).map_or(true, |s| {
                s.rsplit_once(')')
                    .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
            })
        });
    }
}

impl Drop for HungClone {
    fn drop(&mut self) {
        self.end();
    }
}

// ---- the journal's history, as `mayfly events` shows it -------------------------------------

/// One attempt's events, from its stream.
#[derive(Debug, Default)]
struct AttemptHistory {
    /// Its VM, from `attempt_started`.
    vm: Option<String>,
    /// Its event kinds, in revision order.
    kinds: Vec<String>,
    /// The reason of its `attempt_retired`.
    retired: Option<String>,
}

impl AttemptHistory {
    fn has(&self, kind: &str) -> bool {
        self.kinds.iter().any(|k| k == kind)
    }
}

/// `mayfly events --since 1h --json`'s lines, ordered by stream and revision.
struct Events(Vec<Value>);

impl Events {
    /// Each attempt's history, by attempt id.
    fn attempts(&self) -> BTreeMap<String, AttemptHistory> {
        let mut attempts: BTreeMap<String, AttemptHistory> = BTreeMap::new();
        for line in &self.0 {
            let Some(id) = line["stream"]
                .as_str()
                .and_then(|s| s.strip_prefix("attempt-"))
            else {
                continue;
            };
            let entry = attempts.entry(id.to_owned()).or_default();
            let kind = line["kind"].as_str().unwrap_or_default();
            match kind {
                "attempt_started" => {
                    entry.vm = line["payload"]["vm_name"].as_str().map(str::to_owned);
                }
                "attempt_retired" => {
                    entry.retired = line["payload"]["reason"].as_str().map(str::to_owned);
                }
                _ => {}
            }
            entry.kinds.push(kind.to_owned());
        }
        attempts
    }

    /// The host stream's slot events in revision order, as `(kind, attempt id)`.
    fn slots(&self) -> Vec<(String, String)> {
        self.0
            .iter()
            .filter(|l| l["stream"].as_str().is_some_and(|s| s.starts_with("host-")))
            .filter(|l| matches!(l["kind"].as_str(), Some("slot_reserved" | "slot_released")))
            .map(|l| {
                (
                    l["kind"].as_str().unwrap_or_default().to_owned(),
                    l["payload"]["attempt_id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                )
            })
            .collect()
    }

    /// The most leases the host held at once, replaying its stream in order.
    fn peak_leases(&self) -> usize {
        let mut held = BTreeSet::new();
        let mut peak = 0;
        for (kind, attempt) in self.slots() {
            if kind == "slot_reserved" {
                held.insert(attempt);
            } else {
                held.remove(&attempt);
            }
            peak = peak.max(held.len());
        }
        peak
    }

    /// Whether the host released the lease of `attempt`.
    fn released(&self, attempt: &str) -> bool {
        self.slots()
            .iter()
            .any(|(kind, a)| kind == "slot_released" && a == attempt)
    }

    /// The kinds of the host runs' events, in order.
    fn host_run_kinds(&self) -> Vec<String> {
        self.0
            .iter()
            .filter(|l| {
                l["stream"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("hostrun-"))
            })
            .filter_map(|l| l["kind"].as_str().map(str::to_owned))
            .collect()
    }
}

/// Plays GitHub's side for every launched runner, once per call: hands a waiting job to each
/// runner whose launch mayfly recorded and that holds none, and completes the job of each runner
/// whose exit mayfly recorded, once (`completed` holds the jobs completed so far).
fn play_github(world: &World, events: &Events, jobs: &[u64], completed: &mut BTreeSet<u64>) {
    let fake = &world.github.fake;
    for history in events.attempts().values() {
        let Some(vm) = history.vm.as_deref() else {
            continue;
        };
        let Some(runner) = world.github.runner_named(vm) else {
            continue;
        };
        let holding = fake.world().runners.get(&runner).and_then(|r| r.job);
        match holding {
            None if history.has("runner_launched") && !history.has("runner_exited") => {
                // the fake refuses a job not yet assigned to the scale set: the next call retries
                for job in jobs.iter().filter(|j| !completed.contains(j)) {
                    if fake.start_job(*job, runner).is_ok() {
                        break;
                    }
                }
            }
            Some(job) if history.has("runner_exited") && completed.insert(job) => {
                fake.complete_job(job, "succeeded")
                    .expect("the job completes");
            }
            _ => {}
        }
    }
}

#[test]
fn should_run_one_job_delete_its_vm_and_release_its_slot_when_the_static_demand_is_one() {
    // given: one VM prepared ahead of demand on a host of two slots; one job queued at GitHub
    // for the pool, which the listener acquires and the reconciler grants a VM for
    let world = World::new(
        &Demand {
            vm_slots: 2,
            min_prepared: 1,
            max_running: 2,
        },
        "",
    );
    let reaped = Reaped(&world);
    let job = world.github.queue_job(9);
    let daemon = world.start();

    // when: the runner is launched in its VM and takes the job, its job ends in the guest (the
    // exit recorded by the data plane's status read), and GitHub completes the job; then the
    // attempt's VM is deleted and its slot released
    let mut completed = BTreeSet::new();
    let events = world.await_events(
        world.budget.waits(),
        "the job's VM deleted and its slot released, in mayfly events",
        |events| {
            play_github(&world, events, &[job], &mut completed);
            completed.contains(&job)
                && events.attempts().iter().any(|(id, h)| {
                    h.has("runner_exited") && h.has("vm_deleted") && events.released(id)
                })
        },
    );
    let status = world.status();
    let vms = world.vms();
    let stopped = daemon.terminate();
    drop(reaped);

    // then: one job ran, on one runner minted once; its attempt went the whole way, retired
    // `job_done`; its VM deleted through Tart and gone; its slot released, which the pool
    // refilled within the two slots
    assert_eq!(stopped.code, Some(0), "SIGTERM is a clean stop");
    assert!(world.github.finished(job, "succeeded"), "the job finished");
    assert_eq!(
        world.github.fake.counts().jit_configs,
        1,
        "minted once (I19)"
    );
    let attempts = events.attempts();
    let ran: Vec<_> = attempts
        .iter()
        .filter(|(_, h)| h.has("runner_launched"))
        .collect();
    assert_eq!(ran.len(), 1, "one runner launched: {attempts:?}");
    let (attempt, history) = ran[0];
    for kind in [
        "attempt_started",
        "attempt_prepared",
        "runner_launched",
        "runner_exited",
        "attempt_retired",
        "vm_deleted",
    ] {
        assert!(history.has(kind), "{kind}: {history:?}");
    }
    assert_eq!(history.retired.as_deref(), Some("job_done"), "{history:?}");
    let vm = history.vm.clone().expect("the attempt's VM");
    assert!(world.effects("vm_deleted").contains(&vm), "deleted by tart");
    assert!(!vms.contains(&vm), "{vm} still listed: {vms:?}");
    assert!(events.released(attempt), "the slot released");
    assert!(
        world.github.runner_named(&vm).is_none(),
        "GitHub removed the runner that ran the job"
    );
    assert!(events.peak_leases() <= 2 && world.peak_vms() <= 2);
    assert!(
        status["attempts"]["unknown_effects"] == 0,
        "no effect left unknown: {status}"
    );
    world
        .github
        .assert_absent_from(&world.daemon_output(), "mayflyd's stderr");
    world
        .github
        .assert_absent_from(&stopped.stdout, "mayflyd's stdout");
    let tart_log = std::fs::read_to_string(world.tart_home().join("invocations.jsonl"))
        .expect("the fake's log");
    world.github.assert_absent_from(&tart_log, "the tart log");
}

#[test]
fn should_never_hold_more_than_two_vms_when_three_are_wanted_on_two_slots() {
    // given: a pool that wants three VMs prepared (and may run three) on a host of two slots;
    // three jobs queued at GitHub for it
    let world = World::new(
        &Demand {
            vm_slots: 2,
            min_prepared: 3,
            max_running: 3,
        },
        "",
    );
    let reaped = Reaped(&world);
    let jobs: Vec<u64> = (21..=23).map(|run| world.github.queue_job(run)).collect();
    let daemon = world.start();

    // when: the three jobs run through the two slots, each on a VM that is deleted afterwards
    // and replaced
    let mut completed = BTreeSet::new();
    let events = world.await_events(
        world.budget.waits(),
        "the three jobs completed and their VMs deleted, in mayfly events",
        |events| {
            play_github(&world, events, &jobs, &mut completed);
            let attempts = events.attempts();
            let done = attempts
                .iter()
                .filter(|(id, h)| {
                    h.has("runner_exited") && h.has("vm_deleted") && events.released(id)
                })
                .count();
            completed.len() == jobs.len() && done >= jobs.len()
        },
    );
    let stopped = daemon.terminate();
    drop(reaped);

    // then: every job ran; more attempts started than there are slots, yet the host never held
    // more than two leases, and fake-tart never held more than two of the pool's VMs at once
    assert_eq!(stopped.code, Some(0), "SIGTERM is a clean stop");
    for job in &jobs {
        assert!(world.github.finished(*job, "succeeded"), "job {job}");
    }
    let attempts = events.attempts();
    let started_vms: Vec<_> = attempts.values().filter_map(|h| h.vm.clone()).collect();
    assert!(
        started_vms.len() > 2,
        "a third VM started once a slot was released: {attempts:?}"
    );
    assert_eq!(
        events.peak_leases(),
        2,
        "the host held both slots, never more: {:?}",
        events.slots()
    );
    assert_eq!(
        world.peak_vms(),
        2,
        "fake-tart held two of the pool's VMs at most: {:?}",
        world.invocations()
    );
    let unique: BTreeSet<_> = started_vms.iter().collect();
    assert_eq!(unique.len(), started_vms.len(), "a VM name per attempt");
}

/// The crash of Accept 3, as far as the product reaches today. It pins what the restart's
/// recovery does: the killed run recorded unclean, recovery completed, the cloning attempt
/// adopted under the new run with its VM kept, and nothing duplicated **by the time recovery
/// completed** (one attempt, one lease, one VM, its name cloned once and never deleted). It is a
/// snapshot at that point, not a steady state: the reconciler's first pass after admission
/// reopens may run after it, and nothing later can be waited for yet, because the adopted
/// attempt stays `cloning` until its clone directive's deadline (ten minutes from its first
/// send, `DirectiveTimeouts::clone`): nothing resolves a directive merely in flight at
/// adoption. CP-04D (doc/tasks.md) closes that gap; this test then waits for `prepared` and
/// asserts one lease, one VM and one clone at that steady state.
#[test]
fn should_adopt_the_clone_without_a_duplicate_when_mayflyd_is_killed_mid_clone_and_restarted() {
    // given: one VM prepared ahead of demand, and the first attempt's clone made to hang once
    // it has cloned (fake-tart's `hang_after_effect`; the base's clone of the pulled image is
    // the first clone and passes): the VM exists, the clone never answers
    let world = World::new(
        &Demand {
            vm_slots: 2,
            min_prepared: 1,
            max_running: 2,
        },
        "\n[[rule]]\nid = \"attempt-clone-hangs\"\ncommand = \"clone\"\nskip = 1\ntimes = 1\naction = \"hang_after_effect\"\n",
    );
    let reaped = Reaped(&world);
    let first = world.start();
    let mut held = None;
    poll::wait_until(
        world.budget.waits(),
        "the attempt's clone held, in fake-tart's invocation log",
        || {
            let log = world.invocations();
            held = log
                .iter()
                .find(|r| r["event"] == "fault" && r["rule"] == "attempt-clone-hangs")
                .and_then(|fault| {
                    log.iter()
                        .find(|r| r["event"] == "invoke" && r["inv"] == fault["inv"])
                })
                .cloned();
            held.is_some()
        },
    );
    let held = held.expect("the held clone");
    let mut hung = HungClone(
        held["pid"]
            .as_u64()
            .and_then(|p| i32::try_from(p).ok())
            .and_then(rustix::process::Pid::from_raw),
        world.budget,
    );
    assert!(hung.0.is_some(), "the clone's pid: {held}");
    let argv: Vec<String> = held["argv"]
        .as_array()
        .expect("argv")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let hung_vm = argv.get(2).cloned().unwrap_or_default();

    // when: mayflyd is killed with the clone in flight; the clone its run started then ends
    // without an answer, and mayflyd starts again and recovers (its ready line)
    first.kill();
    // The test ends the orphaned clone itself because the fault hangs forever. In production a
    // `tart clone` left by a killed mayflyd keeps running in its own process group, nothing
    // reaps it, and a retried clone of the same name may race it (a caveat CP-04D records).
    hung.end();
    let second = world.start();
    let events = world.await_events(
        world.budget.waits(),
        "the cloning attempt adopted by the second run's recovery, in mayfly events",
        |events| {
            events
                .host_run_kinds()
                .iter()
                .filter(|k| *k == "recovery_completed")
                .count()
                == 2
                && events
                    .attempts()
                    .values()
                    .any(|h| h.vm.as_deref() == Some(hung_vm.as_str()) && h.has("attempt_adopted"))
        },
    );
    // A snapshot as recovery completed (see the doc above). With CP-04D this waits instead for
    // the adopted attempt `prepared`, then asserts one lease, one VM and one clone there.
    let status = world.status();
    let vms = world.vms();
    let stopped = second.terminate();
    drop(reaped);

    // then: the held clone was the attempt's, from the base; the killed run is recorded unclean
    // and the restart recovered; the attempt is adopted with its VM kept, and nothing had
    // duplicated it by then: one attempt, one lease, one VM, its name cloned once, never deleted
    assert_eq!(stopped.code, Some(0), "SIGTERM is a clean stop");
    assert_eq!(argv.first().map(String::as_str), Some("clone"), "{argv:?}");
    assert!(
        argv.get(1).is_some_and(|base| base.starts_with("mfb-")),
        "{argv:?}"
    );
    assert!(hung_vm.starts_with("mf-"), "{argv:?}");
    let kinds = events.host_run_kinds();
    assert!(
        kinds.iter().any(|k| k == "previous_run_ended_unclean"),
        "{kinds:?}"
    );
    let attempts = events.attempts();
    let started_vms: Vec<_> = attempts.values().filter_map(|h| h.vm.clone()).collect();
    assert_eq!(started_vms, std::slice::from_ref(&hung_vm), "{attempts:?}");
    assert_eq!(world.effects("vm_cloned"), std::slice::from_ref(&hung_vm));
    assert!(world.effects("vm_deleted").is_empty());
    assert_eq!(vms, BTreeSet::from([hung_vm]));
    assert_eq!(status["hosts"][0]["leases"], 1, "{status}");
    assert_eq!(events.peak_leases(), 1, "{:?}", events.slots());
}
