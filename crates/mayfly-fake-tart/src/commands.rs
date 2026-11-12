//! The commands' effects on the state directory.
//!
//! Each command takes the directory lock only around its read-modify-write; waiting (a foreground
//! `run`, `ip --wait`, `stop`'s grace period, an `exec` job) happens after the lock is released,
//! so one hung invocation never blocks the others.
//!
//! Output shapes follow Tart's sources at the `2.41.1` release tag (github.com/openai/tart, read
//! 2026-11-11): `Commands/List.swift` (`Source`, `Name`, `Disk`, `Size`, `Accessed`, `Running`,
//! `State`), `Commands/Get.swift` (`OS`, `CPU`, `Memory`, `Disk`, `DiskFormat`, `Size`,
//! `Display`, `MACAddress`, `Running`, `State`), `Commands/IP.swift`, `Commands/Stop.swift`,
//! `Commands/Delete.swift`, `Commands/Clone.swift`, `Commands/Set.swift`, `Commands/Pull.swift`
//! with `VMStorageOCI.swift`. `HumanReadableByteCount` encodes `Disk` and `Size` as the closure
//! each command gives it: whole decimal gigabytes as numbers in `list`, `Disk` likewise and
//! `Size` as a three-decimal string in `get` (`Disk` is `null` whenever Tart's read of the size
//! fails; the fake prints `null` for a running VM with an ASIF disk, the case the source names). **Unknown, marked as
//! such:** the pretty-printing whitespace and key order (Foundation's differ from serde_json's).
//! `--version` prints [`DEFAULT_VERSION`], the release the fake emulates (`CI.swift` prints the
//! release tag); it is derived from the source, not captured from a binary. `tart get` shows a
//! MAC address and no creation time; the creation time in a VM record is the fake's own and is
//! never printed by a Tart-shaped command.
//!
//! Pinned by: `tests/cli/commands.rs` (one test per command) and `tests/fixtures/*.json` through
//! `tests/cli/commands.rs::should_print_the_pinned_list_shape_when_listing_json`.

use std::fs::File;
use std::io::Read;
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::cli::{Command, Format};
use crate::fail::Fail;
use crate::faults::{Rule, RunMode};
use crate::home::{CONFIG, DISK, Home, NVRAM};
use crate::log::{Event, Record};
use crate::timefmt::{iso8601, now_ms};
use crate::vm::{
    Launch, MAX_LAUNCHES, Run, Source, VmRecord, hex, kill_hard, pid_alive, pid_start, pseudo_mac,
};
use crate::wait::pause;

/// The version `--version` prints unless a fault rule overrides it: the release the fake
/// emulates.
pub(crate) const DEFAULT_VERSION: &str = "2.41.1";

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
    pub(crate) fn out(stdout: String) -> Self {
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
    /// The standard output, for a guest that answers before it reads on (the launcher's
    /// handshake); everything else prints through [`Done`].
    pub stdout: &'a mut dyn std::io::Write,
    /// The standard error, likewise.
    pub stderr: &'a mut dyn std::io::Write,
    /// The VM's run lock a foreground `run` holds for as long as its process lives.
    pub held: Option<File>,
}

impl Ctx<'_> {
    pub(crate) fn effect(&self, kind: &str, vm: &str) -> Result<(), Fail> {
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
        Command::Create { name, linux, asif } => create(ctx, name, *linux, *asif),
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
        Command::Set {
            name,
            random_mac,
            random_serial,
            mac,
            cpu,
            memory,
            disk_size,
        } => set(
            ctx,
            name,
            Identity {
                random_mac: *random_mac,
                random_serial: *random_serial,
                mac: mac.as_deref(),
            },
            (*cpu, *memory, *disk_size),
        ),
        Command::Delete { names } => delete(ctx, names),
        Command::Exec {
            name,
            interactive,
            argv,
        } => exec(ctx, name, *interactive, argv),
    }
}

/// A local VM by name; an OCI-cache entry is not a local VM, as in `VMStorageLocal.open`.
pub(crate) fn local_vm(home: &Home, name: &str) -> Result<VmRecord, Fail> {
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

fn create(ctx: &Ctx<'_>, name: &str, linux: bool, asif: bool) -> Result<Done, Fail> {
    let _lock = ctx.home.lock()?;
    if ctx.home.read_vm(name)?.is_some() {
        return Err(Fail::runtime(
            "VM directory is already initialized, preventing overwrite",
        ));
    }
    let counter = next_counter(ctx.home)?;
    let mut vm = VmRecord::new(
        name,
        Source::Local,
        pseudo_mac(name, counter),
        octet(counter),
        now_ms(),
    );
    if linux {
        "linux".clone_into(&mut vm.os);
    }
    vm.disk_format = asif.then(|| "asif".to_owned());
    ctx.home.write_vm(&vm)?;
    // A macOS platform carries its machine identifier (`ecid`), which `set --random-serial`
    // replaces; a Linux one has none.
    let platform = if linux {
        String::new()
    } else {
        format!(r#","ecid":"{}""#, serial(name, counter))
    };
    let config = format!(
        r#"{{"version":1,"os":"{}","arch":"arm64","macAddress":"{}"{platform}}}"#,
        vm.os, vm.mac
    );
    let disk: Vec<u8> = name.bytes().cycle().take(4096).collect();
    ctx.home.write_vm_files(
        name,
        &[
            (CONFIG, config.as_bytes()),
            (NVRAM, format!("fake-nvram:{name}").as_bytes()),
            (DISK, &disk),
        ],
    )?;
    ctx.effect("vm_created", name)?;
    Ok(Done::quiet())
}

/// A synthetic machine identifier, distinct per allocation counter.
fn serial(name: &str, counter: u64) -> String {
    format!(
        "fake-ecid-{}",
        &hex(Sha256::digest(format!("{name}:{counter}")).as_ref())[..16]
    )
}

/// `pull`: an OCI-cache entry named by the remote name. With a seeded registry (`home`) it serves
/// only a reference the registry holds, as a registry answers an unknown manifest, and lays the
/// image out as Tart 2.41.1 does: the manifest's layout is checked as
/// `OCIManifest.tartDiskRepresentation` checks it, the entry is named by the hash of the manifest
/// it was served, never compared with the digest asked for, its `manifest.json` holds the bytes
/// served, and no blob is checked against its descriptor (`VMStorageOCI.pull`,
/// `VMDirectory+OCI.swift`, `Registry.pullBlob`). Verifying is the caller's job (domain-model
/// §9.3). An entry pulled by its own digest is marked explicitly pulled. Without a registry it
/// pulls anything, an image with no files.
fn pull(ctx: &Ctx<'_>, reference: &str) -> Result<Done, Fail> {
    let _lock = ctx.home.lock()?;
    let mut os = None;
    let mut own_digest = false;
    if ctx.home.registry_seeded() {
        let manifest = ctx.home.registry_manifest(reference)?.ok_or_else(|| {
            Fail::generic(&format!("failed to pull {reference}: manifest unknown"))
        })?;
        let (served_os, served_name) = lay_out(ctx.home, reference, &manifest)?;
        os = Some(served_os.clone());
        own_digest = served_name.is_none() && reference.contains('@');
        // The entry the asked name links to is an image of its own, listed by its digest.
        if let Some(served_name) = served_name
            && ctx.home.read_vm(&served_name)?.is_none()
        {
            let counter = next_counter(ctx.home)?;
            let mut vm = VmRecord::new(
                &served_name,
                Source::Oci,
                pseudo_mac(&served_name, counter),
                octet(counter),
                now_ms(),
            );
            vm.os = served_os;
            ctx.home.write_vm(&vm)?;
        }
    }
    let mut vm = match ctx.home.read_vm(reference)? {
        Some(vm) => vm,
        None => {
            let counter = next_counter(ctx.home)?;
            let mut vm = VmRecord::new(
                reference,
                Source::Oci,
                pseudo_mac(reference, counter),
                octet(counter),
                now_ms(),
            );
            if let Some(os) = os {
                vm.os = os;
            }
            ctx.effect("image_pulled", reference)?;
            vm
        }
    };
    vm.explicitly_pulled |= own_digest;
    ctx.home.write_vm(&vm)?;
    Ok(Done::quiet())
}

/// Tart 2.41.1's check of a manifest's layout before it pulls (`OCIManifest.
/// tartDiskRepresentation`, `VMDirectory+OCI.swift`): exactly one config and one NVRAM layer,
/// the config first and the NVRAM last, disk-v2 chunks between them, each with its uncompressed
/// size and digest. A stacked image (ASIF overlays) is real Tart's, not the fake's.
fn check_layout(reference: &str, layers: &[serde_json::Value]) -> Result<(), Fail> {
    let media =
        |layer: &serde_json::Value| layer["mediaType"].as_str().unwrap_or_default().to_owned();
    let count = |kind: &str| layers.iter().filter(|l| media(l) == kind).count();
    if count(CONFIG_MEDIA) != 1 {
        return Err(Fail::generic(
            "manifest must contain exactly one Tart config descriptor",
        ));
    }
    if count(NVRAM_MEDIA) != 1 {
        return Err(Fail::generic(
            "manifest must contain exactly one NVRAM descriptor",
        ));
    }
    let ordered = layers.first().is_some_and(|l| media(l) == CONFIG_MEDIA)
        && layers.last().is_some_and(|l| media(l) == NVRAM_MEDIA);
    if !ordered {
        return Err(Fail::generic(
            "descriptors must be ordered as config, disk chunks, then NVRAM",
        ));
    }
    let disk = &layers[1..layers.len() - 1];
    if disk.is_empty() {
        return Err(Fail::generic("manifest has no disk chunks"));
    }
    if media(&disk[0]) != DISK_V2_MEDIA {
        return Err(Fail::generic("disk chunks must start with a disk.v2 base"));
    }
    if let Some(other) = disk.iter().map(media).find(|m| m != DISK_V2_MEDIA) {
        return Err(Fail::fake(&format!(
            "{reference}: {other} layers (stacked images) are not modelled"
        )));
    }
    let annotated = disk.iter().all(|l| {
        l["annotations"][UNCOMPRESSED_SIZE].is_string()
            && l["annotations"][UNCOMPRESSED_DIGEST].is_string()
    });
    if !annotated {
        return Err(Fail::generic(
            "disk chunks need uncompressed size and content digest",
        ));
    }
    Ok(())
}

const CONFIG_MEDIA: &str = "application/vnd.cirruslabs.tart.config.v1";
const NVRAM_MEDIA: &str = "application/vnd.cirruslabs.tart.nvram.v1";
const DISK_V2_MEDIA: &str = "application/vnd.cirruslabs.tart.disk.v2";
const UNCOMPRESSED_SIZE: &str = "org.cirruslabs.tart.uncompressed-size";
const UNCOMPRESSED_DIGEST: &str = "org.cirruslabs.tart.uncompressed-content-digest";

/// Writes a served manifest's image into the OCI cache; returns the guest OS its config names
/// (`darwin` when it names none) and, when the asked name is only a link to it, the entry's own
/// name (`<repo>@sha256:<hash of the manifest served>`).
fn lay_out(
    home: &Home,
    reference: &str,
    manifest: &[u8],
) -> Result<(String, Option<String>), Fail> {
    let parsed: serde_json::Value = serde_json::from_slice(manifest)
        .map_err(|_| Fail::generic(&format!("failed to pull {reference}: manifest unparseable")))?;
    let layers = parsed["layers"].as_array().cloned().unwrap_or_default();
    check_layout(reference, &layers)?;
    let mut config = Vec::new();
    let mut nvram = Vec::new();
    let mut disk = Vec::new();
    for layer in &layers {
        let digest = layer["digest"].as_str().unwrap_or_default();
        let blob = home.registry_blob(digest)?.ok_or_else(|| {
            Fail::generic(&format!(
                "failed to pull {reference}: blob {digest} unknown"
            ))
        })?;
        let media = layer["mediaType"].as_str().unwrap_or_default();
        if media == CONFIG_MEDIA {
            config = blob;
        } else if media == NVRAM_MEDIA {
            nvram = blob;
        } else {
            disk.extend_from_slice(&blob);
        }
    }
    let os = serde_json::from_slice::<serde_json::Value>(&config)
        .ok()
        .and_then(|c| c["os"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "darwin".to_owned());
    let served = format!("sha256:{}", hex(Sha256::digest(manifest).as_ref()));
    home.write_oci_entry(
        reference,
        &served,
        manifest,
        &[(CONFIG, config), (NVRAM, nvram), (DISK, disk)],
    )?;
    let repository = reference
        .split_once('@')
        .map(|(repo, _)| repo)
        .or_else(|| reference.rsplit_once(':').map(|(repo, _)| repo))
        .unwrap_or(reference);
    let own = format!("{repository}@{served}");
    Ok((os, (own != reference).then_some(own)))
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
    let files = if src.source == Source::Oci {
        ctx.home.oci_dir(source)
    } else {
        Some(ctx.home.vm_dir(source))
    };
    if let Some(files) = files {
        ctx.home.copy_vm_files(&files, name)?;
    }
    ctx.home.set_config_mac(name, &vm.mac)?;
    vm.os.clone_from(&src.os);
    vm.installed_runner.clone_from(&src.installed_runner);
    vm.disk_format.clone_from(&src.disk_format);
    (vm.cpu, vm.memory_mb, vm.disk_gb) = (src.cpu, src.memory_mb, src.disk_gb);
    ctx.home.write_vm(&vm)?;
    ctx.effect("vm_cloned", name)?;
    Ok(Done::quiet())
}

/// `run` in the configured [`RunMode`]. Foreground: the VM runs while this process lives, and the
/// process returns when `stop` or `delete` takes the VM away. Detached: the VM is recorded as
/// running and the process exits at once, so nothing needs to survive a test or a killed daemon.
fn run(ctx: &mut Ctx<'_>, name: &str) -> Result<Done, Fail> {
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
    if !detached {
        ctx.held = Some(ctx.home.take_run_lock(name)?);
    }
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
        return Err(Fail::runtime("no IP address found, is your VM running?"));
    }
    let vm = local_vm(ctx.home, name)?;
    Ok(Done::out(format!("{}\n", vm.ip())))
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ListEntry {
    source: &'static str,
    name: String,
    disk: Option<u64>,
    size: u64,
    accessed: String,
    running: bool,
    state: &'static str,
}

#[derive(Serialize)]
struct GetInfo<'a> {
    #[serde(rename = "OS")]
    os: String,
    #[serde(rename = "CPU")]
    cpu: u64,
    #[serde(rename = "Memory")]
    memory: u64,
    #[serde(rename = "Disk")]
    disk: Option<u64>,
    #[serde(rename = "DiskFormat")]
    disk_format: &'a str,
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

/// `HumanReadableByteCount`'s text for an unread size is `-`.
fn text_disk(disk: Option<u64>) -> String {
    disk.map_or_else(|| "-".to_owned(), |gb| gb.to_string())
}

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
    let listed = |vm: &VmRecord| {
        if vm.source == Source::Oci {
            ctx.home.oci_listed_name(&vm.name)
        } else {
            vm.name.clone()
        }
    };
    vms.sort_by_cached_key(|vm| (vm.source == Source::Oci, listed(vm)));
    if quiet {
        return Ok(Done::out(
            vms.iter().map(|vm| format!("{}\n", listed(vm))).collect(),
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
            name: listed(vm),
            disk: vm.printed_disk(),
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
                    e.source,
                    e.name,
                    text_disk(e.disk),
                    e.size,
                    e.accessed,
                    e.state
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
        disk: vm.printed_disk(),
        disk_format: vm.disk_format.as_deref().unwrap_or("raw"),
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
            text_disk(info.disk),
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

/// What `set` does to a VM's identity: its MAC and, for macOS, its machine identifier.
pub(crate) struct Identity<'a> {
    pub random_mac: bool,
    pub random_serial: bool,
    pub mac: Option<&'a str>,
}

/// `set`: changes what the next boot gets (`Set.swift`); a running VM keeps its current run.
/// `--random-mac` draws a new MAC from the allocation counter, so it differs from every MAC the
/// home has handed out; `--mac` sets the one given (2.41.0 on, so a MAC is anyone's to copy);
/// `--random-serial` gives a macOS VM a new machine identifier (`ecid`) and leaves any other
/// alone; `--disk-size` may only grow the disk, as in Tart.
fn set(
    ctx: &Ctx<'_>,
    name: &str,
    identity: Identity<'_>,
    (cpu, memory, disk_size): (Option<u64>, Option<u64>, Option<u64>),
) -> Result<Done, Fail> {
    let _lock = ctx.home.lock()?;
    let mut vm = local_vm(ctx.home, name)?;
    if let Some(size) = disk_size
        && size < vm.disk_gb
    {
        return Err(Fail::generic(
            "the new disk size must be larger than the current one",
        ));
    }
    if let Some(mac) = identity.mac {
        mac.to_ascii_lowercase().clone_into(&mut vm.mac);
        ctx.home.set_config_mac(name, &vm.mac)?;
    }
    if identity.random_mac {
        vm.mac = pseudo_mac(name, next_counter(ctx.home)?);
        ctx.home.set_config_mac(name, &vm.mac)?;
    }
    if identity.random_serial && vm.os == "darwin" {
        let ecid = serial(name, next_counter(ctx.home)?);
        ctx.home.set_config_field(name, "ecid", &ecid)?;
    }
    vm.cpu = cpu.unwrap_or(vm.cpu);
    vm.memory_mb = memory.unwrap_or(vm.memory_mb);
    vm.disk_gb = disk_size.unwrap_or(vm.disk_gb);
    ctx.home.write_vm(&vm)?;
    ctx.effect("vm_set", name)?;
    Ok(Done::quiet())
}

/// `delete`: a local VM, or an OCI entry by any name `pull` gave it. Deleting an OCI name removes
/// its link or directory, then collects what Tart's `VMStorageOCI.gc` collects: links whose
/// entry is gone, and entries no link names that were not pulled by their own digest.
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
        if vm.source == Source::Oci {
            ctx.home.remove_oci(name)?;
            collect_oci(ctx)?;
        }
        ctx.effect("vm_deleted", name)?;
    }
    Ok(Done::quiet())
}

fn collect_oci(ctx: &Ctx<'_>) -> Result<(), Fail> {
    let entries: Vec<VmRecord> = ctx
        .home
        .list_vms()?
        .into_iter()
        .filter(|vm| vm.source == Source::Oci && ctx.home.oci_dir(&vm.name).is_some())
        .collect();
    let linked: Vec<std::path::PathBuf> = entries
        .iter()
        .filter(|vm| ctx.home.oci_is_link(&vm.name))
        .filter_map(|vm| ctx.home.oci_target(&vm.name))
        .collect();
    for vm in &entries {
        let collect = if ctx.home.oci_is_link(&vm.name) {
            ctx.home.oci_target(&vm.name).is_none()
        } else {
            !vm.explicitly_pulled
                && ctx
                    .home
                    .oci_target(&vm.name)
                    .is_some_and(|dir| !linked.contains(&dir))
        };
        if collect {
            ctx.home.remove_vm(&vm.name)?;
            ctx.home.remove_oci(&vm.name)?;
        }
    }
    Ok(())
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
    let guest = ctx
        .rule
        .and_then(|r| r.route_to.clone())
        .unwrap_or_else(|| name.to_owned());
    if argv.first().map(String::as_str) == Some(crate::guest::LAUNCHER) {
        return crate::guest::launcher(ctx, &guest, interactive, &argv[1..]);
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
pub(crate) fn digest_stdin(ctx: &mut Ctx<'_>) -> Result<(u64, String), Fail> {
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
