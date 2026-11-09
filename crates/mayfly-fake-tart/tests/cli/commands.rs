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
    assert_eq!((ran.code(), ran.stdout().as_str()), (Some(0), "2.28.0\n"));
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
    let pinned = fixture(include_str!("../fixtures/tart-list-2.28.0.json"));
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
    let pinned = fixture(include_str!("../fixtures/tart-get-2.28.0.json"));
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
        (Some(1), "Error: VM \"vm1\" is running\n")
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
    for entry in std::fs::read_dir(fake.home().join("vms"))
        .unwrap()
        .flatten()
    {
        let bytes = std::fs::read(entry.path()).unwrap();
        assert!(
            !contains(&bytes, secret),
            "{:?} holds the secret",
            entry.path()
        );
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
