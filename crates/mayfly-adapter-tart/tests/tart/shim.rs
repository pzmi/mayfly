//! The per-VM shim (ADR 0037) with the real `mayfly vm-shim` and the fake `tart`: what its files
//! say while the VM runs and after `tart stop`, that `tart run`'s stdio are files, that a
//! hypervisor refusal is told apart, and that a daemon killed while a VM runs leaves the VM and
//! its shim running for a restarted daemon to re-adopt (F48 at integration level).

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use mayfly_adapter_tart::TartProvider;
use mayfly_data::{Shim, VmError, VmProvider};
use mayfly_domain::{Millis, ProviderInstanceId};
use mayfly_testkit_data::provider_contract::{START_MEMORY, base, proof, vm};
use mayfly_testkit_shared::poll::wait_until;

use crate::support::Fake;

/// The variable that makes [`should_start_a_vm_then_wait_to_be_killed_when_run_as_the_daemon`]
/// act as the daemon: the shared directory of the parent test's fake.
const DAEMON_ROLE: &str = "TART_ADAPTER_TEST_DAEMON_DIR";

/// How long a test waits for a shim to notice its run ended.
const SHIM_EXIT: Duration = Duration::from_secs(20);

/// The helper daemon, killed and reaped when dropped, so a failing test leaves no process.
struct Daemon(Child);

impl Daemon {
    fn kill(&mut self) {
        let _killed = self.0.kill();
        let _reaped = self.0.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.kill();
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

fn options() -> mayfly_data::RunOptions {
    mayfly_data::RunOptions {
        resources: mayfly_domain::Resources::new(START_MEMORY, 0),
    }
}

async fn clone_and_start(provider: &TartProvider) -> ProviderInstanceId {
    let id = provider.clone_vm(&base(), &vm(1)).await.expect("clone");
    provider.start(&vm(1), &options()).await.expect("start");
    id
}

#[test]
fn should_read_alive_then_exited_when_the_shim_runs_and_tart_stop_ends_the_run() {
    // given
    let fake = Fake::new();
    fake.seed_base(base().as_str());
    let provider = fake.provider();
    let runtime = runtime();
    let id = runtime.block_on(clone_and_start(&provider));
    let name = vm(1).to_string();
    let running = provider.shim(&name);

    // when
    runtime
        .block_on(provider.stop(&proof(1, &id), Millis::new(5_000)))
        .expect("stop");
    wait_until(SHIM_EXIT, "the shim to write exit.json", || {
        provider.shim(&name).shim == Shim::Exited
    });

    // then
    assert_eq!(running.shim, Shim::Alive);
    let run = running.run.expect("shim.json");
    let reading = provider.shim(&name);
    let exit = reading.exit.expect("exit.json");
    assert_eq!(exit.run_id, run.run_id);
    assert_eq!((exit.code, exit.signal), (Some(0), None), "{exit:?}");
    assert!(fake.events("signal").is_empty());
    let runs: Vec<_> = fake
        .events("invoke")
        .into_iter()
        .filter(|record| record["argv"][0] == "run")
        .collect();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["pid"], serde_json::json!(run.child_pid));
}

#[test]
fn should_reap_and_forget_the_shim_when_its_vm_has_stopped() {
    // given
    let fake = Fake::new();
    fake.seed_base(base().as_str());
    let provider = fake.provider();
    let runtime = runtime();
    let id = runtime.block_on(clone_and_start(&provider));
    let name = vm(1).to_string();
    let while_running = provider.shim_children();
    let shim_pid = provider.shim(&name).run.expect("shim.json").shim_pid;

    // when
    runtime
        .block_on(provider.stop(&proof(1, &id), Millis::new(5_000)))
        .expect("stop");
    wait_until(SHIM_EXIT, "the shim to exit after its VM stopped", || {
        provider.shim(&name).shim == Shim::Exited
    });
    wait_until(SHIM_EXIT, "the registry to reap the exited shim", || {
        provider.shim_children() == 0
    });

    // then
    assert_eq!(while_running, 1);
    assert_eq!(
        rustix::process::waitpid(
            rustix::process::Pid::from_raw(i32::try_from(shim_pid).expect("pid")),
            rustix::process::WaitOptions::NOHANG
        )
        .map(|status| status.is_some()),
        Err(rustix::io::Errno::CHILD),
        "the shim is reaped: no longer a child of this process, so not a zombie"
    );
}

#[test]
fn should_stop_reporting_the_shim_when_its_vm_is_deleted() {
    // given
    let fake = Fake::new();
    fake.seed_base(base().as_str());
    let provider = fake.provider();
    let runtime = runtime();
    let id = runtime.block_on(clone_and_start(&provider));
    let name = vm(1).to_string();
    runtime
        .block_on(provider.stop(&proof(1, &id), Millis::new(5_000)))
        .expect("stop");
    wait_until(SHIM_EXIT, "the shim to exit after its VM stopped", || {
        provider.shim(&name).shim == Shim::Exited
    });

    // when
    runtime
        .block_on(provider.delete(&proof(1, &id)))
        .expect("delete");

    // then
    assert!(provider.shims().is_empty(), "{:?}", provider.shims());
}

#[cfg(target_os = "linux")]
#[test]
fn should_give_tart_run_files_for_stdio_never_a_pipe_when_the_shim_starts_it() {
    // given
    let fake = Fake::new();
    fake.seed_base(base().as_str());
    let provider = fake.provider();
    let runtime = runtime();
    runtime.block_on(clone_and_start(&provider));
    let reading = provider.shim(&vm(1).to_string());
    let run = reading.run.expect("shim.json");

    // when
    let stdio: Vec<PathBuf> = [0, 1, 2]
        .iter()
        .map(|fd| {
            std::fs::read_link(format!("/proc/{}/fd/{fd}", run.child_pid)).expect("an fd link")
        })
        .collect();

    // then
    let dir = fake.shims().join(vm(1).to_string());
    assert_eq!(stdio[0], PathBuf::from("/dev/null"));
    assert_eq!(stdio[1], dir.join("run.stdout"));
    assert_eq!(stdio[2], dir.join("run.stderr"));
}

#[test]
fn should_answer_hypervisor_limit_when_tart_run_reports_the_vm_limit() {
    // given
    let fake = Fake::new();
    fake.seed_base(base().as_str());
    fake.faults(
        "[[rule]]\ncommand = \"run\"\naction = \"fail_before_effect\"\nexit_code = 1\nstderr = \"The number of VMs exceeds the system limit (other running VMs: mf-other)\"\n",
    );
    let provider = fake.provider();
    let runtime = runtime();
    runtime
        .block_on(provider.clone_vm(&base(), &vm(1)))
        .expect("clone");

    // when
    let started = runtime.block_on(provider.start(&vm(1), &options()));

    // then
    assert_eq!(started, Err(VmError::HypervisorLimit));
    assert_eq!(fake.exists(&vm(1).to_string()), Some(false));
    assert_eq!(provider.shim(&vm(1).to_string()).shim, Shim::Exited);
}

#[test]
fn should_refuse_definite_when_tart_run_exits_non_zero_at_once() {
    // given
    let fake = Fake::new();
    fake.seed_base(base().as_str());
    fake.faults("[[rule]]\ncommand = \"run\"\naction = \"fail_before_effect\"\nexit_code = 1\n");
    let provider = fake.provider();
    let runtime = runtime();
    runtime
        .block_on(provider.clone_vm(&base(), &vm(1)))
        .expect("clone");

    // when
    let started = runtime.block_on(provider.start(&vm(1), &options()));

    // then
    assert_eq!(started, Err(VmError::Definite));
}

/// A sub-process entry, not a test of its own: the re-adoption test below runs this test
/// executable again with only this test selected and [`DAEMON_ROLE`] set, and this process is
/// then the daemon that starts a VM and is killed. A helper binary would need a `[[bin]]` in a
/// production crate; re-running the test executable keeps the helper test-only. Without the
/// variable (every ordinary run) it returns at once.
#[test]
fn should_start_a_vm_then_wait_to_be_killed_when_run_as_the_daemon() {
    // given
    let Some(dir) = std::env::var_os(DAEMON_ROLE) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let provider = crate::support::provider_at(&dir);

    // when
    let id = runtime().block_on(clone_and_start(&provider));

    // then
    println!("started {}", id.as_str());
    loop {
        std::thread::park();
    }
}

#[test]
fn should_readopt_a_running_vm_through_its_shim_when_the_daemon_is_killed_and_restarted() {
    // given
    let fake = Fake::new();
    fake.seed_base(base().as_str());
    let mut daemon = Daemon(
        Command::new(std::env::current_exe().expect("this test executable"))
            .args([
                "--exact",
                "shim::should_start_a_vm_then_wait_to_be_killed_when_run_as_the_daemon",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(DAEMON_ROLE, fake.dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the daemon"),
    );
    let stdout = daemon.0.stdout.take().expect("the daemon's stdout");
    let started = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .find_map(|line| {
            line.rsplit_once("started ")
                .map(|(_, id)| id.trim().to_owned())
        })
        .expect("the daemon started a VM");
    let id = ProviderInstanceId::new(&started).expect("an instance id");

    // when
    daemon.kill();
    let restarted = fake.provider();
    let name = vm(1).to_string();
    let adopted = restarted.shim(&name);
    let runtime = runtime();
    let inventory = runtime.block_on(restarted.inventory());
    let stopped = runtime.block_on(restarted.stop(&proof(1, &id), Millis::new(5_000)));
    wait_until(SHIM_EXIT, "the adopted shim to write exit.json", || {
        restarted.shim(&name).shim == Shim::Exited
    });

    // then
    assert_eq!(adopted.shim, Shim::Alive, "the shim outlived its daemon");
    let listed = match inventory {
        mayfly_data::Inventory::Complete { vms } => vms,
        other => panic!("a complete listing, got {other:?}"),
    };
    let seen = listed.iter().find(|v| v.name == name).expect("listed");
    assert!(seen.running);
    assert_eq!(seen.instance_id.as_ref(), Some(&id));
    assert_eq!(stopped, Ok(()));
    let exit = restarted.shim(&name).exit.expect("exit.json");
    assert_eq!(exit.run_id, adopted.run.expect("shim.json").run_id);
    assert_eq!((exit.code, exit.signal), (Some(0), None), "{exit:?}");
    assert!(fake.events("signal").is_empty());
}
