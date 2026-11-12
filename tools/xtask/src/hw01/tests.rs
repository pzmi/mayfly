//! Whole runs of `hw01` on Linux against `mayfly-fake-tart` (the `tart` binary a workspace build
//! puts next to this test executable's directory): cleanup after a failure midway and after an
//! interruption, a caller's home and VM left alone, redaction of planted host facts, and the
//! `ImageStore` contract `not_run` when its image cannot be built.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;

use super::redact::Identity;
use super::session::Entry;
use super::{Env, Hooks, Options, execute, supported};

const RUNNER_SHA: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";

/// The fake's `tart`, built beside this test executable by a workspace build.
fn fake_tart() -> PathBuf {
    let exe = std::env::current_exe().expect("the test executable");
    let path = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>/deps")
        .join("tart");
    assert!(
        path.is_file(),
        "{} is not built: build the workspace, or `cargo build -p mayfly-fake-tart`",
        path.display()
    );
    path
}

/// The fake against `home`, directly. A coverage run's `LLVM_PROFILE_FILE` is passed through
/// (test harness only), so the instrumented fake writes its profile where coverage collects it
/// and never into the working directory.
fn tart_in(home: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(fake_tart());
    command.env_clear().env("TART_HOME", home).args(args);
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    if let Some(parent) = home.parent() {
        command.current_dir(parent);
    }
    command.output().expect("run the fake")
}

/// Synthetic pins: the fake pulls any reference.
fn pins(dir: &Path) -> PathBuf {
    let digest = format!("sha256:{}", "ab".repeat(32));
    let path = dir.join("images.toml");
    std::fs::write(
        &path,
        format!(
            "schema = 1\n[images.linux]\nrepository = \"registry.example/linux\"\ndigest = \
             \"{digest}\"\nsize_bytes = 1\nreason = \"test\"\n[images.macos]\nrepository = \
             \"registry.example/macos\"\ndigest = \"{digest}\"\nsize_bytes = 1\nreason = \
             \"test\"\n[runners.\"2.330.0\"]\nlinux-arm64 = \"{RUNNER_SHA}\"\n\
             osx-arm64 = \"{RUNNER_SHA}\"\n"
        ),
    )
    .expect("write pins");
    path
}

/// What a test captured at `remove-run-dir`: the record and the fake's invocation log.
#[derive(Default)]
struct Captured {
    record: Vec<Entry>,
    invocations: Vec<Value>,
}

struct World {
    dir: tempfile::TempDir,
    caller_home: PathBuf,
    captured: Rc<RefCell<Captured>>,
    interrupt: Arc<AtomicBool>,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let caller_home = dir.path().join("caller-home");
        std::fs::create_dir(&caller_home).expect("caller home");
        let created = tart_in(&caller_home, &["create", "caller-vm", "--linux"]);
        assert!(created.status.success(), "{created:?}");
        std::fs::create_dir(dir.path().join("tmp")).expect("tmp");
        Self {
            dir,
            caller_home,
            captured: Rc::new(RefCell::new(Captured::default())),
            interrupt: Arc::new(AtomicBool::new(false)),
        }
    }

    /// An environment over the fake; `faults` goes to the run's home before anything runs, and
    /// `interrupt_at` sets the interruption when that step starts.
    fn env(&self, faults: &str, identity: Identity, interrupt_at: Option<&'static str>) -> Env {
        let faults = faults.to_owned();
        let captured = Rc::clone(&self.captured);
        let interrupt = Arc::clone(&self.interrupt);
        let on_step = move |step: &str, home: &Path| {
            if Some(step) == interrupt_at {
                interrupt.store(true, Ordering::SeqCst);
            }
            if step == "remove-run-dir" {
                let read = |p: PathBuf| std::fs::read_to_string(p).unwrap_or_default();
                let run_dir = home.parent().expect("the run directory");
                let mut captured = captured.borrow_mut();
                captured.record = read(run_dir.join("record.jsonl"))
                    .lines()
                    .map(|l| serde_json::from_str(l).expect("a record line"))
                    .collect();
                captured.invocations = read(home.join("invocations.jsonl"))
                    .lines()
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect();
            }
        };
        let on_home = move |home: &Path| {
            // A VM this run did not make, in its own home.
            assert!(
                tart_in(home, &["create", "foreign-vm", "--linux"])
                    .status
                    .success()
            );
            if !faults.is_empty() {
                std::fs::write(home.join("faults.toml"), &faults).expect("faults");
            }
        };
        Env {
            tart: fake_tart(),
            cargo: PathBuf::from("/bin/true"),
            security: None,
            sudo: None,
            repo: crate::repo::root(),
            pins: pins(self.dir.path()),
            temp_parent: self.dir.path().join("tmp"),
            caller_homes: vec![self.caller_home.clone()],
            identity,
            user_env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            interrupt: Arc::clone(&self.interrupt),
            run_id: "t1".to_owned(),
            hooks: Hooks {
                on_home: Some(Box::new(on_home)),
                on_step: Some(Box::new(on_step)),
            },
        }
    }

    fn options(&self) -> Options {
        Options {
            macos: false,
            service_user: None,
            report: Some(self.dir.path().join("report.json")),
            cleanup: None,
        }
    }

    /// The VM names the fake was asked to stop or delete.
    fn removed(&self) -> BTreeSet<String> {
        self.captured
            .borrow()
            .invocations
            .iter()
            .filter(|r| r["event"] == "invoke")
            .filter_map(|r| r["argv"].as_array())
            .filter(|argv| {
                matches!(
                    argv.first().and_then(Value::as_str),
                    Some("stop" | "delete")
                )
            })
            .filter_map(|argv| argv.get(1).and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    fn recorded(&self, kind: fn(&Entry) -> Option<String>) -> BTreeSet<String> {
        self.captured
            .borrow()
            .record
            .iter()
            .filter_map(kind)
            .collect()
    }

    fn assert_clean(&self) {
        let left: Vec<_> = std::fs::read_dir(self.dir.path().join("tmp"))
            .expect("tmp")
            .collect();
        assert!(left.is_empty(), "the run directory is gone: {left:?}");
        let caller = tart_in(&self.caller_home, &["get", "caller-vm", "--format", "json"]);
        assert!(caller.status.success(), "the caller's VM is untouched");
        let removed = self.removed();
        assert!(!removed.contains("caller-vm"), "{removed:?}");
        for pid in self.recorded(|e| match e {
            Entry::ProcessPid { pid, .. } => Some(pid.to_string()),
            _ => None,
        }) {
            assert!(
                !Path::new("/proc").join(&pid).exists(),
                "process {pid} still runs"
            );
        }
    }
}

/// The only leftover: the VM planted in the run's own home, which the record does not name,
/// stopped and deleted by the final sweep and reported.
fn assert_only_the_swept_foreign_vm(leftovers: &[String]) {
    assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    assert!(
        leftovers[0].contains("vm foreign-vm in $RUN/home")
            && leftovers[0].contains("stopped and deleted"),
        "{leftovers:?}"
    );
}

fn vm_name(entry: &Entry) -> Option<String> {
    match entry {
        Entry::Vm { name } => Some(name.clone()),
        _ => None,
    }
}

fn image_name(entry: &Entry) -> Option<String> {
    match entry {
        Entry::Image { reference } => Some(reference.clone()),
        _ => None,
    }
}

fn status(report: &Value, id: &str) -> (String, String) {
    let check = report["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no check {id}"));
    (
        check["status"].as_str().unwrap_or_default().to_owned(),
        check["reason"].as_str().unwrap_or_default().to_owned(),
    )
}

#[test]
fn should_refuse_to_start_when_the_system_is_not_macos() {
    // given
    let systems = ["linux", "windows", "macos"];

    // when
    let verdicts = systems.map(supported);

    // then
    assert!(verdicts[0].is_err() && verdicts[1].is_err() && verdicts[2].is_ok());
    #[cfg(not(target_os = "macos"))]
    assert_eq!(super::run(&Options::default()), 2);
}

#[test]
fn should_delete_exactly_the_record_when_a_check_fails_midway() {
    // given: the Linux base never boots
    let world = World::new();
    let faults =
        "[[rule]]\ncommand = \"run\"\nvm = \"hw01-t1-linux\"\naction = \"fail_before_effect\"\n";
    let env = world.env(faults, Identity::default(), None);

    // when
    let outcome = execute(&env, &world.options());

    // then
    assert_eq!(outcome.code, 1, "{:#}", outcome.report);
    assert_only_the_swept_foreign_vm(&outcome.leftovers);
    assert_eq!(status(&outcome.report, "run-lock-holder").0, "fail");
    assert_eq!(status(&outcome.report, "exec-guest").0, "not_run");
    let recorded = world.recorded(vm_name);
    assert!(
        recorded.contains("hw01-t1-linux") && recorded.contains("mfb-contract"),
        "{recorded:?}"
    );
    let deleted: BTreeSet<String> = world
        .removed()
        .into_iter()
        .filter(|n| !n.starts_with("hw01-t1-absent"))
        // The planted VM goes only in the final sweep of the run's home, and is reported.
        .filter(|n| n != "foreign-vm")
        .collect();
    let images = world.recorded(image_name);
    assert!(
        deleted.is_subset(&recorded.union(&images).cloned().collect()),
        "{deleted:?}"
    );
    assert!(
        recorded.is_subset(&deleted),
        "every recorded VM deleted: {recorded:?} {deleted:?}"
    );
    world.assert_clean();
}

#[test]
fn should_clean_up_and_stop_its_vm_when_the_run_is_interrupted() {
    // given: Ctrl-C arrives while the Linux base runs
    let world = World::new();
    let env = world.env("", Identity::default(), Some("exec-guest"));

    // when
    let outcome = execute(&env, &world.options());

    // then
    assert_eq!(outcome.code, 130);
    assert_eq!(outcome.report["interrupted"], true);
    assert_ne!(status(&outcome.report, "run-lock-holder").0, "not_run");
    assert_ne!(status(&outcome.report, "exec-guest").0, "pass", "cut short");
    for id in [
        "exec-interactive-redirected",
        "vm-provider-contract",
        "IC-01",
        "launchd-service-mode",
    ] {
        assert_eq!(status(&outcome.report, id).0, "not_run", "{id}");
    }
    assert!(world.removed().contains("hw01-t1-linux"));
    assert_only_the_swept_foreign_vm(&outcome.leftovers);
    world.assert_clean();
}

#[test]
fn should_report_the_image_store_contract_not_run_when_its_image_cannot_be_built() {
    // given: installing the test launcher into the contract source fails
    let world = World::new();
    let faults = "[[rule]]\ncommand = \"exec\"\nvm = \"hw01-t1-contract\"\narg0 = \"sudo\"\n\
                  action = \"fail_before_effect\"\n";
    let env = world.env(faults, Identity::default(), None);

    // when
    let outcome = execute(&env, &world.options());

    // then
    for id in super::checks::IC_CASES {
        let (status, reason) = status(&outcome.report, id);
        assert_eq!(status, "not_run", "{id}");
        assert!(reason.contains("install the launcher"), "{id}: {reason}");
    }
    assert_eq!(status(&outcome.report, "launcher-verify").0, "fail");
    assert_eq!(status(&outcome.report, "contract-image").0, "not_run");
    world.assert_clean();
}

#[test]
fn should_find_no_planted_identity_when_the_report_is_written() {
    // given: Tart's output carries the host's names, a real MAC and a token
    let world = World::new();
    let planted = "owners-mac pat /Users/pat/Library/x C02XK1ABJG5H A1B2C3D4-0000-1111-2222-333344445555 \
         8e:22:11:00:aa:01";
    let token = super::redact::planted_token();
    let faults = format!(
        "[[rule]]\ncommand = \"get\"\nvm = \"hw01-t1-absent\"\naction = \"fail_before_effect\"\n\
         exit_code = 2\nstderr = \"does not exist on {planted}\"\n\
         [[rule]]\ncommand = \"exec\"\narg0 = \"uname\"\njob_stdout = \"Linux {token}\"\n"
    );
    let identity = Identity {
        hostnames: vec!["owners-mac.local".to_owned()],
        username: Some("pat".to_owned()),
        home: Some("/Users/pat".to_owned()),
        serial: Some("C02XK1ABJG5H".to_owned()),
        hardware_uuid: Some("A1B2C3D4-0000-1111-2222-333344445555".to_owned()),
        service_user: None,
    };
    let env = world.env(&faults, identity, None);

    // when
    let outcome = execute(&env, &world.options());

    // then
    let written = std::fs::read_to_string(world.dir.path().join("report.json")).expect("report");
    for leak in [
        "owners-mac",
        "C02XK1ABJG5H",
        "A1B2C3D4",
        "8e:22:11:00:aa:01",
        "ghp_",
        "/home/",
        "/tmp/",
        "/Users/",
    ] {
        assert!(!written.contains(leak), "{leak} in {written}");
    }
    assert_eq!(
        super::redact::find_word(&written, "pat"),
        None,
        "the user name, as a word"
    );
    assert!(written.contains("<home>/Library/x"), "{written}");
    assert!(
        !super::redact::Redactor::new(&Identity::default(), world.dir.path(), &[])
            .text(&written)
            .contains(&token)
    );
    assert!(
        written.contains("<hostname>") && written.contains("<serial>"),
        "{written}"
    );
    let (exec, reason) = status(&outcome.report, "exec-guest");
    assert_eq!(
        (exec.as_str(), reason.contains("redaction")),
        ("fail", true),
        "{reason}"
    );
    super::report::validate(&outcome.report).expect("schema");
}

#[test]
fn should_delete_what_a_crashed_run_recorded_when_cleanup_replays_it() {
    // given: a run directory as a crash leaves it: a recorded VM, an unrecorded one a contract
    // suite left in the run's home, and a VM in the caller's home
    let world = World::new();
    let env = world.env("", Identity::default(), None);
    let run_dir = world.dir.path().join("tmp").join("mayfly-hw01-crashed");
    let home = run_dir.join("home");
    for dir in [&run_dir, &home] {
        std::fs::create_dir(dir).expect("mkdir");
        std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .expect("chmod");
    }
    for name in ["hw01-t1-linux", "left-by-a-suite"] {
        assert!(
            tart_in(&home, &["create", name, "--linux"])
                .status
                .success()
        );
    }
    std::fs::write(
        run_dir.join("record.jsonl"),
        "{\"kind\":\"vm\",\"name\":\"hw01-t1-linux\"}\n",
    )
    .expect("record");

    // when
    let code = super::clean_up(&env, &run_dir);

    // then
    assert!(!run_dir.exists(), "the run directory is gone");
    assert_eq!(
        code, 0,
        "the unrecorded VM was stopped and deleted, and reported"
    );
    let caller = tart_in(
        &world.caller_home,
        &["get", "caller-vm", "--format", "json"],
    );
    assert!(caller.status.success(), "the caller's VM is untouched");
}

#[test]
fn should_refuse_a_directory_when_it_is_not_a_run_of_hw01() {
    // given
    let world = World::new();
    let env = world.env("", Identity::default(), None);
    let other = world.dir.path().join("tmp").join("something-else");
    std::fs::create_dir(&other).expect("mkdir");
    std::fs::write(other.join("keep"), "x").expect("file");

    // when
    let code = super::clean_up(&env, &other);

    // then
    assert_eq!(code, 2);
    assert!(other.join("keep").exists(), "nothing removed");
}
