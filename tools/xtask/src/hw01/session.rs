//! One run of `hw01`: its throwaway directory and `TART_HOME`, the record of everything it makes,
//! the `tart` calls (each against that home only), and the cleanup that deletes exactly what the
//! record names.
//!
//! **The record comes first.** Each VM, image, keychain, LaunchDaemon, privileged directory and
//! process is appended to `record.jsonl` in the run directory (and synced) before the call that
//! creates it; a VM's MAC is appended after it is created and after every MAC change the run
//! makes. Cleanup reads only that list: a VM is deleted by its exact name after its MAC is read
//! again and matches the last recorded one (one whose MAC was never recorded, because the run
//! stopped between the clone and the read, is deleted by its exact name in the run's own home), a
//! process is killed by the pid the run started (only while the run still holds its handle), and
//! nothing is ever chosen by prefix, pattern or listing. Then the run directory goes. Whatever
//! could not be deleted is returned as a leftover, for the report and stderr. Pinned by
//! `hw01::tests::should_delete_exactly_the_record_when_a_check_fails_midway` and
//! `should_clean_up_and_stop_its_vm_when_the_run_is_interrupted`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Env;
use super::pins::Pins;
use super::proc::{self, Call, Finished};
use super::registry::Registry;
use super::report::CheckResult;

/// A short `tart` call (`get`, `list`, `set`, `stop`, `delete`).
pub const SHORT: Duration = Duration::from_secs(120);
/// A pull or a push of a whole image.
pub const TRANSFER: Duration = Duration::from_secs(3 * 3600);
/// A clone.
pub const CLONE: Duration = Duration::from_secs(1800);
/// How long a booting VM or its guest agent may take to answer.
pub const BOOT: Duration = Duration::from_secs(600);

/// One line of the run's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    /// A local VM about to be created in the run's home.
    Vm { name: String },
    /// The MAC a recorded VM has now.
    VmMac { name: String, mac: String },
    /// An OCI image about to be pulled into the run's home.
    Image { reference: String },
    /// A process about to be started.
    Process { id: u32, what: String },
    /// The pid of a started process.
    ProcessPid { id: u32, pid: u32 },
    /// A keychain file about to be created.
    Keychain { path: String },
    /// The user's keychain search list before the run added `added` (its test keychain).
    SearchList {
        original: Vec<String>,
        added: String,
    },
    /// A LaunchDaemon about to be installed.
    LaunchDaemon { label: String, plist: String },
    /// A directory outside the run directory, made with `sudo`.
    PrivilegedDir { path: String },
}

/// The prefix every privileged directory has; cleanup refuses any other.
pub const PRIVILEGED_PREFIX: &str = "/private/tmp/mayfly-hw01-";

/// A run in progress.
pub struct Session<'e> {
    /// Where the programs and the host's facts come from.
    pub env: &'e Env,
    /// The pins.
    pub pins: Pins,
    /// The throwaway directory (0700).
    pub run_dir: PathBuf,
    /// The throwaway `TART_HOME` inside it (0700).
    pub home: PathBuf,
    home_id: (u64, u64),
    record: Vec<Entry>,
    children: BTreeMap<u32, Child>,
    /// Secrets the run made (keychain password and items), for the redactor.
    pub secrets: Vec<String>,
    /// The loopback registry, once started.
    pub registry: Option<Registry>,
    /// Each check's verdict, in order.
    pub results: Vec<CheckResult>,
    /// Set while cleaning up: calls are no longer cut short by the interruption.
    cleaning: bool,
    calm: AtomicBool,
}

/// Why a directory is not private to this user.
fn private(path: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(format!("{} is not a plain directory", path.display()));
    }
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(format!("{} is owned by another user", path.display()));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{} is open to others ({:o})",
            path.display(),
            meta.mode() & 0o777
        ));
    }
    Ok(())
}

/// No ancestor of `path` may be writable by others unless sticky (as `/tmp`), so nobody else can
/// swap the run directory (ADR 0087's check of a private home).
fn safe_ancestors(path: &Path) -> Result<(), String> {
    for ancestor in path.ancestors().skip(1) {
        let meta =
            fs::symlink_metadata(ancestor).map_err(|e| format!("{}: {e}", ancestor.display()))?;
        if meta.file_type().is_symlink() {
            return Err(format!("{} is a symlink", ancestor.display()));
        }
        let mode = meta.mode();
        if mode & 0o022 != 0 && mode & 0o1000 == 0 {
            return Err(format!("{} is writable by others", ancestor.display()));
        }
    }
    Ok(())
}

fn identity(path: &Path) -> Option<(u64, u64)> {
    fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

fn mkdir_private(path: &Path) -> Result<(), String> {
    fs::create_dir(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot chmod {}: {e}", path.display()))
}

/// The parsed JSON of a call's stdout.
pub fn json(done: &Finished) -> Option<Value> {
    serde_json::from_slice(&done.stdout).ok()
}

impl<'e> Session<'e> {
    /// Makes the run directory and the home, checks they are private and not a home the caller
    /// uses.
    ///
    /// # Errors
    /// Either could not be made, is not private, or is a caller's home.
    pub fn create(env: &'e Env, pins: Pins) -> Result<Self, String> {
        // The temporary directory's real path, resolved once before anything is made: macOS
        // spells it under `/var`, a root-owned link to `/private/var`, and every path the run
        // checks and uses afterwards has no symlink component at all (`safe_ancestors` refuses
        // one). The run directory and its home are then real `0700` directories.
        let parent = env
            .temp_parent
            .canonicalize()
            .map_err(|e| format!("{}: {e}", env.temp_parent.display()))?;
        let run_dir = tempfile::Builder::new()
            .prefix(&format!("mayfly-hw01-{}-", env.run_id))
            .tempdir_in(&parent)
            .map_err(|e| format!("cannot create the run directory: {e}"))?
            .keep();
        fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot chmod the run directory: {e}"))?;
        let home = run_dir.join("home");
        let made = mkdir_private(&home)
            .and_then(|()| mkdir_private(&run_dir.join("user")))
            .and_then(|()| mkdir_private(&run_dir.join("logs")))
            .and_then(|()| private(&run_dir))
            .and_then(|()| private(&home))
            .and_then(|()| safe_ancestors(&run_dir));
        let caller = env.caller_homes.iter().find(|caller| {
            caller.canonicalize().ok().as_deref() == Some(home.as_path())
                || identity(caller).is_some() && identity(caller) == identity(&home)
        });
        if made.is_err() || caller.is_some() {
            let _removed = fs::remove_dir_all(&run_dir);
            return Err(made.err().unwrap_or_else(|| {
                "the run's home would be a TART_HOME the caller uses".to_owned()
            }));
        }
        let home_id = identity(&home).ok_or("the home vanished")?;
        if let Some(hook) = &env.hooks.on_home {
            hook(&home);
        }
        Ok(Self {
            env,
            pins,
            run_dir,
            home,
            home_id,
            record: Vec::new(),
            children: BTreeMap::new(),
            secrets: Vec::new(),
            registry: None,
            results: Vec::new(),
            cleaning: false,
            calm: AtomicBool::new(false),
        })
    }

    /// The run directory an earlier run left (a crash, a second Ctrl-C), with its record, for
    /// `hw01 --cleanup <run dir>`: nothing is made, only [`Self::cleanup`] runs.
    ///
    /// # Errors
    /// Not a run directory of `hw01` (its name, its record, its home), or not private.
    pub fn resume(env: &'e Env, pins: Pins, run_dir: &Path) -> Result<Self, String> {
        let run_dir = run_dir
            .canonicalize()
            .map_err(|e| format!("{}: {e}", run_dir.display()))?;
        let named = run_dir
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("mayfly-hw01-"));
        let home = run_dir.join("home");
        if !named || !run_dir.join("record.jsonl").is_file() {
            return Err(format!(
                "{} is not a run directory of hw01 (mayfly-hw01-…, with record.jsonl)",
                run_dir.display()
            ));
        }
        private(&run_dir)?;
        private(&home)?;
        // The working directory the `tart` children start in; a run that crashed early may not
        // have made it.
        if !run_dir.join("user").is_dir() {
            mkdir_private(&run_dir.join("user"))?;
        }
        let text = fs::read_to_string(run_dir.join("record.jsonl"))
            .map_err(|e| format!("cannot read the record: {e}"))?;
        let record = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l).map_err(|e| format!("a record line is not valid: {e}"))
            })
            .collect::<Result<Vec<Entry>, String>>()?;
        let home_id = identity(&home).ok_or("the home vanished")?;
        Ok(Self {
            env,
            pins,
            run_dir,
            home,
            home_id,
            record,
            children: BTreeMap::new(),
            secrets: Vec::new(),
            registry: None,
            results: Vec::new(),
            cleaning: false,
            calm: AtomicBool::new(false),
        })
    }

    /// `hw01-<run id>-<role>`: every VM this run makes is named so, and recorded.
    pub fn vm(&self, role: &str) -> String {
        format!("hw01-{}-{role}", self.env.run_id)
    }

    /// Whether Ctrl-C or a termination signal arrived (never while cleaning up, which always
    /// runs to its end).
    pub fn interrupted(&self) -> bool {
        !self.cleaning && self.env.interrupt.load(Ordering::SeqCst)
    }

    /// The flag a child's call is cut short by.
    fn flag(&self) -> &AtomicBool {
        if self.cleaning {
            &self.calm
        } else {
            &self.env.interrupt
        }
    }

    /// Announces `step` to the hook (tests) and stderr.
    pub fn step(&self, step: &str) {
        eprintln!("hw01: {step}");
        if let Some(hook) = &self.env.hooks.on_step {
            hook(step, &self.home);
        }
    }

    /// Appends `entry` to the record and syncs it before the caller creates what it names.
    ///
    /// # Errors
    /// The record could not be written: the caller must not create anything.
    pub fn note(&mut self, entry: Entry) -> Result<(), String> {
        let line = serde_json::to_string(&entry).map_err(|e| e.to_string())?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.run_dir.join("record.jsonl"))
            .and_then(|mut f| {
                writeln!(f, "{line}")?;
                f.sync_data()?;
                Ok(f)
            })
            .map_err(|e| format!("cannot write the run record: {e}"))?;
        let _flushed = file.flush();
        self.record.push(entry);
        Ok(())
    }

    /// The environment every `tart` child gets: the run's home and nothing of the caller's.
    pub fn tart_env(&self) -> Vec<(OsString, OsString)> {
        vec![
            ("TART_HOME".into(), self.home.clone().into_os_string()),
            ("TART_NO_AUTO_PRUNE".into(), "1".into()),
            ("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()),
            ("HOME".into(), self.run_dir.join("user").into_os_string()),
        ]
    }

    /// The working directory every `tart` child starts in: the run's own, never the caller's, so
    /// nothing a child writes beside itself lands outside the run directory.
    fn tart_dir(&self) -> PathBuf {
        self.run_dir.join("user")
    }

    /// Refuses unless the home is still the directory this run created.
    fn checked_home(&self) -> Result<(), String> {
        if identity(&self.home) != Some(self.home_id) {
            return Err("refused: the TART_HOME is no longer the one this run created".to_owned());
        }
        private(&self.home)
    }

    /// `tart args…` against the run's home.
    ///
    /// # Errors
    /// The home is not the run's, or `tart` could not start.
    pub fn tart(&self, args: &[&str], deadline: Duration) -> Result<Finished, String> {
        self.tart_call(Call::new(&self.env.tart, args, &self.tart_env(), deadline))
    }

    /// A prepared `tart` call against the run's home.
    ///
    /// # Errors
    /// As [`Self::tart`].
    pub fn tart_call(&self, call: Call) -> Result<Finished, String> {
        self.checked_home()?;
        let mut call = call;
        call.cwd.get_or_insert_with(|| self.tart_dir());
        proc::run(&call, self.flag()).map_err(|e| format!("cannot run tart: {e}"))
    }

    /// `tart get <name> --format json`: (MAC, running), `None` when Tart says it does not exist.
    ///
    /// # Errors
    /// Another failure, or an answer that is not the expected JSON.
    pub fn inspect(&self, name: &str) -> Result<Option<(String, bool)>, String> {
        let done = self.tart(&["get", name, "--format", "json"], SHORT)?;
        if !done.ok() {
            if done.err().contains("does not exist") {
                return Ok(None);
            }
            return Err(done.summary());
        }
        let info = json(&done).ok_or("tart get printed no JSON")?;
        let mac = info["MACAddress"]
            .as_str()
            .ok_or("tart get printed no MACAddress")?
            .to_ascii_lowercase();
        Ok(Some((mac, info["Running"].as_bool().unwrap_or(false))))
    }

    /// Records the MAC `name` has now.
    ///
    /// # Errors
    /// It could not be read or recorded.
    pub fn note_mac(&mut self, name: &str) -> Result<String, String> {
        let (mac, _) = self.inspect(name)?.ok_or("the VM is gone")?;
        self.note(Entry::VmMac {
            name: name.to_owned(),
            mac: mac.clone(),
        })?;
        Ok(mac)
    }

    /// Records `name`, then `tart clone <source> <name>`, then its MAC.
    ///
    /// # Errors
    /// The record, the clone or the MAC read failed (the clone's exit summary).
    pub fn clone_vm(
        &mut self,
        source: &str,
        name: &str,
        extra: &[&str],
    ) -> Result<Finished, String> {
        self.note(Entry::Vm {
            name: name.to_owned(),
        })?;
        let mut args = vec!["clone", source, name];
        args.extend_from_slice(extra);
        let done = self.tart(&args, CLONE)?;
        if !done.ok() {
            return Err(format!("tart clone {}", done.summary()));
        }
        self.note_mac(name)?;
        Ok(done)
    }

    /// `tart set <name> --random-mac`, then records the new MAC.
    ///
    /// # Errors
    /// The set or the read failed.
    pub fn random_mac(&mut self, name: &str) -> Result<String, String> {
        let done = self.tart(&["set", name, "--random-mac"], SHORT)?;
        if !done.ok() {
            return Err(format!("tart set --random-mac {}", done.summary()));
        }
        self.note_mac(name)
    }

    /// Records `reference`, then `tart pull` it.
    ///
    /// # Errors
    /// The record failed or Tart could not start.
    pub fn pull(&mut self, reference: &str, insecure: bool) -> Result<Finished, String> {
        self.note(Entry::Image {
            reference: reference.to_owned(),
        })?;
        let mut args = vec!["pull", reference];
        if insecure {
            args.push("--insecure");
        }
        self.tart(&args, TRANSFER)
    }

    /// Starts `tart run --no-graphics <name>` as this run's child (recorded first) and waits for
    /// Tart to list it running; returns the child's pid.
    ///
    /// # Errors
    /// It could not start, exited, or was not listed running within [`BOOT`].
    pub fn boot(&mut self, name: &str) -> Result<u32, String> {
        self.checked_home()?;
        let id = u32::try_from(self.record.len()).unwrap_or(u32::MAX);
        self.note(Entry::Process {
            id,
            what: format!("tart run {name}"),
        })?;
        let log = self.run_dir.join("logs").join(format!("{name}.log"));
        let child = proc::spawn(
            &self.env.tart,
            &["run", "--no-graphics", name],
            &self.tart_env(),
            &log,
            &self.tart_dir(),
        )
        .map_err(|e| format!("cannot start tart run: {e}"))?;
        let pid = child.id();
        self.children.insert(id, child);
        self.note(Entry::ProcessPid { id, pid })?;
        let started = Instant::now();
        loop {
            if let Ok(Some((_, true))) = self.inspect(name) {
                return Ok(pid);
            }
            let exited = self
                .children
                .get_mut(&id)
                .and_then(|c| c.try_wait().ok().flatten());
            if let Some(status) = exited {
                return Err(format!("tart run exited before the VM ran ({status})"));
            }
            if self.interrupted() || started.elapsed() > BOOT {
                return Err("the VM was not listed running in time".to_owned());
            }
            thread::sleep(Duration::from_millis(500));
        }
    }

    /// Waits until `tart exec <name> true` answers 0: the guest agent is up.
    ///
    /// # Errors
    /// It did not within [`BOOT`].
    pub fn wait_guest(&self, name: &str) -> Result<(), String> {
        let started = Instant::now();
        let mut last = String::new();
        while started.elapsed() < BOOT && !self.interrupted() {
            let done = self.tart(&["exec", name, "true"], Duration::from_secs(30))?;
            if done.ok() {
                return Ok(());
            }
            last = done.summary();
            thread::sleep(Duration::from_secs(2));
        }
        Err(format!("the guest agent did not answer: {last}"))
    }

    /// `tart stop <name>`, then waits for this run's `tart run` of it to exit.
    ///
    /// # Errors
    /// The stop failed.
    pub fn halt(&mut self, name: &str) -> Result<Finished, String> {
        let done = self.tart(&["stop", name, "--timeout", "60"], SHORT)?;
        let what = format!("tart run {name}");
        let ids: Vec<u32> = self
            .record
            .iter()
            .filter_map(|e| match e {
                Entry::Process { id, what: w } if *w == what => Some(*id),
                _ => None,
            })
            .collect();
        for id in ids {
            self.reap(id, Duration::from_secs(60));
        }
        if done.ok() {
            Ok(done)
        } else {
            Err(format!("tart stop {}", done.summary()))
        }
    }

    /// Waits up to `grace` for child `id` to exit, then kills its group. Only a child this run
    /// still holds a handle to: never a pid it did not start.
    fn reap(&mut self, id: u32, grace: Duration) -> bool {
        let Some(child) = self.children.get_mut(&id) else {
            return true;
        };
        let started = Instant::now();
        while started.elapsed() < grace {
            if let Ok(Some(_)) = child.try_wait() {
                self.children.remove(&id);
                return true;
            }
            thread::sleep(Duration::from_millis(100));
        }
        proc::kill_group(child);
        self.children.remove(&id);
        true
    }

    /// The pid of the live child `tart run <name>`, if this run holds one.
    pub fn run_pid(&mut self, name: &str) -> Option<u32> {
        let what = format!("tart run {name}");
        let id = self.record.iter().rev().find_map(|e| match e {
            Entry::Process { id, what: w } if *w == what => Some(*id),
            _ => None,
        })?;
        let child = self.children.get_mut(&id)?;
        match child.try_wait() {
            Ok(None) => Some(child.id()),
            _ => None,
        }
    }

    /// A user-environment call (`cargo`, `security`, `sudo`): the caller's few variables, not
    /// the run's home; run in the checkout.
    pub fn user_call(&self, program: &Path, args: &[&str], deadline: Duration) -> Call {
        let mut call = Call::new(program, args, &self.env.user_env, deadline);
        call.cwd = Some(self.env.repo.clone());
        call
    }

    /// Runs a prepared user-environment call.
    ///
    /// # Errors
    /// The program could not start.
    pub fn run(&self, call: &Call) -> Result<Finished, String> {
        proc::run(call, self.flag())
            .map_err(|e| format!("cannot run {}: {e}", call.program.display()))
    }

    /// The local VMs Tart lists in `home` (the run's own, or one of its contract worlds'), by
    /// name, and whether each runs.
    fn listed_in(&self, home: &Path) -> Vec<(String, bool)> {
        let mut env = self.tart_env();
        env[0].1 = home.as_os_str().to_owned();
        let mut call = Call::new(
            &self.env.tart,
            &["list", "--format", "json", "--source", "local"],
            &env,
            SHORT,
        );
        call.cwd = Some(self.tart_dir());
        proc::run(&call, self.flag())
            .ok()
            .and_then(|d| json(&d))
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|e| {
                Some((
                    e["Name"].as_str()?.to_owned(),
                    e["Running"] == Value::Bool(true),
                ))
            })
            .collect()
    }

    /// `tart args…` against `home`, one of the run's own homes.
    fn tart_in(&self, home: &Path, args: &[&str]) -> bool {
        let mut env = self.tart_env();
        env[0].1 = home.as_os_str().to_owned();
        let mut call = Call::new(&self.env.tart, args, &env, SHORT);
        call.cwd = Some(self.tart_dir());
        proc::run(&call, self.flag()).is_ok_and(|d| d.ok())
    }

    /// The homes a contract suite made inside the run directory (`ic/world-*/home`), besides
    /// the run's own.
    fn own_homes(&self) -> Vec<PathBuf> {
        let mut homes = vec![self.home.clone()];
        if let Ok(worlds) = fs::read_dir(self.run_dir.join("ic")) {
            homes.extend(
                worlds
                    .flatten()
                    .map(|w| w.path().join("home"))
                    .filter(|h| h.is_dir() && h.starts_with(&self.run_dir)),
            );
        }
        homes
    }

    /// The user's keychain search list, as `security list-keychains -d user` prints it.
    pub fn search_list(&self, security: &Path) -> Result<Vec<String>, String> {
        let done = self.run(&self.user_call(security, &["list-keychains", "-d", "user"], SHORT))?;
        if !done.ok() {
            return Err(format!("security list-keychains: {}", done.summary()));
        }
        Ok(done
            .out()
            .lines()
            .map(|l| l.trim().trim_matches('"').to_owned())
            .filter(|l| !l.is_empty())
            .collect())
    }

    /// Deletes exactly what the record names, newest first; then stops and deletes what a
    /// contract suite left in the run's own homes (reported, since the record does not name it),
    /// then removes the run directory. Returns what it could not delete, and what it found
    /// unrecorded.
    pub fn cleanup(&mut self) -> Vec<String> {
        self.cleaning = true;
        self.step("cleanup");
        let mut leftovers = Vec::new();
        let entries: Vec<Entry> = self.record.iter().rev().cloned().collect();
        let sudo = self.env.sudo.clone();
        let sudo_n = |args: &[&str]| -> bool {
            sudo.as_ref().is_some_and(|sudo| {
                let mut all = vec!["-n"];
                all.extend_from_slice(args);
                proc::run(
                    &Call::new(sudo, &all, &self.env.user_env, SHORT),
                    &AtomicBool::new(false),
                )
                .is_ok_and(|d| d.ok())
            })
        };
        for entry in &entries {
            if let Entry::LaunchDaemon { label, plist } = entry {
                let _out = sudo_n(&["launchctl", "bootout", &format!("system/{label}")]);
                if Path::new(plist).exists() && !sudo_n(&["rm", "-f", plist]) {
                    leftovers.push(format!(
                        "LaunchDaemon {label}: sudo launchctl bootout system/{label}; sudo rm -f {plist}"
                    ));
                }
            }
        }
        let last_mac = |record: &[Entry], name: &str| {
            record.iter().rev().find_map(|e| match e {
                Entry::VmMac { name: n, mac } if n == name => Some(mac.clone()),
                _ => None,
            })
        };
        let vms: Vec<String> = entries
            .iter()
            .filter_map(|e| match e {
                Entry::Vm { name } => Some(name.clone()),
                _ => None,
            })
            .collect();
        // Stop each recorded VM that runs, then wait for this run's `tart run` children.
        for name in &vms {
            if let Ok(Some((mac, true))) = self.inspect(name)
                && last_mac(&self.record, name).is_none_or(|recorded| recorded == mac)
            {
                let _stopped = self.tart(&["stop", name, "--timeout", "30"], SHORT);
            }
        }
        let ids: Vec<u32> = self.children.keys().copied().collect();
        for id in ids {
            self.reap(id, Duration::from_secs(30));
        }
        for entry in &entries {
            if let Entry::ProcessPid { pid, .. } = entry {
                let alive = i32::try_from(*pid)
                    .ok()
                    .and_then(rustix::process::Pid::from_raw)
                    .is_some_and(|p| rustix::process::test_kill_process(p).is_ok());
                if alive {
                    leftovers.push(format!(
                        "process {pid}: started by an earlier run of hw01 and still alive; it is \
                         never killed by pid once that run has ended"
                    ));
                }
            }
        }
        for name in &vms {
            // The MAC is read again right before the delete: a VM whose MAC is not the last one
            // recorded is not this run's.
            match self.inspect(name) {
                Ok(None) => {}
                Ok(Some((mac, _))) if last_mac(&self.record, name).is_some_and(|r| r != mac) => {
                    leftovers.push(format!(
                        "vm {name}: its MAC is not the one recorded; left in place"
                    ));
                }
                Ok(Some(_)) => {
                    if !self
                        .tart(&["delete", name], SHORT)
                        .as_ref()
                        .is_ok_and(Finished::ok)
                    {
                        leftovers.push(format!("vm {name}: tart delete failed"));
                    }
                }
                Err(why) => leftovers.push(format!("vm {name}: cannot be read ({why})")),
            }
        }
        for entry in &entries {
            if let Entry::Image { reference } = entry {
                // Already gone (a link deleted with its entry, a refused pull) is fine.
                let _deleted = self.tart(&["delete", reference], SHORT);
            }
        }
        if let Some(security) = self.env.security.clone() {
            for entry in &entries {
                match entry {
                    Entry::SearchList { original, added } => {
                        // Restored only if nobody changed it since: exactly the original plus ours.
                        let mut expected = original.clone();
                        expected.push(added.clone());
                        let same = |a: &String, b: &String| {
                            a == b
                                || Path::new(a).canonicalize().ok().is_some_and(|ca| {
                                    Path::new(b).canonicalize().ok().as_ref() == Some(&ca)
                                })
                        };
                        let current = self.search_list(&security);
                        let unchanged = current.as_ref().is_ok_and(|c| {
                            c.len() == expected.len()
                                && c.iter().zip(&expected).all(|(a, b)| same(a, b))
                        });
                        if !unchanged {
                            leftovers.push(format!(
                                "the keychain search list changed during the run; it was left as it \
                                 is: remove {added} with `security list-keychains -d user -s <the others>`"
                            ));
                            continue;
                        }
                        let mut args = vec!["list-keychains", "-d", "user", "-s"];
                        args.extend(original.iter().map(String::as_str));
                        let restored = self.run(&self.user_call(&security, &args, SHORT));
                        if !restored.is_ok_and(|d| d.ok()) {
                            leftovers.push("the keychain search list was not restored".to_owned());
                        }
                    }
                    Entry::Keychain { path } => {
                        let deleted =
                            self.run(&self.user_call(&security, &["delete-keychain", path], SHORT));
                        if !deleted.is_ok_and(|d| d.ok()) && Path::new(path).exists() {
                            leftovers.push(format!("keychain {path}"));
                        }
                    }
                    _ => {}
                }
            }
        }
        for entry in &entries {
            if let Entry::PrivilegedDir { path } = entry {
                let exact = path.starts_with(PRIVILEGED_PREFIX) && !path.contains("..");
                if exact && Path::new(path).exists() && !sudo_n(&["rm", "-rf", path]) {
                    leftovers.push(format!("directory {path}: sudo rm -rf {path}"));
                }
            }
        }
        // What a contract suite left in the run's own homes: the record cannot name it, the
        // home is the run's, so it is stopped and deleted, and reported.
        if self.checked_home().is_ok() {
            for home in self.own_homes() {
                for (name, running) in self.listed_in(&home) {
                    if running {
                        let _stopped = self.tart_in(&home, &["stop", &name, "--timeout", "30"]);
                    }
                    let deleted = self.tart_in(&home, &["delete", &name]);
                    let place = home
                        .strip_prefix(&self.run_dir)
                        .unwrap_or(&home)
                        .display()
                        .to_string();
                    leftovers.push(format!(
                        "vm {name} in $RUN/{place}: not in the record (a contract suite left it); {}",
                        if deleted { "stopped and deleted" } else { "could not be deleted" }
                    ));
                }
            }
        }
        self.registry = None;
        self.step("remove-run-dir");
        if let Err(err) = fs::remove_dir_all(&self.run_dir) {
            leftovers.push(format!("run directory {}: {err}", self.run_dir.display()));
        }
        leftovers
    }
}
