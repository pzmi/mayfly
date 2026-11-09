//! The commands' effects on the state directory.
//!
//! Each command takes the directory lock only around its read-modify-write; waiting (a foreground
//! `run`, `ip --wait`, `stop`'s grace period, an `exec` job) happens after the lock is released,
//! so one hung invocation never blocks the others.
//!
//! Output shapes follow Tart's sources (public repository, `main` branch, read 2026-11-09):
//! `Commands/List.swift` (`Source`, `Name`, `Disk`, `Size`, `Accessed`, `Running`, `State`),
//! `Commands/Get.swift` (`OS`, `CPU`, `Memory`, `Disk`, `DiskFormat`, `Size`, `Display`,
//! `MACAddress`, `Running`, `State`), `Commands/IP.swift`, `Commands/Stop.swift`,
//! `Commands/Delete.swift`, `Commands/Clone.swift`. **Unknown, marked as such:** how
//! `HumanReadableByteCount` encodes `Disk` and `Size` (the fake prints whole gigabytes as numbers
//! in `list`, and `Size` as a three-decimal string in `get`, which is what `Get.swift`'s closure
//! formats), the pretty-printing whitespace (Foundation's differs from serde_json's), and the
//! `--version` string of any real release (the fake prints [`DEFAULT_VERSION`], which is not
//! captured from a binary). `tart get` shows a MAC address and no creation time; the creation time
//! in a VM record is the fake's own and is never printed by a Tart-shaped command.
//!
//! Pinned by: `tests/cli/commands.rs` (one test per command) and `tests/fixtures/*.json` through
//! `tests/cli/commands.rs::should_print_the_pinned_list_shape_when_listing_json`.

use std::io::Read;
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::cli::{Command, Format};
use crate::fail::Fail;
use crate::faults::{Rule, RunMode};
use crate::home::Home;
use crate::log::{Event, Record};
use crate::timefmt::{iso8601, now_ms};
use crate::vm::{
    Launch, MAX_LAUNCHES, Run, Source, VmRecord, hex, kill_hard, pid_alive, pid_start, pseudo_mac,
};
use crate::wait::pause;

/// The version `--version` prints unless a fault rule overrides it.
pub(crate) const DEFAULT_VERSION: &str = "2.28.0";

/// What a command still has to wait for after its effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Wait {
    /// A foreground `run`: until the VM's `run` entry is gone or replaced.
    RunUntilStopped { name: String, run_id: u64 },
    /// An `exec` guest job: this long, unless the VM stops first.
    GuestJob { name: String, ms: u64 },
}

/// A command that took effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Done {
    pub code: i32,
    pub stdout: String,
    /// Printed on stderr before `wait` begins, as a guest job's progress would be.
    pub stderr: String,
    pub wait: Option<Wait>,
}

impl Done {
    fn out(stdout: String) -> Self {
        Self {
            code: 0,
            stdout,
            stderr: String::new(),
            wait: None,
        }
    }

    fn quiet() -> Self {
        Self::out(String::new())
    }
}

/// Everything a command may use besides its arguments.
pub(crate) struct Ctx<'a> {
    pub home: &'a Home,
    pub inv: u64,
    pub pid: u32,
    pub run_mode: RunMode,
    pub rule: Option<&'a Rule>,
    pub stdin: &'a mut dyn Read,
}

impl Ctx<'_> {
    fn effect(&self, kind: &str, vm: &str) -> Result<(), Fail> {
        let event = Event::Effect {
            kind: kind.to_owned(),
            vm: vm.to_owned(),
        };
        self.home
            .append_log(&Record::new(self.inv, now_ms(), event))
    }
}

/// Runs the command's effect.
pub(crate) fn execute(command: &Command, ctx: &mut Ctx<'_>) -> Result<Done, Fail> {
    match command {
        Command::Version => Ok(Done::out(format!(
            "{}\n",
            ctx.rule
                .and_then(|r| r.version.as_deref())
                .unwrap_or(DEFAULT_VERSION)
        ))),
        Command::Create { name } => create(ctx, name),
        Command::Pull { reference } => pull(ctx, reference),
        Command::Clone {
            source,
            name,
            overwrite,
        } => clone_vm(ctx, source, name, *overwrite),
        Command::Run { name } => run(ctx, name),
        Command::Ip { name, wait } => ip(ctx, name, *wait),
        Command::List {
            format,
            source,
            quiet,
        } => list(ctx, *format, source.as_deref(), *quiet),
        Command::Get { name, format } => get(ctx, name, *format),
        Command::Stop { name, timeout } => stop(ctx, name, *timeout),
        Command::Delete { names } => delete(ctx, names),
        Command::Exec {
            name,
            interactive,
            argv,
        } => exec(ctx, name, *interactive, argv),
    }
}

/// A local VM by name; an OCI-cache entry is not a local VM, as in `VMStorageLocal.open`.
fn local_vm(home: &Home, name: &str) -> Result<VmRecord, Fail> {
    match home.read_vm(name)? {
        Some(vm) if vm.source == Source::Local => Ok(vm),
        _ => Err(Fail::does_not_exist(name)),
    }
}

/// Reserves the next allocation counter (MAC and address) in `state.json`.
fn next_counter(home: &Home) -> Result<u64, Fail> {
    let mut state = home.read_state()?;
    let counter = state.next_vm;
    state.next_vm += 1;
    home.write_state(&state)?;
    Ok(counter)
}

fn octet(counter: u64) -> u8 {
    u8::try_from(2 + counter % 250).unwrap_or(2)
}

fn create(ctx: &Ctx<'_>, name: &str) -> Result<Done, Fail> {
    let _lock = ctx.home.lock()?;
    if ctx.home.read_vm(name)?.is_some() {
        return Err(Fail::generic(
            "VM directory is already initialized, preventing overwrite",
        ));
    }
    let counter = next_counter(ctx.home)?;
    let vm = VmRecord::new(
        name,
        Source::Local,
        pseudo_mac(name, counter),
        octet(counter),
        now_ms(),
    );
    ctx.home.write_vm(&vm)?;
    ctx.effect("vm_created", name)?;
    Ok(Done::quiet())
}

fn pull(ctx: &Ctx<'_>, reference: &str) -> Result<Done, Fail> {
    let _lock = ctx.home.lock()?;
    if ctx.home.read_vm(reference)?.is_none() {
        let counter = next_counter(ctx.home)?;
        let vm = VmRecord::new(
            reference,
            Source::Oci,
            pseudo_mac(reference, counter),
            octet(counter),
            now_ms(),
        );
        ctx.home.write_vm(&vm)?;
        ctx.effect("image_pulled", reference)?;
    }
    Ok(Done::quiet())
}

/// `clone` reads its source from local storage or the OCI cache and never pulls. The clone gets a
/// fresh MAC when a local VM already has the source's (`Clone.swift`'s `generateMAC`): always for a
/// local source, and for an OCI image only from its second clone on.
fn clone_vm(ctx: &Ctx<'_>, source: &str, name: &str, overwrite: bool) -> Result<Done, Fail> {
    let _lock = ctx.home.lock()?;
    let src = ctx
        .home
        .read_vm(source)?
        .ok_or_else(|| Fail::does_not_exist(source))?;
    if let Some(existing) = ctx.home.read_vm(name)? {
        if !overwrite {
            return Err(Fail::validation(&format!(
                "VM \"{name}\" already exists, use --overwrite to replace it"
            )));
        }
        if existing.running() {
            return Err(Fail::is_running(name));
        }
    }
    let counter = next_counter(ctx.home)?;
    let shares_mac = ctx
        .home
        .list_vms()?
        .iter()
        .any(|vm| vm.source == Source::Local && vm.mac == src.mac && vm.name != name);
    let mac = if shares_mac {
        pseudo_mac(name, counter)
    } else {
        src.mac.clone()
    };
    let mut vm = VmRecord::new(name, Source::Local, mac, octet(counter), now_ms());
    vm.cloned_from = Some(source.to_owned());
    vm.os.clone_from(&src.os);
    (vm.cpu, vm.memory_mb, vm.disk_gb) = (src.cpu, src.memory_mb, src.disk_gb);
    ctx.home.write_vm(&vm)?;
    ctx.effect("vm_cloned", name)?;
    Ok(Done::quiet())
}

/// `run` in the configured [`RunMode`]. Foreground: the VM runs while this process lives, and the
/// process returns when `stop` or `delete` takes the VM away. Detached: the VM is recorded as
/// running and the process exits at once, so nothing needs to survive a test or a killed daemon.
fn run(ctx: &Ctx<'_>, name: &str) -> Result<Done, Fail> {
    let _lock = ctx.home.lock()?;
    let mut vm = local_vm(ctx.home, name)?;
    if vm.running() {
        return Err(Fail::already_running(name));
    }
    let detached = ctx.run_mode == RunMode::Detached;
    vm.run = Some(Run {
        id: ctx.inv,
        pid: (!detached).then_some(ctx.pid),
        pid_start: if detached { None } else { pid_start(ctx.pid) },
        mode: ctx.run_mode,
        started_at_ms: now_ms(),
    });
    vm.accessed_ms = now_ms();
    ctx.home.write_vm(&vm)?;
    ctx.effect("vm_started", name)?;
    let mut done = Done::quiet();
    if !detached {
        done.wait = Some(Wait::RunUntilStopped {
            name: name.to_owned(),
            run_id: ctx.inv,
        });
    }
    Ok(done)
}

fn ip(ctx: &Ctx<'_>, name: &str, wait: u64) -> Result<Done, Fail> {
    let vm = local_vm(ctx.home, name)?;
    let found = vm.running()
        || (wait > 0
            && poll(Some(Duration::from_secs(wait)), || {
                local_vm(ctx.home, name).is_ok_and(|v| v.running())
            }));
    if !found {
        return Err(Fail::generic("no IP address found, is your VM running?"));
    }
    let vm = local_vm(ctx.home, name)?;
    Ok(Done::out(format!("{}\n", vm.ip())))
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ListEntry {
    source: &'static str,
    name: String,
    disk: u64,
    size: u64,
    accessed: String,
    running: bool,
    state: &'static str,
}

#[derive(Serialize)]
struct GetInfo {
    #[serde(rename = "OS")]
    os: String,
    #[serde(rename = "CPU")]
    cpu: u64,
    #[serde(rename = "Memory")]
    memory: u64,
    #[serde(rename = "Disk")]
    disk: u64,
    #[serde(rename = "DiskFormat")]
    disk_format: &'static str,
    #[serde(rename = "Size")]
    size: String,
    #[serde(rename = "Display")]
    display: &'static str,
    #[serde(rename = "MACAddress")]
    mac_address: String,
    #[serde(rename = "Running")]
    running: bool,
    #[serde(rename = "State")]
    state: &'static str,
}

const SIZE_GB: u64 = 25;

fn json<T: Serialize>(value: &T) -> Result<String, Fail> {
    serde_json::to_string_pretty(value)
        .map(|text| text + "\n")
        .map_err(|e| Fail::fake(&format!("cannot encode output: {e}")))
}

fn list(ctx: &Ctx<'_>, format: Format, source: Option<&str>, quiet: bool) -> Result<Done, Fail> {
    let mut vms = ctx.home.list_vms()?;
    vms.retain(|vm| match source {
        Some("local") => vm.source == Source::Local,
        Some(_) => vm.source == Source::Oci,
        None => true,
    });
    vms.sort_by(|a, b| (a.source == Source::Oci, &a.name).cmp(&(b.source == Source::Oci, &b.name)));
    if quiet {
        return Ok(Done::out(
            vms.iter().map(|vm| format!("{}\n", vm.name)).collect(),
        ));
    }
    let entries: Vec<ListEntry> = vms
        .iter()
        .map(|vm| ListEntry {
            source: if vm.source == Source::Local {
                "local"
            } else {
                "OCI"
            },
            name: vm.name.clone(),
            disk: vm.disk_gb,
            size: SIZE_GB,
            accessed: iso8601(vm.accessed_ms),
            running: vm.running(),
            state: vm.state(),
        })
        .collect();
    match format {
        Format::Json => Ok(Done::out(json(&entries)?)),
        Format::Text => {
            let mut out = String::new();
            if !entries.is_empty() {
                out.push_str("Source  Name  Disk  Size  Accessed  State\n");
            }
            for e in &entries {
                out.push_str(&format!(
                    "{}  {}  {}  {}  {}  {}\n",
                    e.source, e.name, e.disk, e.size, e.accessed, e.state
                ));
            }
            Ok(Done::out(out))
        }
    }
}

fn get(ctx: &Ctx<'_>, name: &str, format: Format) -> Result<Done, Fail> {
    let vm = local_vm(ctx.home, name)?;
    let info = GetInfo {
        os: vm.os.clone(),
        cpu: vm.cpu,
        memory: vm.memory_mb,
        disk: vm.disk_gb,
        disk_format: "raw",
        size: format!("{SIZE_GB}.000"),
        display: "1024x768",
        mac_address: vm.mac.clone(),
        running: vm.running(),
        state: vm.state(),
    };
    match format {
        Format::Json => Ok(Done::out(json(&info)?)),
        Format::Text => Ok(Done::out(format!(
            "OS  CPU  Memory  Disk  DiskFormat  Size  Display  MACAddress  State\n{}  {}  {}  {}  {}  {}  {}  {}  {}\n",
            info.os,
            info.cpu,
            info.memory,
            info.disk,
            info.disk_format,
            info.size,
            info.display,
            info.mac_address,
            info.state
        ))),
    }
}

/// `stop`: the VM leaves the running state at once; a foreground `run` process gets `timeout`
/// seconds to notice and exit, then SIGKILL (`Stop.swift` sends SIGINT, waits, then kills; the fake
/// has no signal handler to interrupt, so the state change is the interrupt). The process is never
/// the caller's own, so an in-process test cannot kill itself.
fn stop(ctx: &Ctx<'_>, name: &str, timeout: u64) -> Result<Done, Fail> {
    let runner = {
        let _lock = ctx.home.lock()?;
        let mut vm = local_vm(ctx.home, name)?;
        if !vm.running() {
            return Err(Fail::not_running(name));
        }
        let run = vm.run.take();
        ctx.home.write_vm(&vm)?;
        ctx.effect("vm_stopped", name)?;
        run
    };
    if let Some(Run {
        pid: Some(pid),
        pid_start,
        ..
    }) = runner
        && pid != std::process::id()
        && !poll(Some(Duration::from_secs(timeout)), || {
            !pid_alive(pid, pid_start)
        })
    {
        kill_hard(pid);
    }
    Ok(Done::quiet())
}

fn delete(ctx: &Ctx<'_>, names: &[String]) -> Result<Done, Fail> {
    for name in names {
        let _lock = ctx.home.lock()?;
        let vm = ctx
            .home
            .read_vm(name)?
            .ok_or_else(|| Fail::does_not_exist(name))?;
        if vm.running() {
            return Err(Fail::is_running(name));
        }
        ctx.home.remove_vm(name)?;
        ctx.effect("vm_deleted", name)?;
    }
    Ok(Done::quiet())
}

/// `exec`: an emulated guest. With `-i` it reads stdin to the end, keeping only its length and
/// SHA-256 (in the VM record and as a `stdin` log event); without `-i` stdin is not read, as in
/// real Tart. The guest does not interpret the command: the matching rule's `job_*` fields decide
/// how long the "job" runs, what it prints and its exit code. A VM that stops during the job
/// fails the exec the way a lost control socket does.
fn exec(ctx: &mut Ctx<'_>, name: &str, interactive: bool, argv: &[String]) -> Result<Done, Fail> {
    let vm = local_vm(ctx.home, name)?;
    if !vm.running() {
        return Err(Fail::not_running(name));
    }
    let stdin = if interactive {
        Some(digest_stdin(ctx)?)
    } else {
        None
    };
    {
        let _lock = ctx.home.lock()?;
        let mut vm = local_vm(ctx.home, name)?;
        vm.guest_launches.push(Launch {
            at_ms: now_ms(),
            argv: argv.to_vec(),
            stdin_len: stdin.as_ref().map(|(len, _)| *len),
            stdin_sha256: stdin.map(|(_, digest)| digest),
        });
        let excess = vm.guest_launches.len().saturating_sub(MAX_LAUNCHES);
        vm.guest_launches.drain(..excess);
        ctx.home.write_vm(&vm)?;
        ctx.effect("guest_launch", name)?;
    }
    let job = ctx.rule;
    Ok(Done {
        code: job.map_or(0, |r| r.job_exit_code),
        stdout: job
            .and_then(|r| r.job_stdout.clone())
            .map(|s| s + "\n")
            .unwrap_or_default(),
        stderr: job
            .and_then(|r| r.job_stderr.clone())
            .map(|s| s + "\n")
            .unwrap_or_default(),
        wait: job.filter(|r| r.job_ms > 0).map(|r| Wait::GuestJob {
            name: name.to_owned(),
            ms: r.job_ms,
        }),
    })
}

/// Reads stdin to its end, logs its length and digest, and returns them; the content is dropped
/// chunk by chunk and never held.
fn digest_stdin(ctx: &mut Ctx<'_>) -> Result<(u64, String), Fail> {
    let mut hasher = Sha256::new();
    let mut len = 0u64;
    let mut chunk = [0u8; 8192];
    loop {
        let n = ctx
            .stdin
            .read(&mut chunk)
            .map_err(|e| Fail::fake(&format!("cannot read stdin: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
        len += u64::try_from(n).unwrap_or(0);
    }
    let digest = hex(hasher.finalize().as_ref());
    let event = Event::Stdin {
        len,
        sha256: digest.clone(),
    };
    ctx.home
        .append_log(&Record::new(ctx.inv, now_ms(), event))?;
    Ok((len, digest))
}

/// Waits for what a command left pending; `Err` when the VM went away under an `exec` job.
pub(crate) fn finish_wait(home: &Home, wait: &Wait) -> Result<(), Fail> {
    match wait {
        Wait::RunUntilStopped { name, run_id } => {
            poll(None, || {
                home.read_vm(name)
                    .ok()
                    .flatten()
                    .is_none_or(|vm| vm.run.as_ref().is_none_or(|run| run.id != *run_id))
            });
            Ok(())
        }
        Wait::GuestJob { name, ms } => {
            let stopped = poll(Some(Duration::from_millis(*ms)), || {
                !home
                    .read_vm(name)
                    .ok()
                    .flatten()
                    .is_some_and(|vm| vm.running())
            });
            if stopped {
                Err(Fail::generic(
                    "Failed to connect to the VM using its control socket: the VM stopped, is the Tart Guest Agent running?",
                ))
            } else {
                Ok(())
            }
        }
    }
}

/// Polls `condition` every 20 ms until it holds (`true`) or `limit` passes (`false`); no limit
/// waits for ever. This is the fake's own waiting, not a test's.
fn poll(limit: Option<Duration>, mut condition: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    loop {
        if condition() {
            return true;
        }
        if limit.is_some_and(|l| started.elapsed() >= l) {
            return false;
        }
        pause(Duration::from_millis(20));
    }
}
