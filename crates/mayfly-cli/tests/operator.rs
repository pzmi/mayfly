//! `mayfly`'s operator commands end to end (RT-03): the built `mayflyd` over
//! `mayfly-fake-tart` in a tempdir world, and the built `mayfly` driven black-box through
//! `assert_cmd`. The world's repository is a loopback address nothing serves, so the pool's
//! placement fails at once, offline, and admits nothing: the state the goldens show is final.
//!
//! `--json` outputs are golden-tested (ver golden table, "CLI JSON"): each equals
//! `tests/fixtures/json/<command>.json` after [`normalised`], the one place that lists what
//! varies between runs (the host id, the build version).
//!
//! `mayfly-fake-tart` is found next to this test executable's directory, where a workspace
//! build puts it (`cargo build -p mayfly-fake-tart`).

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use assert_cmd::Command;

/// The longest the test waits for the ready line or a fact in the status.
const BOUND: Duration = Duration::from_secs(60);

const POOL: &str = "lin";

fn sibling(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("the test executable");
    let path = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>/deps/<test>")
        .join(name);
    assert!(
        path.is_file(),
        "{} is not built: `cargo build -p mayfly-fake-tart`",
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

/// A tempdir with a state directory, the fake `tart` on a `bin` path, a pool manifest and a
/// complete configuration naming them.
struct World {
    dir: tempfile::TempDir,
}

impl World {
    fn new() -> Self {
        let world = Self {
            dir: tempfile::tempdir().expect("tempdir"),
        };
        private_dir(&world.path("state"));
        private_dir(&world.path("state").join("tart"));
        private_dir(&world.path("bin"));
        std::os::unix::fs::symlink(sibling("tart"), world.path("bin").join("tart"))
            .expect("the fake tart on PATH");
        let repository = "127.0.0.1/mayfly/runner";
        std::fs::write(
            world.path("lin.toml"),
            format!(
                "schema = 1\nprovider = \"tart\"\nos = \"linux\"\narch = \"arm64\"\nsource = \"{repository}@sha256:{}\"\nrevision = 1\n\n[runner]\nversion = \"2.330.0\"\nsha256 = \"{}\"\n\n[launcher]\nsha256 = \"{}\"\n\n[capabilities]\nguest_agent = true\n",
                "a".repeat(64),
                "c".repeat(64),
                "d".repeat(64)
            ),
        )
        .expect("the manifest");
        std::fs::write(
            world.config(),
            format!(
                r#"schema_version = 1
state_dir = "{state}"

[github]
organisation = "example-org"
app_client_id = "Iv1.0123456789abcdef"
installation_id = 1
private_key = "env:MAYFLY_APP_KEY"
# loopback, where nothing serves: no test reaches GitHub (and the key never parses)
api_url = "http://127.0.0.1:9"

[host]
vm_slots = 2
macos_guests = 0
image_disk_gib = 50
clone_disk_gib = 60
reserve_memory_mib = 512
reserve_cpus = 1

[provider]
tart_home = "{state}/tart"

[images]
repository = "{repository}"

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
                state = world.path("state").display(),
                manifest = world.path("lin.toml").display(),
            ),
        )
        .expect("the config");
        world
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn config(&self) -> PathBuf {
        self.path("config.toml")
    }

    /// `mayfly --config <config> <args>`, with a cleared environment but `PATH` (for doctor).
    fn mayfly(&self, args: &[&str]) -> std::process::Output {
        Command::cargo_bin("mayfly")
            .expect("the mayfly binary")
            .env_clear()
            .env("PATH", self.path("bin"))
            .envs(std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v)))
            .arg("--config")
            .arg(self.config())
            .args(args)
            .output()
            .expect("run mayfly")
    }

    /// Starts `mayflyd` and waits for its ready line.
    fn start(&self) -> Daemon {
        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("mayflyd"))
            .env_clear()
            .env("PATH", self.path("bin"))
            .env("MAYFLY_APP_KEY", "a-test-secret-never-resolved")
            .envs(std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v)))
            .arg("--config")
            .arg(self.config())
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
        let line = received
            .recv_timeout(BOUND)
            .unwrap_or_else(|e| panic!("no ready line within {BOUND:?}: {e}"));
        assert!(line.starts_with("mayflyd ready "), "{line}");
        let daemon = Daemon { child };
        // the placement of the pool's pin fails offline; wait for that final state, the
        // documented `pools[].hosts[].placement`, so what the goldens show no longer moves
        mayfly_testkit_shared::poll::wait_until(BOUND, "the placement failed offline", || {
            let output = self.mayfly(&["status", "--json"]);
            String::from_utf8_lossy(&output.stdout).contains(r#""placement": "failed""#)
        });
        daemon
    }

    /// The host id the daemon generated.
    fn host(&self) -> String {
        std::fs::read_to_string(self.path("state").join("host-id"))
            .expect("the host id")
            .trim_end()
            .to_owned()
    }

    /// `text` with what varies between runs replaced by placeholders: the one list of the
    /// normalised fields (the host id, the build version).
    fn normalised(&self, text: &str) -> String {
        text.replace(&self.host(), "<host>")
            .replace(env!("CARGO_PKG_VERSION"), "<version>")
    }
}

/// A started `mayflyd`; killed when dropped (the world is thrown away with it).
struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn golden(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/json")
        .join(format!("{name}.json"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn should_answer_status_when_the_cli_is_given_the_daemons_config() {
    // given
    let world = World::new();
    let _daemon = world.start();

    // when
    let json = world.mayfly(&["status", "--json"]);
    let human = world.mayfly(&["status"]);

    // then
    assert_eq!(json.status.code(), Some(0), "{}", text(&json.stderr));
    assert_eq!(world.normalised(&text(&json.stdout)), golden("status"));
    assert_eq!(human.status.code(), Some(0), "{}", text(&human.stderr));
    let human = world.normalised(&text(&human.stdout));
    assert!(
        human.starts_with("host <host>  mayflyd <version>  run 1 epoch 1 operating ready\n"),
        "{human}"
    );
    assert!(
        human.contains("  on host <host>: terms open, placement failed, leases 0\n"),
        "{human}"
    );
}

#[test]
fn should_list_pools_and_vms_when_asked_for_json() {
    // given
    let world = World::new();
    let _daemon = world.start();

    // when
    let pools = world.mayfly(&["pool", "list", "--json"]);
    let vms = world.mayfly(&["vm", "list", "--json"]);
    let vms_human = world.mayfly(&["vm", "list"]);

    // then
    assert_eq!(pools.status.code(), Some(0), "{}", text(&pools.stderr));
    assert_eq!(world.normalised(&text(&pools.stdout)), golden("pool-list"));
    assert_eq!(vms.status.code(), Some(0), "{}", text(&vms.stderr));
    assert_eq!(world.normalised(&text(&vms.stdout)), golden("vm-list"));
    assert_eq!(text(&vms_human.stdout), "no VMs\n");
}

#[test]
fn should_pause_resume_and_answer_the_refusal_when_pool_commands_run() {
    // given
    let world = World::new();
    let _daemon = world.start();

    // when
    let paused = world.mayfly(&["pool", "pause", POOL]);
    let again = world.mayfly(&["pool", "pause", POOL]);
    let listed = world.mayfly(&["pool", "list"]);
    let resumed = world.mayfly(&["pool", "resume", POOL]);
    let reset = world.mayfly(&["pool", "reset-breaker", POOL]);
    let history = world.mayfly(&["events", "--since", "1h", "--json"]);

    // then: each journaled, a refusal named by the aggregate's code with exit 1
    assert_eq!(paused.status.code(), Some(0), "{}", text(&paused.stderr));
    assert_eq!(text(&paused.stdout), format!("pool {POOL} paused\n"));
    assert_eq!(again.status.code(), Some(1));
    assert_eq!(
        text(&again.stderr),
        "mayfly: already_paused: refused: already_paused\n"
    );
    assert!(
        text(&listed.stdout).starts_with(&format!("pool {POOL}: paused, image sha256:")),
        "{}",
        text(&listed.stdout)
    );
    assert_eq!(resumed.status.code(), Some(0), "{}", text(&resumed.stderr));
    assert_eq!(reset.status.code(), Some(1));
    assert!(
        text(&reset.stderr).starts_with("mayfly: not_tripped: "),
        "{}",
        text(&reset.stderr)
    );
    assert_eq!(history.status.code(), Some(0), "{}", text(&history.stderr));
    let kinds: Vec<String> = text(&history.stdout)
        .lines()
        .filter(|l| l.contains(&format!(r#""stream":"pool-{POOL}""#)))
        .filter_map(|l| {
            l.split(r#""kind":""#)
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .map(str::to_owned)
        })
        .collect();
    assert!(
        kinds.ends_with(&["pool_paused".to_owned(), "pool_resumed".to_owned()]),
        "{kinds:?}"
    );
}

#[test]
fn should_drain_and_resume_the_host_when_host_commands_run() {
    // given
    let world = World::new();
    let _daemon = world.start();

    // when
    let drained = world.mayfly(&["host", "drain", "--deadline", "30m"]);
    let status = world.mayfly(&["status"]);
    let resumed = world.mayfly(&["host", "resume"]);

    // then
    assert_eq!(drained.status.code(), Some(0), "{}", text(&drained.stderr));
    assert_eq!(text(&drained.stdout), "host draining\n");
    let status = world.normalised(&text(&status.stdout));
    assert!(
        status.contains("host <host>: admission closed, drain draining, leases 0\n")
            || status.contains("host <host>: admission closed, drain drained, leases 0\n"),
        "{status}"
    );
    assert_eq!(resumed.status.code(), Some(0), "{}", text(&resumed.stderr));
}

#[test]
fn should_report_every_check_when_doctor_runs_against_a_ready_daemon() {
    // given
    let world = World::new();
    let _daemon = world.start();

    // when
    let json = world.mayfly(&["doctor", "--json"]);
    let human = world.mayfly(&["doctor"]);

    // then
    assert_eq!(json.status.code(), Some(0), "{}", text(&json.stderr));
    assert_eq!(world.normalised(&text(&json.stdout)), golden("doctor"));
    assert!(
        text(&human.stdout).starts_with("ok   config: valid\nok   tart: found on PATH\n"),
        "{}",
        text(&human.stdout)
    );
}

#[test]
fn should_exit_69_when_no_daemon_answers() {
    // given: a configuration whose daemon is not running
    let world = World::new();

    // when
    let status = world.mayfly(&["status"]);
    let doctor = world.mayfly(&["doctor"]);
    let elsewhere = Command::cargo_bin("mayfly")
        .expect("the mayfly binary")
        .env_clear()
        .envs(std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v)))
        .args([
            "--socket",
            "/nonexistent/mayfly.sock",
            "pool",
            "pause",
            POOL,
        ])
        .output()
        .expect("run mayfly");

    // then
    assert_eq!(status.status.code(), Some(69));
    assert!(
        text(&status.stderr).starts_with("mayfly: cannot reach mayflyd at "),
        "{}",
        text(&status.stderr)
    );
    assert_eq!(doctor.status.code(), Some(1));
    assert!(
        text(&doctor.stdout).contains("FAIL daemon: cannot reach mayflyd at "),
        "{}",
        text(&doctor.stdout)
    );
    assert_eq!(elsewhere.status.code(), Some(69));
}

#[test]
fn should_validate_a_configuration_offline_when_config_validate_runs() {
    // given
    let world = World::new();
    std::fs::write(
        world.path("bad.toml"),
        "schema_version = 1\nmode = \"classic\"\n",
    )
    .expect("a bad config");

    // when
    let valid = world.mayfly(&["config", "validate", &world.config().display().to_string()]);
    let invalid = world.mayfly(&[
        "config",
        "validate",
        &world.path("bad.toml").display().to_string(),
    ]);
    let usage = world.mayfly(&["pool", "frobnicate"]);

    // then
    assert_eq!(valid.status.code(), Some(0), "{}", text(&valid.stderr));
    assert!(
        text(&valid.stdout).starts_with("configuration valid: "),
        "{}",
        text(&valid.stdout)
    );
    assert_eq!(invalid.status.code(), Some(78));
    assert!(!invalid.stderr.is_empty());
    assert_eq!(usage.status.code(), Some(64));
}
