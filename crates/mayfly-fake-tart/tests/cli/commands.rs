//! The command surface: what each Tart command does to the state and prints.

use std::io::{BufRead, BufReader, Read};

use serde_json::{Value, json};

use crate::support::Fake;

fn fixture(text: &str) -> Value {
    serde_json::from_str(text).expect("fixture is json")
}

fn normalise(
    mut value: Value,
    key: &str,
    placeholder: &str,
    shape: impl Fn(&str) -> bool,
) -> Value {
    let items: Vec<&mut Value> = match &mut value {
        Value::Array(items) => items.iter_mut().collect(),
        other => vec![other],
    };
    for item in items {
        let text = item[key].as_str().expect("string field").to_owned();
        assert!(shape(&text), "{key} has an unexpected shape: {text}");
        item[key] = json!(placeholder);
    }
    value
}

fn iso8601(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 20
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && b[19] == b'Z'
}

fn mac(text: &str) -> bool {
    let parts: Vec<&str> = text.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && u8::from_str_radix(p, 16).is_ok())
}

#[test]
fn should_refuse_to_run_when_tart_home_is_not_set() {
    // given
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_tart"))
        .arg("--version")
        .env_remove("TART_HOME")
        .output()
        .expect("run tart");

    // when
    let stderr = String::from_utf8_lossy(&status.stderr);

    // then
    assert_eq!(status.status.code(), Some(99));
    assert!(stderr.contains("TART_HOME is not set"), "{stderr}");
}

#[test]
fn should_refuse_a_home_that_looks_real_when_it_has_no_marker() {
    // given
    let fake = Fake::new();
    std::fs::create_dir_all(fake.home().join("vms").join("some-vm")).unwrap();

    // when
    let ran = fake.tart(&["list"]);

    // then
    assert_eq!(ran.code(), Some(99));
    assert!(ran.stderr().contains("real Tart home"), "{}", ran.stderr());
    assert!(!fake.home().join("fake-tart.marker").exists());
}

#[test]
fn should_print_the_bare_version_when_asked() {
    // given
    let fake = Fake::new();

    // when
    let ran = fake.tart(&["--version"]);

    // then
    assert_eq!((ran.code(), ran.stdout().as_str()), (Some(0), "2.41.1\n"));
}

#[test]
fn should_refuse_with_usage_exit_when_the_arguments_are_not_tarts() {
    // given
    let fake = Fake::new();

    // when
    let unknown_flag = fake.tart(&["run", "--detach", "vm1"]);
    let unknown_command = fake.tart(&["reboot"]);

    // then
    assert_eq!(unknown_flag.code(), Some(64));
    assert!(
        unknown_flag
            .stderr()
            .starts_with("Error: Unknown option '--detach'")
    );
    assert_eq!(unknown_command.code(), Some(64));
}

#[test]
fn should_print_the_pinned_list_shape_when_listing_json() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    assert_eq!(fake.tart(&["pull", "ghcr.io/acme/img:1"]).code(), Some(0));

    // when
    let ran = fake.tart(&["list", "--format", "json"]);

    // then
    let actual = normalise(
        serde_json::from_str(&ran.stdout()).unwrap(),
        "Accessed",
        "<time>",
        iso8601,
    );
    let pinned = fixture(include_str!("../fixtures/tart-list-2.41.1.json"));
    assert_eq!(ran.code(), Some(0));
    assert_eq!(actual, pinned);
}

#[test]
fn should_print_the_pinned_get_shape_when_getting_json() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");

    // when
    let ran = fake.tart(&["get", "vm1", "--format", "json"]);

    // then
    let actual = normalise(
        serde_json::from_str(&ran.stdout()).unwrap(),
        "MACAddress",
        "<mac>",
        mac,
    );
    let pinned = fixture(include_str!("../fixtures/tart-get-2.41.1.json"));
    assert_eq!(ran.code(), Some(0));
    assert_eq!(actual, pinned);
}

#[test]
fn should_list_quietly_in_text_and_by_source_when_asked() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    fake.tart(&["pull", "ghcr.io/acme/img:1"]);

    // when
    let quiet = fake.tart(&["list", "-q"]);
    let oci = fake.tart(&["list", "--source", "oci", "-q"]);
    let text = fake.tart(&["list"]);
    let get_text = fake.tart(&["get", "vm1"]);

    // then
    assert_eq!(quiet.stdout(), "base\nvm1\nghcr.io/acme/img:1\n");
    assert_eq!(oci.stdout(), "ghcr.io/acme/img:1\n");
    assert!(
        text.stdout().starts_with("Source  Name"),
        "{}",
        text.stdout()
    );
    assert!(
        get_text.stdout().contains("1024x768"),
        "{}",
        get_text.stdout()
    );
}

#[test]
fn should_give_a_clone_its_own_mac_when_cloned_from_a_local_vm() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    fake.tart(&["clone", "base", "vm2"]);

    // when
    let macs: Vec<String> = ["base", "vm1", "vm2"]
        .iter()
        .map(|n| fake.vm(n).unwrap()["mac"].as_str().unwrap().to_owned())
        .collect();

    // then
    assert!(macs.iter().all(|m| mac(m)));
    assert_eq!(
        macs.iter().collect::<std::collections::BTreeSet<_>>().len(),
        3
    );
}

#[test]
fn should_keep_the_image_mac_when_it_is_the_first_clone_of_an_oci_image() {
    // given
    let fake = Fake::new();
    fake.tart(&["pull", "ghcr.io/acme/img:1"]);
    let image_mac = fake.vm("ghcr.io%2Facme%2Fimg%3A1").unwrap()["mac"].clone();

    // when
    fake.tart(&["clone", "ghcr.io/acme/img:1", "first"]);
    fake.tart(&["clone", "ghcr.io/acme/img:1", "second"]);

    // then
    assert_eq!(fake.vm("first").unwrap()["mac"], image_mac);
    assert_ne!(fake.vm("second").unwrap()["mac"], image_mac);
}

#[test]
fn should_refuse_a_clone_when_the_source_is_missing_or_the_name_is_taken() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");

    // when
    let missing = fake.tart(&["clone", "nothing", "vm2"]);
    let taken = fake.tart(&["clone", "base", "vm1"]);
    let overwritten = fake.tart(&["clone", "base", "vm1", "--overwrite"]);
    let create_again = fake.tart(&["create", "base"]);

    // then
    assert_eq!(missing.code(), Some(2));
    assert_eq!(
        missing.stderr(),
        "the specified VM \"nothing\" does not exist\n"
    );
    assert_eq!(taken.code(), Some(64));
    assert_eq!(
        taken.stderr(),
        "Error: VM \"vm1\" already exists, use --overwrite to replace it\n"
    );
    assert_eq!(overwritten.code(), Some(0));
    assert_eq!(create_again.code(), Some(1));
}

#[test]
fn should_refuse_vm_commands_when_the_vm_does_not_exist() {
    // given
    let fake = Fake::new();

    // when
    let results: Vec<_> = [
        vec!["run", "ghost"],
        vec!["ip", "ghost"],
        vec!["get", "ghost"],
        vec!["stop", "ghost"],
        vec!["delete", "ghost"],
        vec!["exec", "ghost", "true"],
    ]
    .iter()
    .map(|args| fake.tart(args))
    .collect();

    // then
    for ran in results {
        assert_eq!(ran.code(), Some(2));
        assert_eq!(ran.stderr(), "the specified VM \"ghost\" does not exist\n");
    }
}

#[test]
fn should_not_treat_an_oci_cache_entry_as_a_local_vm_when_running_or_getting_it() {
    // given
    let fake = Fake::new();
    fake.tart(&["pull", "ghcr.io/acme/img:1"]);

    // when
    let run = fake.tart(&["run", "ghcr.io/acme/img:1"]);
    let delete = fake.tart(&["delete", "ghcr.io/acme/img:1"]);

    // then
    assert_eq!(run.code(), Some(2));
    assert_eq!(
        delete.code(),
        Some(0),
        "delete reaches the OCI cache, as in real Tart"
    );
}

#[test]
fn should_refuse_stop_and_ip_when_the_vm_is_not_running() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");

    // when
    let stop = fake.tart(&["stop", "vm1"]);
    let ip = fake.tart(&["ip", "vm1"]);
    let exec = fake.tart(&["exec", "vm1", "--", "true"]);

    // then
    assert_eq!(
        (stop.code(), stop.stderr().as_str()),
        (Some(2), "VM \"vm1\" is not running\n")
    );
    assert_eq!(ip.code(), Some(1));
    assert!(
        ip.stderr().contains("no IP address found"),
        "{}",
        ip.stderr()
    );
    assert_eq!(exec.code(), Some(2));
}

#[test]
fn should_keep_a_detached_vm_running_when_run_has_returned() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    fake.detached();

    // when
    let run = fake.tart(&["run", "--no-graphics", "vm1"]);
    let again = fake.tart(&["run", "vm1"]);
    let ip = fake.tart(&["ip", "vm1"]);
    let delete = fake.tart(&["delete", "vm1"]);
    let stop = fake.tart(&["stop", "vm1", "--timeout", "1"]);

    // then
    assert_eq!(run.code(), Some(0));
    assert_eq!(
        (again.code(), again.stderr().as_str()),
        (Some(2), "VM \"vm1\" is already running!\n")
    );
    assert!(ip.stdout().starts_with("192.168.64."), "{}", ip.stdout());
    assert_eq!(
        (delete.code(), delete.stderr().as_str()),
        (Some(1), "VM \"vm1\" is running\n")
    );
    assert_eq!(stop.code(), Some(0));
    assert_eq!(fake.vm("vm1").unwrap()["run"], Value::Null);
}

#[test]
fn should_hold_a_foreground_vm_until_stopped_when_run_stays_alive() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    let mut runner = fake.spawn(&["run", "--no-graphics", "vm1"]);
    let ip = fake.tart(&["ip", "vm1", "--wait", "30"]);
    let listed = fake.tart(&["list", "--format", "json", "--source", "local"]);

    // when
    let stop = fake.tart(&["stop", "vm1"]);
    let status = runner.wait().expect("wait for run");

    // then
    assert_eq!(ip.code(), Some(0), "{}", ip.stderr());
    assert!(
        listed.stdout().contains("\"State\": \"running\""),
        "{}",
        listed.stdout()
    );
    assert_eq!(stop.code(), Some(0));
    assert_eq!(status.code(), Some(0), "run returns when the VM is stopped");
    assert!(
        fake.tart(&["list", "--format", "json"])
            .stdout()
            .contains("\"State\": \"stopped\"")
    );
}

#[test]
fn should_lose_a_foreground_vm_when_the_run_process_is_killed() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    let mut runner = fake.spawn(&["run", "vm1"]);
    assert_eq!(fake.tart(&["ip", "vm1", "--wait", "30"]).code(), Some(0));

    // when
    runner.kill().expect("kill run");
    runner.wait().expect("reap run");
    let get = fake.tart(&["get", "vm1", "--format", "json"]);
    let stop = fake.tart(&["stop", "vm1"]);

    // then
    assert!(
        get.stdout().contains("\"State\": \"stopped\""),
        "{}",
        get.stdout()
    );
    assert_eq!(stop.code(), Some(2));
}

#[test]
fn should_end_a_foreground_run_when_the_vm_is_deleted_after_stopping() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    let mut runner = fake.spawn(&["run", "vm1"]);
    assert_eq!(fake.tart(&["ip", "vm1", "--wait", "30"]).code(), Some(0));

    // when
    let stop = fake.tart(&["stop", "vm1"]);
    runner.wait().expect("run ends");
    let delete = fake.tart(&["delete", "vm1", "base"]);

    // then
    assert_eq!((stop.code(), delete.code()), (Some(0), Some(0)));
    assert!(fake.vm("vm1").is_none() && fake.vm("base").is_none());
}

#[test]
fn should_wait_for_the_vm_to_start_when_ip_is_given_a_wait() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    fake.detached();
    let waiter = std::thread::scope(|scope| {
        let waiter = scope.spawn(|| fake.tart(&["ip", "vm1", "--wait", "30"]));
        let run = fake.tart(&["run", "vm1"]);
        assert_eq!(run.code(), Some(0));
        waiter.join().expect("waiter")
    });

    // when
    let stdout = waiter.stdout();

    // then
    assert_eq!(waiter.code(), Some(0));
    assert_eq!(stdout.trim(), fake.tart(&["ip", "vm1"]).stdout().trim());
}

#[test]
fn should_record_what_the_guest_received_without_its_content_when_exec_reads_stdin() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    fake.detached();
    fake.tart(&["run", "vm1"]);
    let secret = b"eyJTUkVULUpJVC1DT05GSUciOiJ4In0=";

    // when
    let ran = fake.tart_with_stdin(
        &["exec", "-i", "vm1", "--", "/opt/launcher", "--jit"],
        secret,
    );

    // then
    assert_eq!(ran.code(), Some(0), "{}", ran.stderr());
    let stdin = &fake.events("stdin")[0];
    assert_eq!(stdin["len"], json!(secret.len()));
    assert_eq!(stdin["sha256"].as_str().unwrap().len(), 64);
    let launch = &fake.vm("vm1").unwrap()["guest_launches"][0];
    assert_eq!(launch["argv"], json!(["/opt/launcher", "--jit"]));
    assert_eq!(launch["stdin_len"], json!(secret.len()));
    assert_eq!(launch["stdin_sha256"], stdin["sha256"]);
    let entries: Vec<std::path::PathBuf> = std::fs::read_dir(fake.home().join("vms"))
        .unwrap()
        .flatten()
        .flat_map(|entry| {
            if entry.path().is_dir() {
                std::fs::read_dir(entry.path())
                    .unwrap()
                    .flatten()
                    .map(|e| e.path())
                    .collect()
            } else {
                vec![entry.path()]
            }
        })
        .collect();
    for entry in entries {
        let bytes = std::fs::read(&entry).unwrap();
        assert!(!contains(&bytes, secret), "{entry:?} holds the secret");
    }
    let log = std::fs::read(fake.home().join("invocations.jsonl")).unwrap();
    assert!(!contains(&log, secret), "the log holds the secret");
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn should_match_the_known_digest_when_exec_reads_a_known_stdin() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    fake.detached();
    fake.tart(&["run", "vm1"]);

    // when
    fake.tart_with_stdin(&["exec", "-i", "vm1", "cat"], b"abc");

    // then
    assert_eq!(
        fake.events("stdin")[0]["sha256"],
        json!("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
}

#[test]
fn should_not_read_stdin_when_exec_is_not_interactive() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    fake.detached();
    fake.tart(&["run", "vm1"]);
    let mut child = fake.spawn(&["exec", "vm1", "--", "true"]);

    // when
    let status = child.wait().expect("exec ends without stdin being closed");

    // then
    assert_eq!(status.code(), Some(0));
    assert!(fake.events("stdin").is_empty());
    assert_eq!(
        fake.vm("vm1").unwrap()["guest_launches"][0]["stdin_len"],
        Value::Null
    );
}

#[test]
fn should_log_every_invocation_with_its_argv_environment_and_exit_when_commands_run() {
    // given
    let fake = Fake::new();

    // when
    fake.tart(&["create", "base"]);
    fake.tart(&["stop", "base"]);
    fake.tart(&["bogus"]);

    // then
    let invokes = fake.events("invoke");
    let exits = fake.events("exit");
    assert_eq!(invokes.len(), 3);
    assert_eq!(invokes[1]["argv"], json!(["stop", "base"]));
    assert_eq!(
        invokes[0]["env"]["TART_HOME"],
        json!(fake.home().to_str().unwrap())
    );
    assert_eq!(
        exits
            .iter()
            .map(|e| e["code"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        [0, 2, 64]
    );
    let ids: Vec<_> = invokes.iter().map(|r| r["inv"].as_u64().unwrap()).collect();
    assert_eq!(ids, [0, 1, 2]);
    assert_eq!(fake.events("effect")[0]["kind"], json!("vm_created"));
}

#[test]
fn should_never_log_an_environment_value_when_the_variable_is_not_allowlisted() {
    // given
    let fake = Fake::new();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tart"))
        .arg("--version")
        .env("TART_HOME", fake.home())
        .env("GITHUB_TOKEN", "s3cr3t-token-value")
        .env("MAYFLY_FAKE_MODE", "visible")
        .output()
        .expect("run tart");

    // when
    let log = std::fs::read_to_string(fake.home().join("invocations.jsonl")).unwrap();
    let env = &fake.events("invoke")[0]["env"];

    // then
    assert_eq!(output.status.code(), Some(0));
    assert!(!log.contains("s3cr3t-token-value"), "{log}");
    assert_eq!(env["GITHUB_TOKEN"], Value::Null);
    assert_eq!(env["MAYFLY_FAKE_MODE"], json!("visible"));
    assert_eq!(env["TART_HOME"], json!(fake.home().to_str().unwrap()));
}

#[test]
fn should_serialise_concurrent_invocations_when_many_clone_at_once() {
    // given
    let fake = Fake::new();
    fake.tart(&["create", "base"]);
    let names: Vec<String> = (0..8).map(|i| format!("vm{i}")).collect();

    // when
    let codes: Vec<Option<i32>> = std::thread::scope(|scope| {
        let handles: Vec<_> = names
            .iter()
            .map(|n| scope.spawn(|| fake.tart(&["clone", "base", n]).code()))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // then
    assert!(codes.iter().all(|c| *c == Some(0)));
    let mut ids: Vec<u64> = fake
        .events("invoke")
        .iter()
        .map(|r| r["inv"].as_u64().unwrap())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 9, "no invocation id was handed out twice");
    let macs: std::collections::BTreeSet<String> = names
        .iter()
        .map(|n| fake.vm(n).unwrap()["mac"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(macs.len(), 8, "no MAC was handed out twice");
}

#[test]
fn should_fail_an_exec_when_the_vm_stops_during_the_job() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    fake.faults(
        "run_mode = \"detached\"\n[[rule]]\ncommand = \"exec\"\njob_ms = 600000\njob_stderr = \"job started\"\n",
    );
    fake.tart(&["run", "vm1"]);
    let mut job = fake.spawn(&["exec", "vm1", "--", "/opt/launcher"]);
    let mut stderr = BufReader::new(job.stderr.take().expect("stderr"));
    let mut first = String::new();
    stderr
        .read_line(&mut first)
        .expect("the guest says its job started");

    // when
    let stop = fake.tart(&["stop", "vm1"]);
    let status = job.wait().expect("job ends");
    let mut rest = String::new();
    stderr.read_to_string(&mut rest).expect("rest of stderr");

    // then
    assert_eq!(first, "job started\n");
    assert_eq!(stop.code(), Some(0));
    assert_eq!(status.code(), Some(1));
    assert!(rest.contains("control socket"), "{rest}");
}

#[test]
fn should_draw_a_new_mac_and_resize_when_set_is_given_random_mac_and_memory() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    let before: Value =
        serde_json::from_str(&fake.tart(&["get", "vm1", "--format", "json"]).stdout())
            .expect("get json");

    // when
    let set = fake.tart(&["set", "vm1", "--random-mac", "--memory", "4096"]);
    let after: Value =
        serde_json::from_str(&fake.tart(&["get", "vm1", "--format", "json"]).stdout())
            .expect("get json");
    let base: Value =
        serde_json::from_str(&fake.tart(&["get", "base", "--format", "json"]).stdout())
            .expect("get json");

    // then
    assert_eq!(set.code(), Some(0), "{}", set.stderr());
    assert_ne!(after["MACAddress"], before["MACAddress"]);
    assert_ne!(after["MACAddress"], base["MACAddress"]);
    assert_eq!(after["Memory"], json!(4096));
    assert_eq!(
        fake.events("effect").last().expect("effect")["kind"],
        "vm_set"
    );
}

#[test]
fn should_refuse_set_when_the_vm_does_not_exist_or_the_disk_would_shrink() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");

    // when
    let absent = fake.tart(&["set", "nope", "--random-mac"]);
    let shrink = fake.tart(&["set", "vm1", "--disk-size", "1"]);

    // then
    assert_eq!(absent.code(), Some(2), "{}", absent.stderr());
    assert_eq!(shrink.code(), Some(1), "{}", shrink.stderr());
}

#[test]
fn should_record_a_signal_when_a_foreground_run_receives_one() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    let mut runner = fake.spawn(&["run", "vm1"]);
    assert_eq!(fake.tart(&["ip", "vm1", "--wait", "30"]).code(), Some(0));
    let pid =
        rustix::process::Pid::from_raw(i32::try_from(runner.id()).expect("pid")).expect("a pid");

    // when
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).expect("send SIGTERM");
    let status = runner.wait().expect("reap run");

    // then
    let signals = fake.events("signal");
    assert_eq!(signals.len(), 1, "{signals:?}");
    assert_eq!(signals[0]["signal"], "SIGTERM");
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(15),
        "the signal then does what it does by default"
    );
    assert!(
        fake.tart(&["get", "vm1", "--format", "json"])
            .stdout()
            .contains("\"State\": \"stopped\"")
    );
}

#[test]
fn should_record_no_signal_when_a_foreground_run_is_stopped_by_tart_stop() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    let mut runner = fake.spawn(&["run", "vm1"]);
    assert_eq!(fake.tart(&["ip", "vm1", "--wait", "30"]).code(), Some(0));

    // when
    let stop = fake.tart(&["stop", "vm1"]);
    let status = runner.wait().expect("reap run");

    // then
    assert_eq!(stop.code(), Some(0));
    assert_eq!(status.code(), Some(0));
    assert!(fake.events("signal").is_empty());
}

#[test]
fn should_record_a_signal_when_a_hung_run_receives_one() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    fake.faults("[[rule]]\ncommand = \"run\"\naction = \"hang\"\n");
    let mut runner = fake.spawn(&["run", "vm1"]);
    let mut stderr = BufReader::new(runner.stderr.take().expect("stderr"));
    let mut line = String::new();
    stderr.read_line(&mut line).expect("the hang line");
    assert!(line.contains("hang fault"), "{line}");
    let pid =
        rustix::process::Pid::from_raw(i32::try_from(runner.id()).expect("pid")).expect("a pid");

    // when
    rustix::process::kill_process(pid, rustix::process::Signal::INT).expect("send SIGINT");
    runner.wait().expect("reap run");

    // then
    let signals = fake.events("signal");
    assert_eq!(signals.len(), 1, "{signals:?}");
    assert_eq!(signals[0]["signal"], "SIGINT");
}

fn encode(name: &str) -> String {
    name.bytes()
        .enumerate()
        .map(|(i, b)| {
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || (b == b'.' && i > 0) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn sha(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Seeds `blobs` (config, nvram and two disk chunks) and a manifest naming them in Tart's order
/// (config, the chunks with their uncompressed size and digest, NVRAM last); returns the
/// manifest's bytes.
fn seed_image(
    fake: &Fake,
    reference: &str,
    blobs: [&[u8]; 4],
    manifest_for: Option<&[u8]>,
) -> Vec<u8> {
    let dir = fake.home().join("registry");
    std::fs::create_dir_all(dir.join("blobs")).expect("registry");
    let [config, nvram, first, second] = blobs;
    let layer = |media: &str, bytes: &[u8], chunk: bool| {
        let digest = format!("sha256:{}", sha(bytes));
        std::fs::write(dir.join("blobs").join(encode(&digest)), bytes).expect("blob");
        let mut layer =
            serde_json::json!({"mediaType": media, "digest": digest, "size": bytes.len()});
        if chunk {
            layer["annotations"] = serde_json::json!({
                "org.cirruslabs.tart.uncompressed-size": bytes.len().to_string(),
                "org.cirruslabs.tart.uncompressed-content-digest": digest,
            });
        }
        layer
    };
    let disk = "application/vnd.cirruslabs.tart.disk.v2";
    let layers = vec![
        layer("application/vnd.cirruslabs.tart.config.v1", config, false),
        layer(disk, first, true),
        layer(disk, second, true),
        layer("application/vnd.cirruslabs.tart.nvram.v1", nvram, false),
    ];
    let manifest = serde_json::to_vec(&serde_json::json!({"schemaVersion": 2, "layers": layers}))
        .expect("manifest");
    let served = manifest_for.map_or_else(|| manifest.clone(), <[u8]>::to_vec);
    std::fs::write(dir.join(encode(reference)), &served).expect("seed the registry");
    served
}

#[test]
fn should_lay_out_the_image_named_by_the_served_manifest_hash_when_pulling() {
    // given
    let fake = Fake::new();
    let blobs: [&[u8]; 4] = [
        br#"{"os":"linux","macAddress":"aa"}"#,
        b"nvram",
        b"chunk-1",
        b"chunk-2",
    ];
    let probe = seed_image(&fake, "ghcr.io/acme/img:probe", blobs, None);
    let digest = format!("sha256:{}", sha(&probe));
    let reference = format!("ghcr.io/acme/img@{digest}");
    seed_image(&fake, &reference, blobs, None);

    // when
    let pulled = fake.tart(&["pull", &reference]);
    let unknown = fake.tart(&["pull", "ghcr.io/acme/img:latest"]);
    let listed = fake.tart(&["list", "--source", "oci", "-q"]);

    // then
    assert_eq!(pulled.code(), Some(0));
    assert_eq!(unknown.code(), Some(1));
    assert!(unknown.stderr().contains("manifest unknown"));
    assert_eq!(listed.stdout(), format!("{reference}\n"));
    let entry = fake
        .home()
        .join("cache/OCIs/ghcr.io/acme/img")
        .join(&digest);
    assert!(!entry.is_symlink(), "served what was asked: no link");
    let cached = std::fs::read(entry.join("manifest.json")).expect("cached");
    assert_eq!(cached, probe, "2.41.1 caches the manifest as served");
    assert_eq!(
        std::fs::read(entry.join("disk.img")).expect("disk"),
        b"chunk-1chunk-2"
    );
    assert_eq!(
        std::fs::read(entry.join("nvram.bin")).expect("nvram"),
        b"nvram"
    );
    let clone = fake.tart(&["clone", &reference, "vm1"]);
    assert_eq!(clone.code(), Some(0));
    let got = fake.tart(&["get", "vm1", "--format", "json"]);
    assert_eq!(
        serde_json::from_str::<Value>(&got.stdout()).expect("get")["OS"],
        "linux"
    );
    let vm_dir = fake.home().join("vms/vm1");
    assert_eq!(
        std::fs::read(vm_dir.join("disk.img")).expect("disk"),
        b"chunk-1chunk-2"
    );
}

#[test]
fn should_link_the_asked_digest_to_another_entry_when_the_registry_serves_another_manifest() {
    // given
    let fake = Fake::new();
    let asked = format!("sha256:{}", "ab".repeat(32));
    let reference = format!("ghcr.io/acme/img@{asked}");
    let served = seed_image(&fake, &reference, [b"{}", b"n", b"d", b"e"], None);

    // when
    let pulled = fake.tart(&["pull", &reference]);

    // then
    assert_eq!(pulled.code(), Some(0));
    let link = fake.home().join("cache/OCIs/ghcr.io/acme/img").join(&asked);
    assert!(link.is_symlink());
    let own = format!("sha256:{}", sha(&served));
    let target = std::fs::read_link(&link).expect("link");
    assert!(target.is_absolute(), "{}", target.display());
    assert_eq!(target, link.with_file_name(&own));
    let listed = fake.tart(&["list", "--source", "oci", "-q"]).stdout();
    assert_eq!(
        listed,
        format!("ghcr.io/acme/img:{asked}\nghcr.io/acme/img@{own}\n"),
        "a link is listed with `:`, an entry with `@`"
    );
}

#[test]
fn should_collect_the_linked_entry_when_its_last_link_is_deleted() {
    // given
    let fake = Fake::new();
    let reference = "ghcr.io/acme/img:1";
    let manifest = seed_image(&fake, reference, [b"{}", b"n", b"d", b"e"], None);
    assert_eq!(fake.tart(&["pull", reference]).code(), Some(0));
    let cached = fake.home().join("cache/OCIs/ghcr.io/acme/img/1");
    assert!(cached.is_symlink());
    let own = format!("ghcr.io/acme/img@sha256:{}", sha(&manifest));
    let entry = fake
        .home()
        .join("cache/OCIs/ghcr.io/acme/img")
        .join(format!("sha256:{}", sha(&manifest)));
    let before = fake.tart(&["list", "--source", "oci", "-q"]).stdout();

    // when
    let tag_deleted = fake.tart(&["delete", reference]);
    let own_deleted = fake.tart(&["delete", &own]);

    // then: Tart's `gc` removes an entry no link names that was not pulled by its own digest
    assert_eq!(before, format!("{reference}\n{own}\n"));
    assert_eq!((tag_deleted.code(), own_deleted.code()), (Some(0), Some(2)));
    assert!(own_deleted.stderr().contains("does not exist"));
    assert!(!cached.exists() && !cached.is_symlink());
    assert!(!entry.exists());
    assert_eq!(fake.tart(&["list", "--source", "oci", "-q"]).stdout(), "");
}

#[test]
fn should_keep_an_entry_pulled_by_its_own_digest_when_a_tag_to_it_is_deleted() {
    // given
    let fake = Fake::new();
    let blobs: [&[u8]; 4] = [b"{}", b"n", b"d", b"e"];
    let probe = seed_image(&fake, "ghcr.io/acme/img:1", blobs, None);
    let own = format!("ghcr.io/acme/img@sha256:{}", sha(&probe));
    seed_image(&fake, &own, blobs, None);
    assert_eq!(fake.tart(&["pull", &own]).code(), Some(0));
    assert_eq!(fake.tart(&["pull", "ghcr.io/acme/img:1"]).code(), Some(0));

    // when
    let tag_deleted = fake.tart(&["delete", "ghcr.io/acme/img:1"]);

    // then
    assert_eq!(tag_deleted.code(), Some(0));
    assert_eq!(
        fake.tart(&["list", "--source", "oci", "-q"]).stdout(),
        format!("{own}\n")
    );
}

#[test]
fn should_refuse_a_pull_when_the_layout_is_not_what_tart_accepts() {
    // given: disk v1 and a manifest whose NVRAM precedes its disk, both refused by 2.41.1
    let fake = Fake::new();
    let dir = fake.home().join("registry");
    std::fs::create_dir_all(&dir).expect("registry");
    let manifest = |media: [&str; 3]| {
        let layers: Vec<Value> = media
            .iter()
            .map(|m| json!({"mediaType": m, "digest": format!("sha256:{}", "0".repeat(64)), "size": 1}))
            .collect();
        serde_json::to_vec(&json!({"schemaVersion": 2, "layers": layers})).expect("manifest")
    };
    let config = "application/vnd.cirruslabs.tart.config.v1";
    let nvram = "application/vnd.cirruslabs.tart.nvram.v1";
    std::fs::write(
        dir.join(encode("ghcr.io/acme/img:v1")),
        manifest([config, "application/vnd.cirruslabs.tart.disk.v1", nvram]),
    )
    .expect("v1");
    std::fs::write(
        dir.join(encode("ghcr.io/acme/img:order")),
        manifest([config, nvram, "application/vnd.cirruslabs.tart.disk.v2"]),
    )
    .expect("order");

    // when
    let v1 = fake.tart(&["pull", "ghcr.io/acme/img:v1"]);
    let order = fake.tart(&["pull", "ghcr.io/acme/img:order"]);

    // then
    assert_eq!((v1.code(), order.code()), (Some(1), Some(1)));
    assert!(v1.stderr().contains("disk.v2 base"), "{}", v1.stderr());
    assert!(order.stderr().contains("ordered"), "{}", order.stderr());
    assert_eq!(fake.tart(&["list", "--source", "oci", "-q"]).stdout(), "");
}

#[test]
fn should_set_the_given_mac_and_a_macos_serial_when_set_is_given_mac_and_random_serial() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    assert_eq!(fake.tart(&["create", "lin", "--linux"]).code(), Some(0));
    let config = |vm: &str| -> Value {
        serde_json::from_slice(
            &std::fs::read(fake.home().join("vms").join(vm).join("config.json")).expect("config"),
        )
        .expect("json")
    };
    let (mac_ecid, linux_before) = (config("vm1")["ecid"].clone(), config("lin"));

    // when
    let mac = fake.tart(&[
        "set",
        "vm1",
        "--mac",
        "02:AA:00:00:00:01",
        "--random-serial",
    ]);
    let linux = fake.tart(&["set", "lin", "--random-serial"]);
    let both = fake.tart(&["set", "vm1", "--mac", "02:aa:00:00:00:02", "--random-mac"]);

    // then
    assert_eq!(
        (mac.code(), linux.code(), both.code()),
        (Some(0), Some(0), Some(64))
    );
    let got: Value = serde_json::from_str(&fake.tart(&["get", "vm1", "--format", "json"]).stdout())
        .expect("get");
    assert_eq!(got["MACAddress"], "02:aa:00:00:00:01");
    assert!(mac_ecid.is_string());
    assert_ne!(config("vm1")["ecid"], mac_ecid);
    assert_eq!(config("lin"), linux_before, "a Linux VM has no serial");
}

#[test]
fn should_print_a_null_disk_when_a_vm_with_an_asif_disk_runs() {
    // given
    let fake = Fake::new();
    fake.detached();
    assert_eq!(
        fake.tart(&["create", "base", "--disk-format", "asif"])
            .code(),
        Some(0)
    );
    assert_eq!(fake.tart(&["clone", "base", "vm1"]).code(), Some(0));
    let disk = || -> Value {
        serde_json::from_str::<Value>(&fake.tart(&["get", "vm1", "--format", "json"]).stdout())
            .expect("get")["Disk"]
            .clone()
    };
    let stopped = disk();

    // when
    assert_eq!(fake.tart(&["run", "vm1"]).code(), Some(0));
    let running = disk();
    let listed: Value =
        serde_json::from_str(&fake.tart(&["list", "--format", "json"]).stdout()).expect("list");

    // then
    assert_eq!((stopped, running), (json!(50), Value::Null));
    assert_eq!(listed[1]["Disk"], Value::Null);
    assert_eq!(fake.tart(&["stop", "vm1"]).code(), Some(0));
}

#[test]
fn should_rewrite_the_config_mac_when_set_draws_a_random_mac() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    let config = || -> Value {
        serde_json::from_slice(
            &std::fs::read(fake.home().join("vms/vm1/config.json")).expect("config"),
        )
        .expect("json")
    };
    let before = config()["macAddress"].clone();

    // when
    let set = fake.tart(&["set", "vm1", "--random-mac"]);

    // then
    assert_eq!(set.code(), Some(0));
    let got: Value = serde_json::from_str(&fake.tart(&["get", "vm1", "--format", "json"]).stdout())
        .expect("get");
    assert_ne!(config()["macAddress"], before);
    assert_eq!(config()["macAddress"], got["MACAddress"]);
}
