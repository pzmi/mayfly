//! The fault file: each action, its selectors, and the invocation log that proves a fault landed.

use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;

use serde_json::json;

use crate::support::Fake;

#[test]
fn should_delay_a_command_and_log_the_delay_when_a_rule_sets_delay_ms() {
    // given
    let fake = Fake::new();
    fake.faults("[[rule]]\nid = \"slow\"\ncommand = \"create\"\ndelay_ms = 30\n");

    // when
    let ran = fake.tart(&["create", "base"]);

    // then
    assert_eq!(ran.code(), Some(0));
    let faults = fake.events("fault");
    assert_eq!(faults.len(), 1);
    assert_eq!(faults[0]["action"], json!("delay"));
    assert_eq!(faults[0]["rule"], json!("slow"));
    assert!(fake.vm("base").is_some());
}

#[test]
fn should_hang_before_any_effect_until_killed_when_the_action_is_hang() {
    // given
    let fake = Fake::new();
    fake.faults("[[rule]]\nid = \"stuck\"\ncommand = \"create\"\naction = \"hang\"\n");
    let mut child = fake.spawn(&["create", "base"]);
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let mut line = String::new();
    stderr
        .read_line(&mut line)
        .expect("the hang announces itself");

    // when
    child.kill().expect("kill the hung process");
    let status = child.wait().expect("reap");

    // then
    assert_eq!(line, "fake-tart: hang fault 'stuck' is active\n");
    assert_eq!(status.signal(), Some(9));
    assert!(
        fake.vm("base").is_none(),
        "a hang before the effect leaves no VM"
    );
    let fault = &fake.events("fault")[0];
    assert_eq!(
        (&fault["action"], &fault["phase"]),
        (&json!("hang"), &json!("before_effect"))
    );
    assert!(
        fake.events("exit").is_empty(),
        "a hung process never exits by itself"
    );
}

#[test]
fn should_hang_after_the_effect_when_the_action_is_hang_after_effect() {
    // given
    let fake = Fake::new();
    fake.faults("[[rule]]\ncommand = \"create\"\naction = \"hang_after_effect\"\n");
    let mut child = fake.spawn(&["create", "base"]);
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let mut line = String::new();
    stderr
        .read_line(&mut line)
        .expect("the hang announces itself");

    // when
    child.kill().expect("kill");
    child.wait().expect("reap");

    // then
    assert!(
        fake.vm("base").is_some(),
        "the effect happened before the hang"
    );
    assert_eq!(fake.events("fault")[0]["phase"], json!("after_effect"));
    assert!(fake.events("exit").is_empty());
}

#[test]
fn should_fail_without_effect_when_the_action_is_fail_before_effect() {
    // given
    let fake = Fake::new();
    fake.faults(
        "[[rule]]\ncommand = \"create\"\naction = \"fail_before_effect\"\nexit_code = 3\nstderr = \"disk full\"\n",
    );

    // when
    let ran = fake.tart(&["create", "base"]);

    // then
    assert_eq!(
        (ran.code(), ran.stderr().as_str()),
        (Some(3), "disk full\n")
    );
    assert!(fake.vm("base").is_none());
    assert_eq!(fake.events("exit")[0]["code"], json!(3));
    assert!(fake.events("effect").is_empty());
}

#[test]
fn should_fail_with_the_effect_done_when_the_action_is_fail_after_effect() {
    // given
    let fake = Fake::new();
    fake.faults("[[rule]]\ncommand = \"create\"\naction = \"fail_after_effect\"\n");

    // when
    let ran = fake.tart(&["create", "base"]);

    // then
    assert_eq!(
        (ran.code(), ran.stderr().as_str()),
        (Some(1), "fake-tart: injected failure\n")
    );
    assert!(
        fake.vm("base").is_some(),
        "the VM exists although the command failed"
    );
    assert_eq!(fake.events("fault")[0]["phase"], json!("after_effect"));
}

#[test]
fn should_die_by_abort_after_the_effect_when_the_action_is_crash_after_effect() {
    // given
    let fake = Fake::new();
    fake.faults("[[rule]]\ncommand = \"create\"\naction = \"crash_after_effect\"\n");

    // when
    let ran = fake.tart(&["create", "base"]);

    // then
    assert_eq!(ran.output.status.signal(), Some(6));
    assert!(fake.vm("base").is_some());
    assert!(fake.events("exit").is_empty());
    assert_eq!(
        fake.events("fault")[0]["action"],
        json!("crash_after_effect")
    );
}

#[test]
fn should_damage_stdout_after_the_effect_when_the_action_is_corrupt_output() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    let healthy = fake.tart(&["list", "--format", "json"]).stdout();
    let modes = ["truncate", "garbage", "empty"];

    for mode in modes {
        fake.faults(&format!(
            "[[rule]]\ncommand = \"list\"\naction = \"corrupt_output\"\ncorrupt = \"{mode}\"\n"
        ));

        // when
        let ran = fake.tart(&["list", "--format", "json"]);

        // then
        assert_eq!(ran.code(), Some(0), "{mode}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&ran.stdout()).is_err(),
            "{mode}"
        );
        if mode == "truncate" {
            assert!(healthy.starts_with(&ran.stdout()) && ran.stdout().len() < healthy.len());
        }
        if mode == "empty" {
            assert_eq!(ran.stdout(), "");
        }
    }
}

#[test]
fn should_print_the_overridden_version_when_a_rule_sets_one() {
    // given
    let fake = Fake::new();
    fake.faults("[[rule]]\ncommand = \"--version\"\nversion = \"9.9.9\"\ntimes = 1\n");

    // when
    let first = fake.tart(&["--version"]);
    let second = fake.tart(&["--version"]);

    // then
    assert_eq!(
        (first.stdout().as_str(), second.stdout().as_str()),
        ("9.9.9\n", "2.41.1\n")
    );
}

#[test]
fn should_fire_once_across_processes_when_the_rule_is_one_shot_and_every_time_when_sticky() {
    // given
    let fake = Fake::new();
    fake.faults(
        "[[rule]]\nid = \"once\"\ncommand = \"create\"\naction = \"fail_before_effect\"\ntimes = 1\n\
         [[rule]]\nid = \"always\"\ncommand = \"stop\"\naction = \"fail_before_effect\"\n",
    );

    // when
    let creates: Vec<_> = (0..2)
        .map(|_| fake.tart(&["create", "base"]).code())
        .collect();
    let stops: Vec<_> = (0..3)
        .map(|_| fake.tart(&["stop", "base"]).code())
        .collect();

    // then
    assert_eq!(creates, [Some(1), Some(0)]);
    assert_eq!(stops, [Some(1), Some(1), Some(1)]);
}

#[test]
fn should_skip_the_first_matches_and_target_one_vm_when_the_rule_narrows() {
    // given
    let fake = Fake::new();
    fake.with_vm("a");
    fake.tart(&["clone", "base", "b"]);
    fake.faults(
        "[[rule]]\ncommand = \"get\"\nvm = \"b\"\nskip = 1\naction = \"fail_before_effect\"\n",
    );

    // when
    let on_a = fake.tart(&["get", "a"]).code();
    let first_b = fake.tart(&["get", "b"]).code();
    let second_b = fake.tart(&["get", "b"]).code();

    // then
    assert_eq!((on_a, first_b, second_b), (Some(0), Some(0), Some(1)));
}

#[test]
fn should_run_a_guest_job_with_the_scripted_output_when_a_rule_describes_it() {
    // given
    let fake = Fake::new();
    fake.with_vm("vm1");
    fake.faults(
        "run_mode = \"detached\"\n[[rule]]\ncommand = \"exec\"\narg0 = \"/opt/launcher\"\n\
         job_ms = 20\njob_exit_code = 7\njob_stdout = \"job done\"\n",
    );
    fake.tart(&["run", "vm1"]);

    // when
    let launcher = fake.tart_with_stdin(&["exec", "-i", "vm1", "/opt/launcher"], b"jit");
    let probe = fake.tart(&["exec", "vm1", "--", "true"]);

    // then
    assert_eq!(
        (launcher.code(), launcher.stdout().as_str()),
        (Some(7), "job done\n")
    );
    assert_eq!((probe.code(), probe.stdout().as_str()), (Some(0), ""));
}

#[test]
fn should_refuse_and_still_log_the_invocation_when_the_fault_file_is_invalid() {
    // given
    let fake = Fake::new();
    fake.faults("[[rule]]\ncommand = \"create\"\naction = \"explode\"\n");

    // when
    let ran = fake.tart(&["create", "base"]);

    // then
    assert_eq!(ran.code(), Some(99));
    assert!(
        ran.stderr().contains("faults.toml is invalid"),
        "{}",
        ran.stderr()
    );
    assert_eq!(fake.events("invoke").len(), 1);
    assert!(fake.vm("base").is_none());
}

#[test]
fn should_log_a_usage_error_without_consulting_the_fault_file_when_the_arguments_are_wrong() {
    // given
    let fake = Fake::new();
    fake.faults("[[rule]]\naction = \"hang\"\n");

    // when
    let ran = fake.tart(&["reboot"]);

    // then
    assert_eq!(ran.code(), Some(64));
    assert_eq!(fake.events("exit")[0]["code"], json!(64));
}
