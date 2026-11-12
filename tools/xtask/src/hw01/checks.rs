//! The checks of `hw01`, in the order they run. Each confirms an assumption a task made about
//! real Tart, the Keychain or launchd (`doc/tasks.md`, HW-01): it passes, fails with bounded
//! evidence, or is `not_run` with its reason (a missing prerequisite, a flag not given, a pin not
//! filled in, an interruption). Evidence is exit codes, booleans, JSON shapes and Tart's wording;
//! never a value the redactor would have to hide if it can be avoided.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::proc::{Call, Finished};
use super::registry::{self, Registry};
use super::report::{CheckResult, shape};
use super::session::{BOOT, CLONE, Entry, PRIVILEGED_PREFIX, SHORT, Session, TRANSFER, json};

/// Where every mayfly image keeps its launcher (ADR 0082).
pub const LAUNCHER: &str = "/usr/local/libexec/mayfly/launcher";

/// The Tart release this checkout supports (TART-01).
const TART_VERSION: &str = "2.41.1";

/// The IC cases of the `ImageStore` contract.
pub const IC_CASES: [&str; 14] = [
    "IC-01", "IC-02", "IC-03", "IC-04", "IC-05", "IC-06", "IC-07", "IC-08", "IC-09", "IC-10",
    "IC-11", "IC-12", "IC-13", "IC-14",
];

/// The repository the contract image is pushed to on the loopback registry.
const CONTRACT_REPOSITORY: &str = "mayfly-contract";

/// How long a contract suite run through cargo may take (its first build included).
const SUITE: Duration = Duration::from_secs(4 * 3600);

/// What the session knows of the Linux base after the Tart checks.
#[derive(Debug, Clone)]
struct Pulled {
    reference: String,
    digest: String,
    repository: String,
}

/// The contract image on the loopback registry.
#[derive(Debug, Clone)]
struct ContractImage {
    /// `127.0.0.1:<port>/mayfly-contract`.
    repository: String,
    digest: String,
    manifest: Vec<u8>,
    /// The stopped local VM it was pushed from.
    source: String,
}

fn text(s: impl Into<String>) -> Value {
    Value::String(s.into())
}

fn check(
    s: &mut Session<'_>,
    id: &str,
    task: &'static str,
    f: impl FnOnce(&mut Session<'_>) -> CheckResult,
) {
    if s.interrupted() {
        s.results
            .push(CheckResult::not_run(id, task, "the run was interrupted"));
        return;
    }
    s.step(id);
    let result = f(s);
    s.results.push(result);
}

fn after<T>(
    s: &mut Session<'_>,
    prerequisite: &Result<T, String>,
    id: &str,
    task: &'static str,
    f: impl FnOnce(&mut Session<'_>, &T) -> CheckResult,
) {
    match prerequisite {
        Ok(value) => check(s, id, task, |s| f(s, value)),
        Err(reason) => s.results.push(CheckResult::not_run(id, task, reason)),
    }
}

fn cache_entry(home: &Path, repository: &str, digest: &str) -> PathBuf {
    let mut path = home.join("cache").join("OCIs");
    for part in repository.split('/') {
        path.push(part);
    }
    path.push(digest);
    path
}

/// Change times of a VM directory's or cache entry's files (the link followed).
fn change_times(dir: &Path) -> BTreeMap<String, (i64, i64)> {
    ["config.json", "nvram.bin", "disk.img", "manifest.json"]
        .iter()
        .filter_map(|name| {
            fs::metadata(dir.join(name))
                .ok()
                .map(|m| ((*name).to_owned(), (m.ctime(), m.ctime_nsec())))
        })
        .collect()
}

/// Every cache entry under `cache/OCIs`, by path relative to it.
fn cache_entries(home: &Path) -> BTreeSet<String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        let Ok(read) = fs::read_dir(dir) else { return };
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("sha256:") {
                out.insert(
                    path.strip_prefix(root)
                        .unwrap_or(&path)
                        .display()
                        .to_string(),
                );
            } else if entry.file_type().is_ok_and(|t| t.is_dir()) {
                walk(root, &path, out);
            }
        }
    }
    let root = home.join("cache").join("OCIs");
    let mut out = BTreeSet::new();
    walk(&root, &root, &mut out);
    out
}

/// The run lock's holder on `vms/<vm>/config.json`, by `fcntl(F_GETLK)` (ADR 0082).
fn lock_holder(home: &Path, vm: &str) -> Result<Option<u32>, String> {
    use rustix::fs::{Mode, OFlags};
    use rustix::process::{Flock, FlockType, fcntl_getlk};
    let path = home.join("vms").join(vm).join("config.json");
    let fd = rustix::fs::open(&path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| format!("open config.json: {e}"))?;
    let held = fcntl_getlk(&fd, &Flock::from(FlockType::WriteLock))
        .map_err(|e| format!("F_GETLK: {e}"))?;
    Ok(held
        .and_then(|lock| lock.pid)
        .and_then(|pid| u32::try_from(pid.as_raw_nonzero().get()).ok()))
}

fn is_mac(text: &str) -> bool {
    text.len() == 17
        && text.bytes().enumerate().all(|(i, b)| {
            if i % 3 == 2 {
                b == b':'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn nonce() -> String {
    let mut bytes = [0u8; 16];
    let random = fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes));
    if random.is_err() {
        let seed = format!("{:?}{}", Instant::now(), std::process::id());
        bytes.copy_from_slice(&registry::digest(seed.as_bytes()).as_bytes()[7..23]);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Runs every check in order.
pub fn all(s: &mut Session<'_>, macos: bool, service_user: Option<&str>) {
    check(s, "tart-version", "TART-01", |s| {
        match s.tart(&["--version"], SHORT) {
            Ok(done) => CheckResult::verdict(
                "tart-version",
                "TART-01",
                done.ok() && done.out() == TART_VERSION,
                vec![
                    text(done.summary()),
                    json!({"printed": done.out(), "expected": TART_VERSION}),
                ],
            ),
            Err(e) => CheckResult::fail("tart-version", "TART-01", vec![text(e)]),
        }
    });
    let pulled = pull_linux(s);
    let base = tart_basics(s, &pulled);
    vm_provider_contract(s, &pulled);
    let contract = contract_image(s, &pulled);
    install_runner_linux(s, &contract);
    registry_checks(s, &contract);
    let macos_image = macos_checks(s, macos);
    let macos_contract = macos_contract_image(s, &macos_image, &contract);
    image_store_contract(s, &contract, macos_contract.as_ref().ok(), &pulled);
    keychain(s);
    service_mode(s, service_user, &base);
}

fn pull_linux(s: &mut Session<'_>) -> Result<Pulled, String> {
    let image = s.pins.linux.clone();
    let pulled = Pulled {
        reference: image.reference(),
        digest: image.digest.clone(),
        repository: image.repository.clone(),
    };
    let mut outcome = Err("the run was interrupted before the pull".to_owned());
    check(s, "pull-by-digest", "DP-07", |s| {
        let done = match s.pull(&pulled.reference, false) {
            Ok(done) => done,
            Err(e) => return CheckResult::fail("pull-by-digest", "DP-07", vec![text(e)]),
        };
        let listed = s.tart(&["list", "--source", "oci", "--format", "json"], SHORT);
        let entries = listed.as_ref().ok().and_then(json).unwrap_or(Value::Null);
        let names: Vec<String> = entries
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["Name"].as_str().map(str::to_owned))
            .filter(|n| n.starts_with(&pulled.repository))
            .collect();
        let held = done.ok() && names.contains(&pulled.reference);
        outcome = if done.ok() {
            Ok(pulled.clone())
        } else {
            Err(format!(
                "the Linux image was not pulled: {}",
                done.summary()
            ))
        };
        CheckResult::verdict(
            "pull-by-digest",
            "DP-07",
            held,
            vec![
                text(done.summary()),
                json!({"oci_names_for_the_repository": names}),
                json!({"oci_entry_shape": entries.get(0).map(shape)}),
            ],
        )
    });
    if outcome.is_ok() {
        after(s, &outcome, "cache-layout", "DP-07", |s, p| {
            cache_layout(s, p)
        });
    } else {
        s.results.push(CheckResult::not_run(
            "cache-layout",
            "DP-07",
            "the Linux image was not pulled",
        ));
    }
    outcome
}

fn cache_layout(s: &mut Session<'_>, p: &Pulled) -> CheckResult {
    let entry = cache_entry(&s.home, &p.repository, &p.digest);
    let meta = fs::symlink_metadata(&entry);
    let (kind, link) = match &meta {
        Ok(m) if m.file_type().is_symlink() => {
            let target = fs::read_link(&entry)
                .map(|t| t.display().to_string())
                .unwrap_or_default();
            (
                "link",
                Some(
                    json!({"absolute": target.starts_with('/'), "target_last": Path::new(&target).file_name().map(|n| n.to_string_lossy().into_owned())}),
                ),
            )
        }
        Ok(m) if m.is_dir() => ("directory", None),
        Ok(_) => ("other", None),
        Err(_) => ("absent", None),
    };
    let files: BTreeMap<&str, bool> = ["config.json", "nvram.bin", "disk.img", "manifest.json"]
        .iter()
        .map(|f| (*f, entry.join(f).is_file()))
        .collect();
    let manifest_digest = fs::read(entry.join("manifest.json"))
        .ok()
        .map(|b| registry::digest(&b));
    let content = s.home.join("cache").join("content").is_dir();
    let held = matches!(kind, "directory" | "link")
        && files.values().take(3).all(|f| *f)
        && files.get("disk.img").copied().unwrap_or(false);
    CheckResult::verdict(
        "cache-layout",
        "DP-07",
        held,
        vec![json!({
            "entry": format!("cache/OCIs/{}/{}", p.repository, p.digest),
            "kind": kind,
            "link": link,
            "files": files,
            "manifest_json_hashes_to_the_digest": manifest_digest.as_deref() == Some(p.digest.as_str()),
            "cache_content_dir": content,
        })],
    )
}

/// The Tart CLI checks on a clone of the Linux image; returns the base's name.
fn tart_basics(s: &mut Session<'_>, pulled: &Result<Pulled, String>) -> Result<String, String> {
    let base = s.vm("linux");
    let mut cloned: Result<String, String> = Err("the Linux image was not pulled".to_owned());
    after(s, pulled, "clone-from-digest", "DP-07", |s, p| {
        let entry = cache_entry(&s.home, &p.repository, &p.digest);
        let before = change_times(&entry);
        let done = s.clone_vm(&p.reference, &base, &[]);
        let after = change_times(&entry);
        cloned = done.as_ref().map(|_| base.clone()).map_err(Clone::clone);
        CheckResult::verdict(
            "clone-from-digest",
            "DP-07",
            done.is_ok() && !before.is_empty() && before == after,
            vec![json!({
                "clone": done.as_ref().map(Finished::summary).unwrap_or_else(Clone::clone),
                "cache_files_change_times_unchanged": before == after,
                "cache_files_seen": before.keys().collect::<Vec<_>>(),
            })],
        )
    });
    after(
        s,
        &cloned,
        "get-keeps-change-times",
        "TART-01",
        |s, base| {
            let dir = s.home.join("vms").join(base);
            let before = change_times(&dir);
            let got = s.tart(&["get", base, "--format", "json"], SHORT);
            let after = change_times(&dir);
            CheckResult::verdict(
                "get-keeps-change-times",
                "TART-01",
                got.as_ref().is_ok_and(Finished::ok) && !before.is_empty() && before == after,
                vec![
                    json!({"files": before.keys().collect::<Vec<_>>(), "unchanged": before == after}),
                ],
            )
        },
    );
    after(s, &cloned, "list-get-json", "TART-01", |s, base| {
        list_get_json(s, base)
    });
    after(s, &cloned, "clone-onto-existing", "TART-01", |s, base| {
        let source = s.pins.linux.reference();
        match s.tart(&["clone", &source, base], CLONE) {
            Ok(done) => CheckResult::verdict(
                "clone-onto-existing",
                "TART-01",
                done.code == Some(64),
                vec![text(done.summary())],
            ),
            Err(e) => CheckResult::fail("clone-onto-existing", "TART-01", vec![text(e)]),
        }
    });
    after(s, &cloned, "set-random-mac", "DEC-01", |s, base| {
        let before = s.inspect(base).ok().flatten().map(|(mac, _)| mac);
        match s.random_mac(base) {
            Ok(after) => CheckResult::verdict(
                "set-random-mac",
                "DEC-01",
                is_mac(&after) && before.as_deref() != Some(after.as_str()),
                vec![
                    json!({"read_back_is_a_mac": is_mac(&after), "changed": before.as_deref() != Some(after.as_str())}),
                ],
            ),
            Err(e) => CheckResult::fail("set-random-mac", "DEC-01", vec![text(e)]),
        }
    });
    after(s, &cloned, "set-memory", "DP-04", |s, base| {
        let set = s.tart(&["set", base, "--memory", "2048"], SHORT);
        let got = s.tart(&["get", base, "--format", "json"], SHORT);
        let memory = got
            .as_ref()
            .ok()
            .and_then(json)
            .map(|v| v["Memory"].clone());
        CheckResult::verdict(
            "set-memory",
            "DP-04",
            set.as_ref().is_ok_and(Finished::ok) && memory == Some(json!(2048)),
            vec![
                json!({"set": set.map(|d| d.summary()).unwrap_or_else(|e| e), "memory_after": memory}),
            ],
        )
    });
    check(s, "missing-vm-errors", "TART-01", |s| {
        let absent = s.vm("absent");
        let mut evidence = Vec::new();
        let mut held = true;
        for verb in ["get", "stop", "delete"] {
            let done = s.tart(&[verb, &absent], SHORT);
            let ok = done
                .as_ref()
                .is_ok_and(|d| d.code == Some(2) && d.err().contains("does not exist"));
            held &= ok;
            evidence.push(json!({verb: done.map(|d| d.summary()).unwrap_or_else(|e| e)}));
        }
        CheckResult::verdict("missing-vm-errors", "TART-01", held, evidence)
    });
    let mut booted: Result<String, String> = Err("the Linux base was not cloned".to_owned());
    after(s, &cloned, "run-lock-holder", "DEC-01", |s, base| {
        let pid = match s.boot(base) {
            Ok(pid) => pid,
            Err(e) => {
                booted = Err(format!("the Linux base did not boot: {e}"));
                return CheckResult::fail("run-lock-holder", "DEC-01", vec![text(e)]);
            }
        };
        booted = Ok(base.clone());
        let holder = lock_holder(&s.home, base);
        CheckResult::verdict(
            "run-lock-holder",
            "DEC-01",
            holder.as_ref().is_ok_and(|h| *h == Some(pid)),
            vec![json!({
                "holder_is_the_tart_run_child": holder.as_ref().is_ok_and(|h| *h == Some(pid)),
                "lock_held": holder.as_ref().map(Option::is_some).unwrap_or(false),
                "error": holder.err(),
            })],
        )
    });
    after(s, &booted, "get-running", "TART-01", |s, base| {
        let got = s.tart(&["get", base, "--format", "json"], SHORT);
        let info = got.as_ref().ok().and_then(json).unwrap_or(Value::Null);
        CheckResult::verdict(
            "get-running",
            "TART-01",
            info["Running"] == json!(true),
            vec![json!({"shape": shape(&info), "disk": info.get("Disk").map(shape)})],
        )
    });
    after(s, &booted, "exec-guest", "DEC-01", |s, base| {
        if let Err(e) = s.wait_guest(base) {
            return CheckResult::fail("exec-guest", "DEC-01", vec![text(e)]);
        }
        let uname = s.tart(&["exec", base, "uname", "-s"], SHORT);
        let failing = s.tart(&["exec", base, "false"], SHORT);
        let named = uname.as_ref().is_ok_and(|d| d.ok() && d.out() == "Linux");
        let passed_through = failing.as_ref().is_ok_and(|d| d.code == Some(1));
        CheckResult::verdict(
            "exec-guest",
            "DEC-01",
            named && passed_through,
            vec![json!({
                "uname": uname.map(|d| d.out()).unwrap_or_else(|e| e),
                "guest_exit_1_passed_through": passed_through,
            })],
        )
    });
    after(
        s,
        &booted,
        "exec-interactive-redirected",
        "TART-01",
        |s, base| {
            let probe = format!("hw01-stdin-probe-{}\n", nonce());
            let mut call = Call::new(
                &s.env.tart,
                &["exec", "-i", base, "cat"],
                &s.tart_env(),
                SHORT,
            )
            .input(probe.as_bytes());
            call.to_files = Some((
                s.run_dir.join("logs/exec.out"),
                s.run_dir.join("logs/exec.err"),
            ));
            match s.tart_call(call) {
                Ok(done) => CheckResult::verdict(
                    "exec-interactive-redirected",
                    "TART-01",
                    done.ok() && done.stdout == probe.as_bytes(),
                    vec![
                        json!({"exit": done.code, "stdout_matches_stdin": done.stdout == probe.as_bytes()}),
                    ],
                ),
                Err(e) => {
                    CheckResult::fail("exec-interactive-redirected", "TART-01", vec![text(e)])
                }
            }
        },
    );
    after(s, &booted, "stop-delete-semantics", "TART-01", |s, base| {
        let delete_running = s.tart(&["delete", base], SHORT);
        let stop = s.halt(base);
        let stop_again = s.tart(&["stop", base], SHORT);
        let ok_delete = delete_running
            .as_ref()
            .is_ok_and(|d| d.code == Some(1) && d.err().contains("is running"));
        let ok_stop = stop.is_ok();
        let ok_again = stop_again
            .as_ref()
            .is_ok_and(|d| d.code == Some(2) && d.err().contains("is not running"));
        CheckResult::verdict(
            "stop-delete-semantics",
            "TART-01",
            ok_delete && ok_stop && ok_again,
            vec![json!({
                "delete_running": delete_running.map(|d| d.summary()).unwrap_or_else(|e| e),
                "stop": stop.map(|d| d.summary()).unwrap_or_else(|e| e),
                "stop_stopped": stop_again.map(|d| d.summary()).unwrap_or_else(|e| e),
            })],
        )
    });
    // Interrupted, the cleanup stops it with the rest.
    if !s.interrupted() && s.run_pid(&base).is_some() {
        let _stopped = s.halt(&base);
    }
    cloned
}

fn list_get_json(s: &mut Session<'_>, base: &str) -> CheckResult {
    let got = s.tart(&["get", base, "--format", "json"], SHORT);
    let listed = s.tart(&["list", "--format", "json"], SHORT);
    let info = got.as_ref().ok().and_then(json).unwrap_or(Value::Null);
    let list = listed.as_ref().ok().and_then(json).unwrap_or(Value::Null);
    let entries = list.as_array().cloned().unwrap_or_default();
    let sources: Vec<String> = entries
        .iter()
        .map(|e| e["Source"].as_str().unwrap_or("?").to_ascii_lowercase())
        .collect();
    let local_first = sources
        .iter()
        .skip_while(|s| *s == "local")
        .all(|s| *s != "local");
    let sorted = ["local", "oci"].iter().all(|source| {
        let names: Vec<&str> = entries
            .iter()
            .filter(|e| {
                e["Source"]
                    .as_str()
                    .unwrap_or("")
                    .eq_ignore_ascii_case(source)
            })
            .filter_map(|e| e["Name"].as_str())
            .collect();
        names.windows(2).all(|w| w[0] <= w[1])
    });
    let integers = info["Memory"].is_u64() && (info["Disk"].is_u64() || info["Disk"].is_null());
    let held =
        info["MACAddress"].is_string() && integers && local_first && sorted && !entries.is_empty();
    CheckResult::verdict(
        "list-get-json",
        "TART-01",
        held,
        vec![json!({
            "get_shape": shape(&info),
            "list_entry_shape": entries.first().map(shape),
            "memory_mib": info["Memory"],
            "disk_gb": info["Disk"],
            "local_before_oci": local_first,
            "sorted_by_name_per_source": sorted,
        })],
    )
}

fn user_env_with(
    s: &Session<'_>,
    extra: &[(&str, String)],
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let mut env = s.env.user_env.clone();
    env.extend(extra.iter().map(|(k, v)| ((*k).into(), v.into())));
    env
}

/// The last lines of a suite's output that say what failed, bounded.
fn tail(done: &Finished) -> Vec<Value> {
    let all = format!(
        "{}\n{}",
        String::from_utf8_lossy(&done.stdout),
        String::from_utf8_lossy(&done.stderr)
    );
    all.lines()
        .filter(|l| {
            l.contains("FAIL") || l.contains("VP-") || l.contains("SP-") || l.contains("panicked")
        })
        .rev()
        .take(20)
        .map(|l| text(l.trim()))
        .collect()
}

fn vm_provider_contract(s: &mut Session<'_>, pulled: &Result<Pulled, String>) {
    after(s, pulled, "vm-provider-contract", "DP-04", |s, p| {
        if let Err(e) = s.clone_vm(&p.reference, "mfb-contract", &[]) {
            return CheckResult::fail(
                "vm-provider-contract",
                "DP-04",
                vec![text(format!("preparing mfb-contract: {e}"))],
            );
        }
        let home = s.home.display().to_string();
        let mut call = s.user_call(
            &s.env.cargo,
            &[
                "xtask",
                "test",
                "--only",
                "real-tart",
                "--real-tart-home",
                &home,
            ],
            SUITE,
        );
        call.limit = 1 << 20;
        match s.run(&call) {
            Ok(done) => {
                let mut evidence = vec![json!({
                    "cases": "VP-01..VP-10, VP-13..VP-15, VP-17",
                    "result": done.summary().lines().next().unwrap_or_default(),
                })];
                evidence.extend(tail(&done));
                CheckResult::verdict("vm-provider-contract", "DP-04", done.ok(), evidence)
            }
            Err(e) => CheckResult::fail("vm-provider-contract", "DP-04", vec![text(e)]),
        }
    });
}

/// Installs the test launcher into the booted `vm` through `tart exec` (argv only).
fn install_launcher(s: &Session<'_>, vm: &str) -> Result<(), String> {
    let script = fs::read(s.env.repo.join("tools/hw01/launcher"))
        .map_err(|e| format!("read the launcher: {e}"))?;
    let steps: [(&[&str], Option<&[u8]>); 3] = [
        (
            &[
                "exec",
                vm,
                "sudo",
                "mkdir",
                "-p",
                "/usr/local/libexec/mayfly",
            ],
            None,
        ),
        (&["exec", "-i", vm, "sudo", "tee", LAUNCHER], Some(&script)),
        (&["exec", vm, "sudo", "chmod", "0755", LAUNCHER], None),
    ];
    for (args, stdin) in steps {
        let mut call = Call::new(&s.env.tart, args, &s.tart_env(), SHORT);
        if let Some(bytes) = stdin {
            call = call.input(bytes);
        }
        let done = s.tart_call(call)?;
        if !done.ok() {
            return Err(format!("tart {}: {}", args.join(" "), done.summary()));
        }
    }
    Ok(())
}

fn contract_image(
    s: &mut Session<'_>,
    pulled: &Result<Pulled, String>,
) -> Result<ContractImage, String> {
    let mut built: Result<ContractImage, String> = Err("the Linux image was not pulled".to_owned());
    let mut steps: Vec<Value> = Vec::new();
    after(s, pulled, "launcher-verify", "DP-05", |s, p| {
        let source = s.vm("contract");
        let result = (|| -> Result<(String, Value), String> {
            s.clone_vm(&p.reference, &source, &[])
                .map_err(|e| format!("clone: {e}"))?;
            s.boot(&source).map_err(|e| format!("boot: {e}"))?;
            s.wait_guest(&source)
                .map_err(|e| format!("guest agent: {e}"))?;
            install_launcher(s, &source).map_err(|e| format!("install the launcher: {e}"))?;
            let nonce = nonce();
            let call = Call::new(
                &s.env.tart,
                &["exec", "-i", &source, LAUNCHER, "verify"],
                &s.tart_env(),
                SHORT,
            )
            .input(format!("{nonce}\n").as_bytes());
            let answer = s.tart_call(call)?;
            let claims = json(&answer).unwrap_or(Value::Null);
            let mac = s.inspect(&source)?.map(|(m, _)| m).unwrap_or_default();
            let verified = claims["v"] == json!(1)
                && claims["nonce"] == json!(nonce)
                && claims["mac"].as_str() == Some(mac.as_str());
            let evidence = json!({"exit": answer.code, "claims_shape": shape(&claims), "nonce_echoed": claims["nonce"] == json!(nonce), "mac_matches_tart": claims["mac"].as_str() == Some(mac.as_str()), "os": claims["os"]});
            s.halt(&source).map_err(|e| format!("stop: {e}"))?;
            if verified {
                Ok((source.clone(), evidence))
            } else {
                Err(format!("verify answered {evidence}"))
            }
        })();
        match result {
            Ok((source, evidence)) => {
                built = Ok(ContractImage {
                    repository: String::new(),
                    digest: String::new(),
                    manifest: Vec::new(),
                    source,
                });
                CheckResult::pass("launcher-verify", "DP-05", vec![evidence])
            }
            Err(e) => {
                built = Err(format!("the contract image was not built: {e}"));
                CheckResult::fail("launcher-verify", "DP-05", vec![text(e)])
            }
        }
    });
    let prepared = built.clone();
    let mut pushed: Result<ContractImage, String> = prepared.clone();
    after(s, &prepared, "contract-image", "DP-07", |s, image| {
        let result = (|| -> Result<ContractImage, String> {
            let dir = s.run_dir.join("registry");
            let registry =
                Registry::start(&dir).map_err(|e| format!("start the loopback registry: {e}"))?;
            let repository = format!("{}/{CONTRACT_REPOSITORY}", registry.host());
            s.registry = Some(registry);
            steps.push(json!({"registry": "started on 127.0.0.1"}));
            let target = format!("{repository}:hw01");
            let done = s.tart(&["push", "--insecure", &image.source, &target], TRANSFER)?;
            steps.push(json!({"push": done.summary()}));
            if !done.ok() {
                return Err(format!("push: {}", done.summary()));
            }
            let manifest = fs::read(registry::manifest_path(&dir, CONTRACT_REPOSITORY, "hw01"))
                .map_err(|e| format!("push: the registry holds no manifest ({e})"))?;
            Ok(ContractImage {
                repository,
                digest: registry::digest(&manifest),
                manifest,
                source: image.source.clone(),
            })
        })();
        pushed = result
            .clone()
            .map_err(|e| format!("the contract image was not built: {e}"));
        CheckResult::verdict("contract-image", "DP-07", result.is_ok(), steps.clone())
    });
    pushed
}

fn install_runner(s: &mut Session<'_>, vm: &str, os: &str) -> Result<Value, String> {
    let (version, sha) = s.pins.runner.for_os(os)?;
    let (version, sha) = (version.to_owned(), sha.to_owned());
    s.boot(vm).map_err(|e| format!("boot: {e}"))?;
    s.wait_guest(vm).map_err(|e| format!("guest agent: {e}"))?;
    let done = s.tart(
        &["exec", vm, LAUNCHER, "install-runner", &version, &sha],
        Duration::from_secs(1800),
    )?;
    let answer = json(&done).unwrap_or(Value::Null);
    let _stopped = s.halt(vm);
    let evidence = json!({"exit": done.code, "answer": answer, "stderr": done.err().chars().take(300).collect::<String>()});
    if done.ok() && answer == json!({"v": 1, "runner_version": version}) {
        Ok(evidence)
    } else {
        Err(evidence.to_string())
    }
}

fn install_runner_linux(s: &mut Session<'_>, contract: &Result<ContractImage, String>) {
    let ready = contract
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|c| s.pins.runner.for_os("linux").map(|_| c.source.clone()));
    after(s, &ready, "install-runner-linux", "DP-07", |s, source| {
        let vm = s.vm("install");
        if let Err(e) = s.clone_vm(source, &vm, &[]) {
            return CheckResult::fail("install-runner-linux", "DP-07", vec![text(e)]);
        }
        match install_runner(s, &vm, "linux") {
            Ok(evidence) => CheckResult::pass("install-runner-linux", "DP-07", vec![evidence]),
            Err(e) => CheckResult::fail("install-runner-linux", "DP-07", vec![text(e)]),
        }
    });
}

fn registry_lines(s: &Session<'_>) -> usize {
    s.registry
        .as_ref()
        .and_then(|r| fs::read_to_string(r.dir.join("requests.log")).ok())
        .map_or(0, |l| l.lines().count())
}

/// Stores `bytes` as a manifest of the contract repository under `reference`.
fn serve_manifest(s: &Session<'_>, reference: &str, bytes: &[u8]) -> Result<(), String> {
    let registry = s.registry.as_ref().ok_or("no registry")?;
    let path = registry::manifest_path(&registry.dir, CONTRACT_REPOSITORY, reference);
    fs::write(path, bytes).map_err(|e| e.to_string())
}

fn registry_checks(s: &mut Session<'_>, contract: &Result<ContractImage, String>) {
    after(s, contract, "clone-offline", "DP-07", |s, c| {
        let reference = format!("{}@{}", c.repository, c.digest);
        let entries_before = cache_entries(&s.home);
        let pulled = s.pull(&reference, true);
        let entry = cache_entry(&s.home, &c.repository, &c.digest);
        let times = change_times(&entry);
        let requests = registry_lines(s);
        let vm = s.vm("offline");
        let cloned = s.clone_vm(&reference, &vm, &[]);
        let quiet = registry_lines(s) == requests;
        let unchanged = !times.is_empty() && change_times(&entry) == times;
        let kept = cache_entries(&s.home).is_superset(&entries_before);
        s.results.push(CheckResult::verdict(
            "no-auto-prune",
            "TART-01",
            kept,
            vec![json!({"cache_entries_kept_across_pull_and_clone": kept})],
        ));
        CheckResult::verdict(
            "clone-offline",
            "DP-07",
            pulled.as_ref().is_ok_and(Finished::ok) && cloned.is_ok() && quiet && unchanged,
            vec![json!({
                "pull": pulled.map(|d| d.summary()).unwrap_or_else(|e| e),
                "clone": cloned.map(|d| d.summary()).unwrap_or_else(|e| e),
                "registry_requests_during_clone": registry_lines(s) - requests,
                "cache_files_change_times_unchanged": unchanged,
            })],
        )
    });
    after(s, contract, "pull-link-form", "TART-01", |s, c| {
        // The registry answers a request for another digest with the contract's manifest.
        let mut other = c.manifest.clone();
        other.push(b'\n');
        let requested = registry::digest(&other);
        if let Err(e) = serve_manifest(s, &requested, &c.manifest) {
            return CheckResult::fail("pull-link-form", "TART-01", vec![text(e)]);
        }
        let reference = format!("{}@{requested}", c.repository);
        let pulled = s.pull(&reference, true);
        let link = cache_entry(&s.home, &c.repository, &requested);
        let target = cache_entry(&s.home, &c.repository, &c.digest);
        let read = fs::read_link(&link).ok();
        let absolute = read.as_ref().is_some_and(|t| t.is_absolute());
        let names_served = read
            .as_ref()
            .and_then(|t| t.file_name())
            .is_some_and(|n| n.to_string_lossy() == c.digest);
        let deleted = s.tart(&["delete", &reference], SHORT);
        let link_gone = fs::symlink_metadata(&link).is_err();
        let target_gone = fs::symlink_metadata(&target).is_err();
        CheckResult::verdict(
            "pull-link-form",
            "TART-01",
            pulled.as_ref().is_ok_and(Finished::ok)
                && absolute
                && names_served
                && link_gone
                && target_gone,
            vec![json!({
                "pull": pulled.map(|d| d.summary()).unwrap_or_else(|e| e),
                "is_link": read.is_some(),
                "absolute": absolute,
                "names_the_served_manifest": names_served,
                "delete": deleted.map(|d| d.summary()).unwrap_or_else(|e| e),
                "link_removed": link_gone,
                "linked_entry_removed": target_gone,
            })],
        )
    });
    after(s, contract, "pull-refusals", "TART-01", |s, c| {
        let Ok(mut manifest) = serde_json::from_slice::<Value>(&c.manifest) else {
            return CheckResult::fail(
                "pull-refusals",
                "TART-01",
                vec![text("the contract manifest is not JSON")],
            );
        };
        let mut evidence = Vec::new();
        let mut held = true;
        let mut reordered = manifest.clone();
        if let Some(layers) = reordered["layers"].as_array_mut() {
            layers.rotate_left(1);
        }
        if let Some(layers) = manifest["layers"].as_array_mut() {
            for layer in layers {
                if layer["mediaType"] == json!("application/vnd.cirruslabs.tart.disk.v2") {
                    layer["mediaType"] = json!("application/vnd.cirruslabs.tart.disk.v1");
                }
            }
        }
        for (name, variant) in [("layers_out_of_order", reordered), ("disk_v1", manifest)] {
            let bytes = serde_json::to_vec(&variant).unwrap_or_default();
            let digest = registry::digest(&bytes);
            if let Err(e) = serve_manifest(s, &digest, &bytes) {
                return CheckResult::fail("pull-refusals", "TART-01", vec![text(e)]);
            }
            let done = s.pull(&format!("{}@{digest}", c.repository), true);
            let refused = done.as_ref().is_ok_and(|d| !d.ok());
            held &= refused;
            evidence.push(
                json!({name: done.map(|d| d.summary()).unwrap_or_else(|e| e), "refused": refused}),
            );
        }
        CheckResult::verdict("pull-refusals", "TART-01", held, evidence)
    });
}

fn image_store_contract(
    s: &mut Session<'_>,
    contract: &Result<ContractImage, String>,
    macos_digest: Option<&String>,
    pulled: &Result<Pulled, String>,
) {
    let ready = contract.as_ref().map_err(Clone::clone).and_then(|c| {
        s.pins
            .runner
            .for_os("linux")
            .map(|(v, sha)| (c.clone(), v.to_owned(), sha.to_owned()))
    });
    // The osx-arm64 pin and the macOS contract image only matter to IC-14; without them the
    // world reports it `not_run`.
    let osx = s
        .pins
        .runner
        .for_os("osx")
        .ok()
        .map(|(_, sha)| sha.to_owned());
    let (contract, version, sha) = match ready {
        Ok(ready) => ready,
        Err(reason) => {
            for id in IC_CASES.iter().chain(["public-manifest-fetch"].iter()) {
                s.results.push(CheckResult::not_run(id, "DP-07", &reason));
            }
            return;
        }
    };
    if s.interrupted() {
        for id in IC_CASES.iter().chain(["public-manifest-fetch"].iter()) {
            s.results
                .push(CheckResult::not_run(id, "DP-07", "the run was interrupted"));
        }
        return;
    }
    s.step("image-store-contract");
    let root = s.run_dir.join("ic");
    let made = fs::create_dir(&root).and_then(|()| {
        fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700))
    });
    let registry = s
        .registry
        .as_ref()
        .map(|r| (r.host(), r.dir.display().to_string()));
    let (Ok(()), Some((host, dir))) = (made, registry) else {
        for id in IC_CASES.iter().chain(["public-manifest-fetch"].iter()) {
            s.results.push(CheckResult::fail(
                id,
                "DP-07",
                vec![text("the contract world's directory could not be made")],
            ));
        }
        return;
    };
    let public = pulled
        .as_ref()
        .map(|p| p.reference.clone())
        .unwrap_or_default();
    let env = user_env_with(
        s,
        &[
            ("MAYFLY_HW01_IC_ROOT", root.display().to_string()),
            ("MAYFLY_HW01_REGISTRY", host),
            ("MAYFLY_HW01_REGISTRY_DIR", dir),
            (
                "MAYFLY_HW01_CONTRACT",
                format!("{}@{}", contract.repository, contract.digest),
            ),
            ("MAYFLY_HW01_RUNNER", version),
            ("MAYFLY_HW01_RUNNER_SHA256_LINUX_ARM64", sha),
            (
                "MAYFLY_HW01_RUNNER_SHA256_OSX_ARM64",
                osx.unwrap_or_default(),
            ),
            (
                "MAYFLY_HW01_CONTRACT_MACOS",
                macos_digest
                    .map(|d| format!("{}@{d}", contract.repository))
                    .unwrap_or_default(),
            ),
            ("MAYFLY_HW01_PUBLIC_IMAGE", public),
        ],
    );
    let args = [
        "nextest",
        "run",
        "--workspace",
        "--locked",
        "--profile",
        "serial",
        "--no-capture",
        "--features",
        "mayfly-adapter-tart/real-tart",
        "-E",
        "binary(real_tart_images)",
    ];
    let mut call = Call::new(&s.env.cargo, &args, &env, SUITE);
    call.cwd = Some(s.env.repo.clone());
    call.limit = 4 << 20;
    let done = s.run(&call);
    let output = done
        .as_ref()
        .map(|d| {
            format!(
                "{}\n{}",
                String::from_utf8_lossy(&d.stdout),
                String::from_utf8_lossy(&d.stderr)
            )
        })
        .unwrap_or_default();
    let verdicts: BTreeMap<String, (String, String)> = output
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("hw01-case ")?;
            let mut parts = rest.splitn(3, ' ');
            let (id, status) = (parts.next()?, parts.next()?);
            Some((
                id.to_owned(),
                (
                    status.to_owned(),
                    parts.next().unwrap_or_default().to_owned(),
                ),
            ))
        })
        .collect();
    let summary = done.map(|d| d.summary()).unwrap_or_else(|e| e);
    for id in IC_CASES.iter().chain(["public-manifest-fetch"].iter()) {
        let result = match verdicts.get(*id) {
            Some((status, detail)) if status == "pass" => {
                CheckResult::pass(id, "DP-07", vec![text(detail.clone())])
            }
            Some((status, detail)) if status == "not_run" => {
                CheckResult::not_run(id, "DP-07", detail)
            }
            Some((_, detail)) => CheckResult::fail(id, "DP-07", vec![text(detail.clone())]),
            None => CheckResult::fail(
                id,
                "DP-07",
                vec![text(format!("the suite printed no verdict: {summary}"))],
            ),
        };
        s.results.push(result);
    }
}

fn keychain(s: &mut Session<'_>) {
    const FACTS: [&str; 4] = [
        "security-trailing-newline",
        "security-absent-exit",
        "security-hex-output",
        "security-locked-exit",
    ];
    let Some(security) = s.env.security.clone() else {
        for id in FACTS.iter().chain(["secret-provider-contract"].iter()) {
            s.results.push(CheckResult::not_run(
                id,
                "SEC-02",
                "no /usr/bin/security: not macOS",
            ));
        }
        return;
    };
    if s.interrupted() {
        for id in FACTS.iter().chain(["secret-provider-contract"].iter()) {
            s.results.push(CheckResult::not_run(
                id,
                "SEC-02",
                "the run was interrupted",
            ));
        }
        return;
    }
    s.step("keychain");
    let path = s.run_dir.join("hw01.keychain-db").display().to_string();
    let password = nonce();
    let value = nonce();
    s.secrets.extend([password.clone(), value.clone()]);
    let service = format!("mayfly-hw01-{}", s.env.run_id);
    let sec = |s: &Session<'_>, args: &[&str]| {
        s.run(&s.user_call(&security, args, Duration::from_secs(60)))
    };
    let prepared = (|| -> Result<(), String> {
        s.note(Entry::Keychain { path: path.clone() })?;
        for args in [
            vec!["create-keychain", "-p", &password, &path],
            vec!["set-keychain-settings", &path],
            vec!["unlock-keychain", "-p", &password, &path],
        ] {
            let done = sec(s, &args)?;
            if !done.ok() {
                return Err(format!("security {}: {}", args[0], done.summary()));
            }
        }
        // The list is changed only when it was read whole and holds the login keychain: an
        // empty or unreadable list is never written back over the user's.
        let original = s.search_list(&security)?;
        let login = original.iter().any(|k| {
            Path::new(k)
                .file_name()
                .is_some_and(|n| n == "login.keychain-db" || n == "login.keychain")
        });
        if !login {
            return Err(
                "the keychain search list was unreadable, empty or without the login keychain; \
                 it was left untouched"
                    .to_owned(),
            );
        }
        s.note(Entry::SearchList {
            original: original.clone(),
            added: path.clone(),
        })?;
        let mut args = vec!["list-keychains", "-d", "user", "-s"];
        args.extend(original.iter().map(String::as_str));
        args.push(&path);
        let done = sec(s, &args)?;
        if done.ok() {
            Ok(())
        } else {
            Err(format!("security list-keychains: {}", done.summary()))
        }
    })();
    if let Err(e) = prepared {
        for id in FACTS.iter().chain(["secret-provider-contract"].iter()) {
            s.results.push(CheckResult::fail(
                id,
                "SEC-02",
                vec![text(format!("the test keychain: {e}"))],
            ));
        }
        return;
    }
    let add = sec(
        s,
        &[
            "add-generic-password",
            "-s",
            &service,
            "-a",
            "fact",
            "-w",
            &value,
            &path,
        ],
    );
    let found = sec(
        s,
        &["find-generic-password", "-s", &service, "-a", "fact", "-w"],
    );
    let newline = found
        .as_ref()
        .is_ok_and(|d| d.ok() && d.stdout == format!("{value}\n").as_bytes());
    s.results.push(CheckResult::verdict(
        "security-trailing-newline",
        "SEC-02",
        add.as_ref().is_ok_and(Finished::ok) && newline,
        vec![json!({"exit": found.as_ref().ok().and_then(|d| d.code), "value_then_one_line_break": newline})],
    ));
    let absent = sec(
        s,
        &[
            "find-generic-password",
            "-s",
            &format!("{service}-absent"),
            "-a",
            "fact",
            "-w",
        ],
    );
    s.results.push(CheckResult::verdict(
        "security-absent-exit",
        "SEC-02",
        absent.as_ref().is_ok_and(|d| d.code == Some(44)),
        vec![json!({"exit": absent.as_ref().ok().and_then(|d| d.code)})],
    ));
    let hex_service = format!("{service}-hex");
    let added = sec(
        s,
        &[
            "add-generic-password",
            "-s",
            &hex_service,
            "-a",
            "fact",
            "-X",
            "00ff41",
            &path,
        ],
    );
    let hex = sec(
        s,
        &[
            "find-generic-password",
            "-s",
            &hex_service,
            "-a",
            "fact",
            "-w",
        ],
    );
    s.results.push(CheckResult::verdict(
        "security-hex-output",
        "SEC-02",
        added.as_ref().is_ok_and(Finished::ok) && hex.as_ref().is_ok_and(|d| d.ok() && d.out() == "00ff41"),
        vec![json!({"exit": hex.as_ref().ok().and_then(|d| d.code), "printed": hex.as_ref().map(|d| d.out()).unwrap_or_default()})],
    ));
    let locked = sec(s, &["lock-keychain", &path]).and_then(|_| {
        sec(
            s,
            &["find-generic-password", "-s", &service, "-a", "fact", "-w"],
        )
    });
    let _unlocked = sec(s, &["unlock-keychain", "-p", &password, &path]);
    s.results.push(CheckResult::verdict(
        "security-locked-exit",
        "SEC-02",
        locked.as_ref().is_ok_and(|d| matches!(d.code, Some(36 | 51))),
        vec![json!({"exit": locked.as_ref().ok().and_then(|d| d.code), "timed_out": locked.as_ref().is_ok_and(|d| d.timed_out)})],
    ));
    check(s, "secret-provider-contract", "SEC-02", |s| {
        let env = user_env_with(
            s,
            &[
                ("MAYFLY_REAL_KEYCHAIN", path.clone()),
                (
                    "MAYFLY_REAL_KEYCHAIN_SERVICE",
                    format!("{service}-contract"),
                ),
            ],
        );
        let args = [
            "nextest",
            "run",
            "--workspace",
            "--locked",
            "--profile",
            "serial",
            "--features",
            "mayfly-adapter-secrets/real-keychain",
            "-E",
            "binary(real_keychain)",
        ];
        let mut call = Call::new(&s.env.cargo, &args, &env, SUITE);
        call.cwd = Some(s.env.repo.clone());
        call.limit = 1 << 20;
        match s.run(&call) {
            Ok(done) => {
                let mut evidence = vec![
                    json!({"cases": "SP-01..SP-08", "result": done.summary().lines().next().unwrap_or_default()}),
                ];
                evidence.extend(tail(&done));
                CheckResult::verdict("secret-provider-contract", "SEC-02", done.ok(), evidence)
            }
            Err(e) => CheckResult::fail("secret-provider-contract", "SEC-02", vec![text(e)]),
        }
    });
}

fn config_ecid(s: &Session<'_>, vm: &str) -> Option<Value> {
    let config: Value =
        serde_json::from_slice(&fs::read(s.home.join("vms").join(vm).join("config.json")).ok()?)
            .ok()?;
    config.get("ecid").cloned()
}

/// With `--macos`: the macOS image with the test launcher, pushed beside the Linux contract
/// image (tag `hw01-macos` of the same repository), so IC-14 has a macOS base beside a Linux one.
/// Returns its digest.
fn macos_contract_image(
    s: &mut Session<'_>,
    macos: &Result<String, String>,
    contract: &Result<ContractImage, String>,
) -> Result<String, String> {
    let ready = match (macos, contract) {
        (Ok(reference), Ok(contract)) => Ok((reference.clone(), contract.clone())),
        (Err(e), _) | (_, Err(e)) => Err(e.clone()),
    };
    let mut built = Err("the macOS contract image was not built".to_owned());
    after(
        s,
        &ready,
        "contract-image-macos",
        "DP-07",
        |s, (reference, contract)| {
            let source = s.vm("contract-macos");
            let result = (|| -> Result<String, String> {
                s.clone_vm(reference, &source, &[])
                    .map_err(|e| format!("clone: {e}"))?;
                s.boot(&source).map_err(|e| format!("boot: {e}"))?;
                s.wait_guest(&source)
                    .map_err(|e| format!("guest agent: {e}"))?;
                install_launcher(s, &source).map_err(|e| format!("install the launcher: {e}"))?;
                s.halt(&source).map_err(|e| format!("stop: {e}"))?;
                let target = format!("{}:hw01-macos", contract.repository);
                let done = s.tart(&["push", "--insecure", &source, &target], TRANSFER)?;
                if !done.ok() {
                    return Err(format!("push: {}", done.summary()));
                }
                let dir = s.registry.as_ref().ok_or("no registry")?.dir.clone();
                let manifest = fs::read(registry::manifest_path(
                    &dir,
                    CONTRACT_REPOSITORY,
                    "hw01-macos",
                ))
                .map_err(|e| format!("push: the registry holds no manifest ({e})"))?;
                Ok(registry::digest(&manifest))
            })();
            built = result.clone();
            match result {
                Ok(_) => CheckResult::pass(
                    "contract-image-macos",
                    "DP-07",
                    vec![json!({"pushed": true})],
                ),
                Err(e) => CheckResult::fail("contract-image-macos", "DP-07", vec![text(e)]),
            }
        },
    );
    built
}

fn macos_checks(s: &mut Session<'_>, macos: bool) -> Result<String, String> {
    const IDS: [&str; 4] = [
        "macos-pull",
        "macos-random-serial",
        "macos-exec-install-runner",
        "hypervisor-limit",
    ];
    if !macos {
        for (id, task) in IDS.iter().zip(["DP-07", "TART-01", "DP-07", "DP-04"]) {
            s.results
                .push(CheckResult::not_run(id, task, "needs --macos"));
        }
        return Err("needs --macos".to_owned());
    }
    let image = s.pins.macos.clone();
    let reference = image.reference();
    let mut pulled: Result<String, String> = Err("the macOS image was not pulled".to_owned());
    check(s, "macos-pull", "DP-07", |s| {
        match s.pull(&reference, false) {
            Ok(done) => {
                if done.ok() {
                    pulled = Ok(reference.clone());
                }
                CheckResult::verdict("macos-pull", "DP-07", done.ok(), vec![text(done.summary())])
            }
            Err(e) => CheckResult::fail("macos-pull", "DP-07", vec![text(e)]),
        }
    });
    let mut cloned: Result<String, String> = Err("the macOS image was not cloned".to_owned());
    after(
        s,
        &pulled,
        "macos-random-serial",
        "TART-01",
        |s, reference| {
            let vm = s.vm("macos");
            if let Err(e) = s.clone_vm(reference, &vm, &[]) {
                return CheckResult::fail("macos-random-serial", "TART-01", vec![text(e)]);
            }
            cloned = Ok(vm.clone());
            let before = config_ecid(s, &vm);
            let set = s.tart(&["set", &vm, "--random-serial"], SHORT);
            let after = config_ecid(s, &vm);
            let mac = s.inspect(&vm).ok().flatten().map(|(m, _)| m);
            let booted = s.boot(&vm).and_then(|_| s.wait_guest(&vm));
            let mac_after = s.inspect(&vm).ok().flatten().map(|(m, _)| m);
            let _stopped = s.halt(&vm);
            CheckResult::verdict(
                "macos-random-serial",
                "TART-01",
                set.as_ref().is_ok_and(Finished::ok)
                    && before.is_some()
                    && before != after
                    && booted.is_ok()
                    && mac == mac_after,
                vec![json!({
                    "set": set.map(|d| d.summary()).unwrap_or_else(|e| e),
                    "ecid_changed": before.is_some() && before != after,
                    "booted": booted.err(),
                    "mac_kept_across_boot": mac == mac_after,
                })],
            )
        },
    );
    after(s, &cloned, "macos-exec-install-runner", "DP-07", |s, vm| {
        let result = s
            .boot(vm)
            .and_then(|_| s.wait_guest(vm))
            .and_then(|()| install_launcher(s, vm))
            .and_then(|()| {
                let _stopped = s.halt(vm);
                install_runner(s, vm, "osx")
            });
        let _stopped = s.halt(vm);
        match result {
            Ok(evidence) => CheckResult::pass("macos-exec-install-runner", "DP-07", vec![evidence]),
            Err(e) => CheckResult::fail("macos-exec-install-runner", "DP-07", vec![text(e)]),
        }
    });
    after(s, &pulled, "hypervisor-limit", "DP-04", |s, reference| {
        let vms: Vec<String> = (1..=3).map(|n| s.vm(&format!("limit-{n}"))).collect();
        for vm in &vms {
            if let Err(e) = s.clone_vm(reference, vm, &[]) {
                return CheckResult::fail("hypervisor-limit", "DP-04", vec![text(e)]);
            }
        }
        for vm in &vms[..2] {
            if let Err(e) = s.boot(vm) {
                return CheckResult::fail(
                    "hypervisor-limit",
                    "DP-04",
                    vec![text(format!("boot {vm}: {e}"))],
                );
            }
        }
        // The third `tart run` is this run's child like the others, never killed: started
        // over the limit it exits by itself; started anyway, it is stopped with `tart stop`.
        let third = s.boot(&vms[2]);
        let log = fs::read_to_string(s.run_dir.join("logs").join(format!("{}.log", vms[2])))
            .unwrap_or_default();
        let wording = log
            .lines()
            .find(|l| l.contains("exceeds the system limit"))
            .map(str::to_owned);
        for vm in &vms {
            if s.run_pid(vm).is_some() {
                let _stopped = s.halt(vm);
            }
        }
        let refused = third.as_ref().is_err_and(|e| e.contains("exit status: 1"))
            && wording
                .as_deref()
                .is_some_and(|w| w.contains("The number of VMs exceeds the system limit"));
        CheckResult::verdict(
            "hypervisor-limit",
            "DP-04",
            refused,
            vec![json!({
                "third_run": third.map(|_| "ran".to_owned()).unwrap_or_else(|e| e),
                "wording": wording,
            })],
        )
    });
    pulled
}

fn plist(label: &str, tart: &Path, user: &str, home: &str, vm: &str) -> String {
    let escape = |t: &str| {
        t.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\
         <key>Label</key><string>{}</string>\n<key>UserName</key><string>{}</string>\n\
         <key>ProgramArguments</key><array><string>{}</string><string>run</string>\
         <string>--no-graphics</string><string>{}</string></array>\n\
         <key>EnvironmentVariables</key><dict><key>TART_HOME</key><string>{}</string></dict>\n\
         <key>RunAtLoad</key><true/>\n</dict>\n</plist>\n",
        escape(label),
        escape(user),
        escape(&tart.display().to_string()),
        escape(vm),
        escape(home),
    )
}

fn service_mode(s: &mut Session<'_>, user: Option<&str>, base: &Result<String, String>) {
    let Some(user) = user else {
        s.results.push(CheckResult::not_run(
            "launchd-service-mode",
            "DP-07",
            "needs --service-mode --service-user <user>",
        ));
        return;
    };
    let Some(sudo) = s.env.sudo.clone() else {
        s.results.push(CheckResult::not_run(
            "launchd-service-mode",
            "DP-07",
            "no sudo: not macOS",
        ));
        return;
    };
    let user = user.to_owned();
    after(s, base, "launchd-service-mode", "DP-07", |s, base| {
        let result = (|| -> Result<Value, String> {
            let id = s.run(&s.user_call(Path::new("/usr/bin/id"), &["-u", &user], SHORT))?;
            let uid: u32 = id.out().parse().map_err(|_| {
                "the service user does not exist (hw01 never creates one)".to_owned()
            })?;
            if uid < 200 {
                return Err("the service user must be a dedicated, non-system user".to_owned());
            }
            if s.env.identity.username.as_deref() == Some(user.as_str())
                || uid == rustix::process::getuid().as_raw()
            {
                return Err("the service user must not be the user running hw01".to_owned());
            }
            let mut ask = s.user_call(&sudo, &["-v"], Duration::from_secs(300));
            ask.foreground = true;
            if !s.run(&ask)?.ok() {
                return Err("sudo was refused".to_owned());
            }
            let sudo_n = |s: &Session<'_>, args: &[&str]| {
                let mut all = vec!["-n"];
                all.extend_from_slice(args);
                s.run(&s.user_call(&sudo, &all, CLONE)).and_then(|d| {
                    if d.ok() {
                        Ok(d)
                    } else {
                        Err(format!("sudo {}: {}", args[0], d.summary()))
                    }
                })
            };
            let vm = s.vm("service");
            let label = format!("dev.mayfly.hw01.{}", s.env.run_id);
            let plist_path = format!("/Library/LaunchDaemons/{label}.plist");
            if Path::new(&plist_path).exists() {
                return Err(format!(
                    "{plist_path} already exists; hw01 never replaces one"
                ));
            }
            // mktemp makes the directory under a name nobody can have chosen; it is recorded the
            // moment its name is known (a crash in between leaves it, see doc/hw01.md).
            let template = format!("{PRIVILEGED_PREFIX}{}-service.XXXXXXXX", s.env.run_id);
            let dir = sudo_n(s, &["mktemp", "-d", &template])?.out();
            let tail = dir.strip_prefix(PRIVILEGED_PREFIX).unwrap_or_default();
            if tail.is_empty() || tail.contains('/') || dir.contains("..") || dir.contains('\n') {
                return Err(format!("mktemp answered an unexpected path: {dir}"));
            }
            s.note(Entry::PrivilegedDir { path: dir.clone() })?;
            sudo_n(s, &["chown", &user, &dir])?;
            sudo_n(s, &["chmod", "0700", &dir])?;
            sudo_n(
                s,
                &[
                    "install",
                    "-d",
                    "-o",
                    &user,
                    "-m",
                    "0700",
                    &format!("{dir}/vms"),
                ],
            )?;
            let source = s.home.join("vms").join(base).display().to_string();
            let target = format!("{dir}/vms/{vm}");
            sudo_n(s, &["cp", "-c", "-R", &source, &target])?;
            sudo_n(s, &["chown", "-R", &user, &target])?;
            let local = s.run_dir.join(format!("{label}.plist"));
            fs::write(&local, plist(&label, &s.env.tart, &user, &dir, &vm))
                .map_err(|e| e.to_string())?;
            s.note(Entry::LaunchDaemon {
                label: label.clone(),
                plist: plist_path.clone(),
            })?;
            sudo_n(
                s,
                &[
                    "install",
                    "-o",
                    "root",
                    "-g",
                    "wheel",
                    "-m",
                    "0644",
                    &local.display().to_string(),
                    &plist_path,
                ],
            )?;
            sudo_n(s, &["launchctl", "bootstrap", "system", &plist_path])?;
            let tart = s.env.tart.display().to_string();
            let home_var = format!("TART_HOME={dir}");
            let started = Instant::now();
            let mut running = false;
            while started.elapsed() < BOOT && !s.interrupted() {
                let got = sudo_n(
                    s,
                    &[
                        "-u",
                        &user,
                        "/usr/bin/env",
                        &home_var,
                        &tart,
                        "get",
                        &vm,
                        "--format",
                        "json",
                    ],
                );
                if got
                    .as_ref()
                    .ok()
                    .and_then(json)
                    .is_some_and(|v| v["Running"] == json!(true))
                {
                    running = true;
                    break;
                }
                std::thread::sleep(Duration::from_secs(3));
            }
            let _stopped = sudo_n(
                s,
                &["-u", &user, "/usr/bin/env", &home_var, &tart, "stop", &vm],
            );
            let _out = sudo_n(s, &["launchctl", "bootout", &format!("system/{label}")]);
            // Removed now, while sudo's credentials are fresh, so a crash later leaves no daemon
            // that loads at boot.
            let _removed = sudo_n(s, &["rm", "-f", &plist_path]);
            if running {
                Ok(json!({"ran_under_a_launch_daemon_without_a_gui_session": true}))
            } else {
                Err("Tart did not list the VM running under the LaunchDaemon".to_owned())
            }
        })();
        match result {
            Ok(evidence) => CheckResult::pass("launchd-service-mode", "DP-07", vec![evidence]),
            Err(e) => CheckResult::fail("launchd-service-mode", "DP-07", vec![text(e)]),
        }
    });
}
