//! `cargo xtask hw01`: the real-Tart checks the owner runs from a terminal on their Mac (HW-01X,
//! ADR 0015 as amended on 2026-11-11, `doc/hw01.md`).
//!
//! It refuses to start off macOS ([`supported`]). On a Mac it makes a throwaway run directory and
//! `TART_HOME` (both `0700`, checked as ADR 0087 checks mayfly's private home, never `~/.tart` or
//! the caller's `TART_HOME`), pulls the images `tools/hw01/images.toml` pins by digest, runs the
//! checks of [`checks`] (the Tart CLI facts of DEC-01, DP-04, DP-07 and TART-01, the
//! `VmProvider`, `SecretProvider` and `ImageStore` contracts, the Keychain facts of ADR 0058, and
//! with `--macos` and `--service-mode` the macOS image and launchd checks), cleans up exactly what
//! its record names ([`session`]), and writes a redacted JSON report ([`report`], [`redact`]).
//!
//! Agents never run it on real Tart: its steps are tested here on Linux against `mayfly-fake-tart`
//! through [`execute`], which takes every program and host fact from an [`Env`].

pub mod checks;
pub mod pins;
pub mod proc;
pub mod redact;
pub mod registry;
pub mod report;
pub mod session;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use redact::{Identity, Redactor};
use report::{Report, Status};
use session::{SHORT, Session};

/// What the owner asked for.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Also pull the macOS image and run the macOS checks.
    pub macos: bool,
    /// Run the launchd check as this existing, dedicated user.
    pub service_user: Option<String>,
    /// Where the report goes (default `target/hw01-report.json` in the checkout).
    pub report: Option<PathBuf>,
    /// Only clean up this earlier run's directory (`--cleanup <run dir>`).
    pub cleanup: Option<PathBuf>,
}

/// Called with the run's home once it exists.
pub type HomeHook = Box<dyn Fn(&Path)>;

/// Called with each step's name and the run's home.
pub type StepHook = Box<dyn Fn(&str, &Path)>;

/// Test seams: called when the home exists, and at each step with the home.
#[derive(Default)]
pub struct Hooks {
    /// After the home is made, before anything runs in it.
    pub on_home: Option<HomeHook>,
    /// At each step (check id, `cleanup`, `remove-run-dir`).
    pub on_step: Option<StepHook>,
}

/// Every program and host fact a run uses.
pub struct Env {
    /// `tart`.
    pub tart: PathBuf,
    /// `cargo`, for the contract suites.
    pub cargo: PathBuf,
    /// `/usr/bin/security` (macOS only).
    pub security: Option<PathBuf>,
    /// `/usr/bin/sudo` (macOS only; `--service-mode`).
    pub sudo: Option<PathBuf>,
    /// The checkout.
    pub repo: PathBuf,
    /// The pins file.
    pub pins: PathBuf,
    /// Where the run directory is made.
    pub temp_parent: PathBuf,
    /// Homes the run must never use (`$TART_HOME`, `~/.tart`).
    pub caller_homes: Vec<PathBuf>,
    /// What the report must never contain.
    pub identity: Identity,
    /// The few variables `cargo`, `security` and `sudo` get.
    pub user_env: Vec<(OsString, OsString)>,
    /// Set by Ctrl-C, SIGTERM or SIGHUP.
    pub interrupt: Arc<AtomicBool>,
    /// Names this run's VMs (`hw01-<run id>-…`).
    pub run_id: String,
    /// Test seams.
    pub hooks: Hooks,
}

/// How a run ended.
#[derive(Debug)]
pub struct Outcome {
    /// The exit code: 0 all passed or not run, 1 a check failed, 2 the run could not start,
    /// 130 interrupted.
    pub code: i32,
    /// The redacted report, `Null` when the run could not start.
    pub report: Value,
    /// What cleanup could not delete.
    pub leftovers: Vec<String>,
}

/// `hw01` drives real Tart and the Keychain: macOS only.
///
/// # Errors
/// Any other system.
pub fn supported(os: &str) -> Result<(), String> {
    if os == "macos" {
        Ok(())
    } else {
        Err(format!(
            "hw01 runs only on macOS, where it drives real Tart; this is {os}. See doc/hw01.md."
        ))
    }
}

/// Eight random hex digits for the run's names.
fn run_id() -> String {
    let mut bytes = [0u8; 4];
    let read = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes));
    if read.is_err() {
        bytes = std::process::id().to_be_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

/// The output of `program args…`, trimmed, if it ran and exited 0.
fn ask(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// `"Key" = "value"` from `ioreg` output.
fn ioreg_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let rest = line.split_once(&format!("\"{key}\" = \""))?.1;
        rest.split('"').next().map(str::to_owned)
    })
}

impl Env {
    /// The host's programs and facts (macOS).
    ///
    /// # Errors
    /// `tart` is not on `PATH`.
    pub fn host(interrupt: Arc<AtomicBool>) -> Result<Self, String> {
        let tart = on_path("tart").ok_or("tart is not on PATH: install Tart 2.41.1 first")?;
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let mut hostnames: Vec<String> = Vec::new();
        hostnames.extend(ask("hostname", &[]));
        hostnames.extend(ask("scutil", &["--get", "ComputerName"]));
        hostnames.extend(ask("scutil", &["--get", "LocalHostName"]));
        let ioreg = ask("ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"]).unwrap_or_default();
        let home = var("HOME");
        let mut caller_homes: Vec<PathBuf> =
            var("TART_HOME").map(PathBuf::from).into_iter().collect();
        caller_homes.extend(home.as_ref().map(|h| Path::new(h).join(".tart")));
        let user_env = [
            "HOME",
            "USER",
            "LOGNAME",
            "PATH",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "CARGO_TARGET_DIR",
            "TMPDIR",
            "LANG",
            "TERM",
        ]
        .iter()
        .filter_map(|k| std::env::var_os(k).map(|v| (OsString::from(k), v)))
        .collect();
        let existing = |p: &str| Path::new(p).is_file().then(|| PathBuf::from(p));
        let repo = crate::repo::root();
        Ok(Self {
            tart,
            cargo: var("CARGO").map_or_else(|| PathBuf::from("cargo"), PathBuf::from),
            security: existing("/usr/bin/security"),
            sudo: existing("/usr/bin/sudo"),
            pins: repo.join("tools/hw01/images.toml"),
            repo,
            temp_parent: std::env::temp_dir(),
            caller_homes,
            identity: Identity {
                hostnames,
                username: var("USER").or_else(|| var("LOGNAME")),
                home,
                serial: ioreg_value(&ioreg, "IOPlatformSerialNumber"),
                hardware_uuid: ioreg_value(&ioreg, "IOPlatformUUID"),
                service_user: None,
            },
            user_env,
            interrupt,
            run_id: run_id(),
            hooks: Hooks::default(),
        })
    }
}

/// Ctrl-C, SIGTERM and SIGHUP set `flag` so the run stops its checks and cleans up; a second one
/// while the flag is already set exits at once (cleanup is then left undone: run `hw01` again
/// only after reading what the first run's stderr listed).
fn handle_signals(flag: &Arc<AtomicBool>) -> Result<(), String> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    for signal in [SIGINT, SIGTERM, SIGHUP] {
        signal_hook::flag::register_conditional_shutdown(signal, 130, Arc::clone(flag))
            .and_then(|_| signal_hook::flag::register(signal, Arc::clone(flag)))
            .map_err(|e| format!("cannot handle signal {signal}: {e}"))?;
    }
    Ok(())
}

/// `cargo xtask hw01`.
pub fn run(options: &Options) -> i32 {
    if let Err(err) = supported(std::env::consts::OS) {
        eprintln!("xtask: {err}");
        return 2;
    }
    let interrupt = Arc::new(AtomicBool::new(false));
    if let Err(err) = handle_signals(&interrupt) {
        eprintln!("xtask: {err}");
        return 2;
    }
    let env = match Env::host(interrupt) {
        Ok(env) => env,
        Err(err) => {
            eprintln!("xtask: {err}");
            return 2;
        }
    };
    if let Some(run_dir) = &options.cleanup {
        return clean_up(&env, run_dir);
    }
    let outcome = execute(&env, options);
    let count = |status: &str| {
        outcome.report["checks"]
            .as_array()
            .map_or(0, |c| c.iter().filter(|c| c["status"] == status).count())
    };
    eprintln!(
        "hw01: {} pass, {} fail, {} not_run; send the report back (doc/hw01.md)",
        count("pass"),
        count("fail"),
        count("not_run")
    );
    for leftover in &outcome.leftovers {
        eprintln!("hw01: NOT deleted: {leftover}");
    }
    outcome.code
}

/// `hw01 --cleanup <run dir>`: replays an earlier run's record (after a crash or a second
/// Ctrl-C), deleting exactly what it names and what its own homes still hold, then the
/// directory; prints what it could not delete. Pinned by
/// `tests::should_delete_what_a_crashed_run_recorded_when_cleanup_replays_it`.
pub fn clean_up(env: &Env, run_dir: &Path) -> i32 {
    let pins = match pins::load(&env.pins) {
        Ok(pins) => pins,
        Err(err) => {
            eprintln!("xtask: hw01: {err}");
            return 2;
        }
    };
    let mut session = match Session::resume(env, pins, run_dir) {
        Ok(session) => session,
        Err(err) => {
            eprintln!("xtask: hw01: {err}");
            return 2;
        }
    };
    let leftovers = session.cleanup();
    for leftover in &leftovers {
        eprintln!("hw01: {leftover}");
    }
    eprintln!("hw01: cleaned up {}", run_dir.display());
    i32::from(leftovers.iter().any(|l| !l.contains("stopped and deleted")))
}

fn version_of(session: &Session<'_>, program: &str, args: &[&str]) -> Option<String> {
    let call = session.user_call(Path::new(program), args, SHORT);
    session.run(&call).ok().filter(|d| d.ok()).map(|d| d.out())
}

fn versions(session: &Session<'_>) -> BTreeMap<String, String> {
    let mut versions = BTreeMap::new();
    let mut put = |key: &str, value: Option<String>| {
        versions.insert(
            key.to_owned(),
            value.unwrap_or_else(|| "unknown".to_owned()),
        );
    };
    put(
        "macos",
        version_of(session, "/usr/bin/sw_vers", &["-productVersion"]),
    );
    put(
        "tart",
        session
            .tart(&["--version"], SHORT)
            .ok()
            .filter(|d| d.ok())
            .map(|d| d.out()),
    );
    put("rustc", version_of(session, "rustc", &["--version"]));
    put("cargo", version_of(session, "cargo", &["--version"]));
    let repo = session.env.repo.display().to_string();
    put(
        "mayfly",
        version_of(session, "git", &["-C", &repo, "rev-parse", "HEAD"]),
    );
    versions
}

fn write_report(path: &Path, report: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(report).map_err(|e| e.to_string())?;
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(format!("{text}\n").as_bytes()))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// One whole run: the session, every check, cleanup, the report. Takes every program and fact
/// from `env`, so it runs on Linux against fake-tart in tests.
pub fn execute(env: &Env, options: &Options) -> Outcome {
    let failed = |why: String| {
        eprintln!("xtask: hw01: {why}");
        Outcome {
            code: 2,
            report: Value::Null,
            leftovers: Vec::new(),
        }
    };
    let pins = match pins::load(&env.pins) {
        Ok(pins) => pins,
        Err(err) => return failed(err),
    };
    let images = vec![
        (
            "linux".to_owned(),
            pins.linux.repository.clone(),
            pins.linux.digest.clone(),
        ),
        (
            "macos".to_owned(),
            pins.macos.repository.clone(),
            pins.macos.digest.clone(),
        ),
    ];
    let mut session = match Session::create(env, pins) {
        Ok(session) => session,
        Err(err) => return failed(err),
    };
    let run_dir = session.run_dir.clone();
    eprintln!(
        "hw01: run directory {} (its record: {}); after a crash or a second Ctrl-C, run \
         `cargo xtask hw01 --cleanup {}`",
        run_dir.display(),
        run_dir.join("record.jsonl").display(),
        run_dir.display()
    );
    let versions = versions(&session);
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        checks::all(&mut session, options.macos, options.service_user.as_deref());
    }));
    let interrupted = session.interrupted();
    let leftovers = session.cleanup();
    let mut results = std::mem::take(&mut session.results);
    if checked.is_err() {
        results.push(report::CheckResult::fail(
            "hw01",
            "HW-01X",
            vec![Value::String(
                "a check panicked; the run cleaned up and stopped".to_owned(),
            )],
        ));
    }
    let mut identity = env.identity.clone();
    identity.service_user.clone_from(&options.service_user);
    let redactor = Redactor::new(&identity, &run_dir, &session.secrets);
    let report = Report {
        versions,
        options: BTreeMap::from([
            ("macos".to_owned(), options.macos),
            ("service_mode".to_owned(), options.service_user.is_some()),
        ]),
        images,
        checks: results,
        leftovers: leftovers.clone(),
        interrupted,
    };
    let json = report.to_json(&redactor);
    if let Err(err) = report::validate(&json) {
        eprintln!("xtask: hw01: the report does not match its schema: {err}");
        return Outcome {
            code: 1,
            report: json,
            leftovers,
        };
    }
    let path = options
        .report
        .clone()
        .unwrap_or_else(|| env.repo.join("target").join("hw01-report.json"));
    if let Err(err) = write_report(&path, &json) {
        eprintln!("xtask: hw01: {err}");
    } else {
        eprintln!("hw01: report written to {}", path.display());
    }
    let any_failed = report.checks.iter().any(|c| c.status == Status::Fail)
        || json["checks"]
            .as_array()
            .is_some_and(|c| c.iter().any(|c| c["status"] == "fail"));
    for check in json["checks"].as_array().into_iter().flatten() {
        eprintln!(
            "hw01: {:<8} {}",
            check["status"].as_str().unwrap_or_default(),
            check["id"].as_str().unwrap_or_default()
        );
    }
    let code = if interrupted {
        130
    } else if any_failed || !leftovers.is_empty() {
        1
    } else {
        0
    };
    Outcome {
        code,
        report: json,
        leftovers,
    }
}

#[cfg(test)]
mod tests;
