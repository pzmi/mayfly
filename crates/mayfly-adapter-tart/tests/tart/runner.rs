//! The process runner with real children: the deadline kills the whole group, a dropped call
//! kills it too, stdout is bounded, and the child's environment is exactly what it was given
//! (the EX-04 probe of `mayfly-testkit-shared::child_env`). The children here are `/bin/sh`
//! scripts written by the test: the runner itself never runs a shell.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use mayfly_adapter_tart::process::runner::{Call, Ran, run};
use mayfly_testkit_shared::child_env::{NOT_INHERITED, inherited_variable, probe_script};
#[cfg(target_os = "linux")]
use mayfly_testkit_shared::poll::wait_until;

const SH: &str = "/bin/sh";

/// How long a test waits for a killed process to be gone.
#[cfg(target_os = "linux")]
const GONE: Duration = Duration::from_secs(10);

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

fn sh(script: &str, deadline: Duration, output_limit: usize) -> Call<'static> {
    Call {
        program: Path::new(SH),
        args: vec![OsString::from("-c"), OsString::from(script)],
        environment: &[],
        deadline,
        output_limit,
        redact: &[],
    }
}

#[cfg(target_os = "linux")]
/// Whether `pid` is gone or a zombie nobody reaped yet (its parent was killed with it).
fn gone(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
        stat.rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state == "Z" || state == "X")
    })
}

#[cfg(target_os = "linux")]
fn read_pid(path: &Path) -> i32 {
    wait_until(GONE, "the script to write its grandchild's pid", || {
        std::fs::read_to_string(path).is_ok_and(|text| text.ends_with('\n'))
    });
    std::fs::read_to_string(path)
        .expect("the pid file")
        .trim()
        .parse()
        .expect("a pid")
}

#[cfg(target_os = "linux")]
#[test]
fn should_kill_the_whole_group_and_answer_timed_out_when_the_deadline_passes() {
    // given
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("pid");
    let script = format!("sleep 600 & echo $! > {}; wait", pid_file.display());
    let call = sh(&script, Duration::from_millis(500), 1024);

    // when
    let ran = runtime().block_on(run(call));

    // then
    assert_eq!(ran, Ran::TimedOut);
    let grandchild = read_pid(&pid_file);
    wait_until(GONE, "the grandchild to be killed with its group", || {
        gone(grandchild)
    });
}

#[cfg(target_os = "linux")]
#[test]
fn should_kill_the_group_when_the_call_is_dropped() {
    // given
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("pid");
    let script = format!("sleep 600 & echo $! > {}; wait", pid_file.display());
    let call = sh(&script, Duration::from_secs(600), 1024);
    let runtime = runtime();

    // when
    let dropped = runtime.block_on(async {
        tokio::select! {
            ran = run(call) => Some(ran),
            () = async {
                while std::fs::read_to_string(&pid_file).map_or(true, |t| !t.ends_with('\n')) {
                    tokio::task::yield_now().await;
                }
            } => None,
        }
    });

    // then
    assert_eq!(dropped, None);
    let grandchild = read_pid(&pid_file);
    wait_until(
        GONE,
        "the grandchild to be killed when the call was dropped",
        || gone(grandchild),
    );
}

#[test]
fn should_keep_only_the_limit_and_mark_truncated_when_stdout_is_longer() {
    // given
    let call = sh("head -c 100000 /dev/zero", Duration::from_secs(30), 1000);

    // when
    let ran = runtime().block_on(run(call));

    // then
    let Ran::Exited(exited) = ran else {
        panic!("an exit, got {ran:?}");
    };
    assert!(exited.success());
    assert_eq!(exited.stdout.len(), 1000);
    assert!(exited.truncated);
}

#[test]
fn should_give_the_child_only_the_environment_it_is_given_when_the_caller_has_more() {
    // given
    let inherited = inherited_variable().expect("a variable of this test's environment");
    let script = probe_script(&inherited);
    let call = sh(&script, Duration::from_secs(30), 1024);

    // when
    let ran = runtime().block_on(run(call));

    // then
    let Ran::Exited(exited) = ran else {
        panic!("an exit, got {ran:?}");
    };
    assert_eq!(String::from_utf8_lossy(&exited.stdout), NOT_INHERITED);
}

#[test]
fn should_answer_spawn_failed_when_the_program_does_not_exist() {
    // given
    let call = Call {
        program: Path::new("/nonexistent/tart"),
        args: Vec::new(),
        environment: &[],
        deadline: Duration::from_secs(1),
        output_limit: 1024,
        redact: &[],
    };

    // when
    let ran = runtime().block_on(run(call));

    // then
    assert_eq!(ran, Ran::SpawnFailed(std::io::ErrorKind::NotFound));
}

#[test]
fn should_redact_a_secret_from_stderr_when_the_child_prints_it() {
    // given
    let secrets = vec!["s3cr3t-value".to_owned()];
    let call = Call {
        redact: &secrets,
        ..sh(
            "echo 'token s3cr3t-value refused' >&2; exit 3",
            Duration::from_secs(30),
            1024,
        )
    };

    // when
    let ran = runtime().block_on(run(call));

    // then
    let Ran::Exited(exited) = ran else {
        panic!("an exit, got {ran:?}");
    };
    assert_eq!(exited.code, Some(3));
    assert_eq!(exited.stderr, "token [redacted] refused\n");
}
