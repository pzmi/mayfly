//! The emulated guest launcher (`src/guest.rs`) and the run lock a foreground `run` holds.

use rustix::fs::{Mode, OFlags};
use rustix::process::{Flock, FlockType, fcntl_getlk};
use serde_json::Value;

use crate::support::Fake;

const LAUNCHER: &str = "/usr/local/libexec/mayfly/launcher";

/// The pid holding `vm`'s run lock (`vms/<vm>/config.json`), as `fcntl(F_GETLK)` reports it.
fn lock_holder(fake: &Fake, vm: &str) -> Option<i32> {
    let path = fake.home().join("vms").join(vm).join("config.json");
    let fd = rustix::fs::open(&path, OFlags::RDONLY, Mode::empty()).ok()?;
    let held = fcntl_getlk(&fd, &Flock::from(FlockType::WriteLock)).ok()??;
    held.pid
        .map(rustix::process::Pid::as_raw_nonzero)
        .map(i32::from)
}

fn running(fake: &Fake, names: &[&str]) {
    fake.detached();
    assert_eq!(fake.tart(&["create", "base"]).code(), Some(0));
    for name in names {
        assert_eq!(fake.tart(&["clone", "base", name]).code(), Some(0));
        assert_eq!(fake.tart(&["run", name]).code(), Some(0));
    }
}

fn json(text: &str) -> Value {
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{e}: {text}"))
}

#[test]
fn should_hold_the_run_lock_in_the_run_process_when_a_vm_runs_in_the_foreground() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    let mut runner = fake.spawn(&["run", "vm1"]);
    assert_eq!(fake.tart(&["ip", "vm1", "--wait", "30"]).code(), Some(0));

    // when
    let held = lock_holder(&fake, "vm1");
    let stop = fake.tart(&["stop", "vm1"]);
    runner.wait().expect("reap run");
    let after = lock_holder(&fake, "vm1");

    // then
    assert_eq!(held, i32::try_from(runner.id()).ok());
    assert_eq!(stop.code(), Some(0));
    assert_eq!(after, None, "the lock dies with the run process");
}

#[test]
fn should_answer_the_launcher_contract_when_the_guest_is_verified_launched_and_read() {
    // given
    let fake = Fake::new();
    running(&fake, &["vm1"]);
    let mac = json(&fake.tart(&["get", "vm1", "--format", "json"]).stdout())["MACAddress"].clone();
    let mac_text = mac.as_str().expect("a MAC").to_ascii_lowercase();
    let hello = format!("n1 {mac_text}\ncfg");
    let launch = |stdin: &[u8]| fake.tart_with_stdin(&["exec", "-i", "vm1", LAUNCHER], stdin);

    // when
    let verified = fake.tart_with_stdin(&["exec", "-i", "vm1", LAUNCHER, "verify"], b"00ff\n");
    let before = fake.tart(&["exec", "vm1", LAUNCHER, "status"]);
    let wrong = launch(b"n0 0a:0b:0c:0d:0e:0f\ncfg");
    let empty = launch(format!("n2 {mac_text}\n").as_bytes());
    let launched = launch(hello.as_bytes());
    let again = launch(hello.as_bytes());
    let after = fake.tart(&["exec", "vm1", LAUNCHER, "status"]);

    // then
    let claims = json(&verified.stdout());
    assert_eq!(claims["nonce"], "00ff");
    assert_eq!(claims["mac"], mac);
    assert_eq!(claims["os"], "macos");
    assert_eq!(json(&before.stdout())["state"], "none");
    assert_eq!(
        (wrong.code(), wrong.stdout()),
        (Some(5), format!("mismatch n0 {mac_text}\n"))
    );
    assert_eq!(empty.code(), Some(4), "an empty config starts nothing");
    assert_eq!(launched.code(), Some(0), "{}", launched.stderr());
    assert_eq!(launched.stdout(), format!("ok n1 {mac_text}\n"));
    assert_eq!(again.code(), Some(3), "a second launch is refused");
    assert_eq!(again.stdout(), format!("launched n1 {mac_text}\n"));
    assert_eq!(json(&after.stdout())["state"], "running");
    let lens: Vec<Value> = fake
        .events("stdin")
        .iter()
        .map(|e| e["len"].clone())
        .collect();
    assert_eq!(
        lens,
        [Value::from(0), Value::from(3)],
        "each config read logs its length"
    );
    assert!(
        !std::fs::read_to_string(fake.home().join("invocations.jsonl"))
            .expect("log")
            .contains("\"cfg\"")
    );
}

#[test]
fn should_end_the_runner_with_its_rule_exit_code_when_the_launch_rule_sets_a_job() {
    // given
    let fake = Fake::new();
    running(&fake, &["vm1"]);
    fake.faults(&format!(
        "run_mode = \"detached\"\n[[rule]]\ncommand = \"exec\"\narg0 = \"{LAUNCHER}\"\njob_exit_code = 5\n"
    ));
    let mac = json(&fake.tart(&["get", "vm1", "--format", "json"]).stdout())["MACAddress"].clone();
    let hello = format!("n1 {}\ncfg", mac.as_str().expect("a MAC"));

    // when
    let launched = fake.tart_with_stdin(&["exec", "-i", "vm1", LAUNCHER], hello.as_bytes());
    let status = fake.tart(&["exec", "vm1", LAUNCHER, "status"]);

    // then
    assert_eq!(launched.code(), Some(0));
    let status = json(&status.stdout());
    assert_eq!(
        (status["state"].clone(), status["code"].clone()),
        ("exited".into(), 5.into())
    );
}

#[test]
fn should_echo_the_config_on_stderr_when_the_launch_rule_asks_for_it() {
    // given
    let fake = Fake::new();
    running(&fake, &["vm1"]);
    fake.faults(&format!(
        "run_mode = \"detached\"\n[[rule]]\ncommand = \"exec\"\narg0 = \"{LAUNCHER}\"\necho_stdin = true\naction = \"fail_after_effect\"\n"
    ));
    let mac = json(&fake.tart(&["get", "vm1", "--format", "json"]).stdout())["MACAddress"].clone();
    let hello = format!("n1 {}\nsecret-cfg", mac.as_str().expect("a MAC"));

    // when
    let launched = fake.tart_with_stdin(&["exec", "-i", "vm1", LAUNCHER], hello.as_bytes());

    // then
    assert_eq!(launched.code(), Some(1));
    assert!(
        launched.stderr().starts_with("secret-cfg"),
        "{}",
        launched.stderr()
    );
}

#[test]
fn should_answer_from_the_other_guest_when_the_exec_is_routed_elsewhere() {
    // given
    let fake = Fake::new();
    running(&fake, &["vm1", "vm2"]);
    let mac2 = json(&fake.tart(&["get", "vm2", "--format", "json"]).stdout())["MACAddress"].clone();
    fake.faults(
        "run_mode = \"detached\"\n[[rule]]\ncommand = \"exec\"\nvm = \"vm1\"\nroute_to = \"vm2\"\n",
    );
    std::fs::create_dir_all(fake.home().join("guest-logs")).expect("logs dir");
    std::fs::write(
        fake.home().join("guest-logs").join("vm2.log"),
        "runner of vm2\n",
    )
    .expect("log");

    // when
    let verified = fake.tart_with_stdin(&["exec", "-i", "vm1", LAUNCHER, "verify"], b"01");
    let diagnostics = fake.tart(&["exec", "vm1", LAUNCHER, "diagnostics"]);

    // then
    assert_eq!(json(&verified.stdout())["mac"], mac2);
    assert_eq!(diagnostics.stdout(), "runner of vm2\n");
}

#[test]
fn should_refuse_route_to_when_the_rule_is_not_an_exec() {
    // given
    let fake = Fake::new();
    fake.faults("[[rule]]\ncommand = \"stop\"\nroute_to = \"vm2\"\n");

    // when
    let ran = fake.tart(&["--version"]);

    // then
    assert_eq!(ran.code(), Some(99), "{}", ran.stderr());
}

#[test]
fn should_record_the_runner_when_install_runner_is_given_a_version_and_its_digest() {
    // given
    let fake = Fake::new();
    running(&fake, &["vm1"]);
    let sha = "0f".repeat(32);

    // when
    let installed = fake.tart(&["exec", "vm1", LAUNCHER, "install-runner", "2.330.0", &sha]);
    let malformed = fake.tart(&["exec", "vm1", LAUNCHER, "install-runner", "latest", &sha]);
    let short = fake.tart(&["exec", "vm1", LAUNCHER, "install-runner", "2.330.0", "0f"]);

    // then
    assert_eq!(installed.code(), Some(0));
    assert_eq!(
        json(&installed.stdout()),
        serde_json::json!({"v": 1, "runner_version": "2.330.0"})
    );
    assert_eq!(malformed.code(), Some(64));
    assert_eq!(short.code(), Some(64));
    assert_eq!(
        fake.vm("vm1").expect("the vm")["installed_runner"],
        "2.330.0"
    );
}
