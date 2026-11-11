//! The formal toolchain: the Quint models in `formal/` and the stages that check them
//! (verification.md section 6, foundation.md sections 8 and 9, D10, D15).
//!
//! Ported from site-preview's `conformance` module and cut to what FND-06 owns: the registry of
//! models, `formal-typecheck` and `formal-test`. Later tasks add the scenario, simulation,
//! freshness and replay stages beside them (VER-01) and the Apalache lane (`formal_verify`).
//!
//! - `formal-typecheck`: `quint typecheck` on every `formal/*.qnt`; a file that is not in
//!   [`MODELS`], or a registered file that does not exist, fails too, so a model cannot sit in
//!   `formal/` unchecked by the other stages;
//! - `formal-test`: `quint test` on every registered model, which must run at least one test
//!   (`quint test` passes when nothing matches, so a model whose tests stop being collected
//!   must fail here, never pass as zero tests).
//!
//! Quint is the version `formal/package-lock.json` locks (`tools/versions.toml` notes it), run
//! with `npx --prefix formal --no`, which never fetches a package from the registry, and
//! always with the TypeScript backend: the default Rust backend downloads an evaluator binary
//! from GitHub at first use, which no lockfile pins (foundation.md section 8.4). Node is a
//! development tool only; the shipped binaries never need it. A missing Node or Quint fails
//! with the command that installs it (D15), never a skip.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::process::{Stage, finish, run_report};
use crate::{repo, tools};

/// One Quint model and the step relations that drive it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    /// File name in `formal/`; the main module has the same name without `.qnt`.
    pub file: &'static str,
    /// The step relations (`quint run` and `quint verify --step`). Every relation named here
    /// is bounded by an entry of [`crate::formal_verify::BOUNDED`].
    pub steps: &'static [&'static str],
}

/// The registry of models (verification.md section 6). The real models arrive with the tasks
/// that own them (`scaleset` with KER-06, `registration` with KER-09, `event_store` with ES-06,
/// `image_store` with DP-07).
/// Registering a
/// model is one entry here and one entry in [`crate::formal_verify::BOUNDED`] per step
/// relation; `should_register_every_model_file_when_formal_is_read` and
/// `should_bound_every_step_relation_when_models_are_registered` fail when either is missing.
pub const MODELS: &[Model] = &[
    Model {
        file: "smoke.qnt",
        steps: &["step"],
    },
    Model {
        file: "lifecycle.qnt",
        steps: &["step", "serveStep", "activationStep"],
    },
    Model {
        file: "link.qnt",
        steps: &["step", "implStep"],
    },
    Model {
        file: "scheduling.qnt",
        steps: &["step", "admitStep"],
    },
    Model {
        file: "recovery.qnt",
        steps: &["step", "restartStep", "repairStep"],
    },
    Model {
        file: "registration.qnt",
        steps: &["step", "raceStep"],
    },
    Model {
        file: "scaleset.qnt",
        steps: &["step", "messageStep"],
    },
    Model {
        file: "event_store.qnt",
        steps: &[
            "step",
            "implStepSerial",
            "relayStep",
            "storeOnlyStep",
            "deliveryOnlyStep",
        ],
    },
    Model {
        file: "image_store.qnt",
        steps: &["step"],
    },
];

/// The backend flags of every `quint run` and `quint test`: the TypeScript evaluator in
/// `formal/node_modules`, never the Rust one that Quint downloads at first use.
pub(crate) const BACKEND: [&str; 2] = ["--backend", "typescript"];

/// The stages `formal-typecheck` and `formal-test` run, in order. Both need Node and Quint:
/// `process::run_report` checks Node's pin, and each stage checks the locked Quint itself
/// ([`require_quint`], which `doctor` runs too) before it runs it.
pub fn stages() -> Vec<Stage> {
    vec![
        Stage::in_process(
            "formal-typecheck",
            "npx --prefix formal --no -- quint typecheck <every formal/*.qnt>",
            typecheck,
        )
        .needs("node")
        .and_needs("quint"),
        Stage::in_process(
            "formal-test",
            "npx --prefix formal --no -- quint test --backend typescript <every model of MODELS>",
            test,
        )
        .needs("node")
        .and_needs("quint"),
    ]
}

/// Runs the stage named `name` through the gate's report (`cargo xtask formal-typecheck` and
/// `formal-test`, the same code `check` runs) and returns the exit code.
pub fn run_stage(name: &str) -> i32 {
    let picked: Vec<Stage> = stages().into_iter().filter(|s| s.name == name).collect();
    finish(&run_report(&picked, &tools::require))
}

/// `formal/` in the repository.
pub(crate) fn formal_dir() -> PathBuf {
    repo::root().join("formal")
}

/// The locally installed Quint, never one fetched from the registry: `--no` refuses to
/// install, and `--` keeps npx from taking Quint's own flags (`--version`) as its own.
pub(crate) fn quint(formal: &Path) -> Command {
    let mut command = Command::new("npx");
    command
        .arg("--prefix")
        .arg(formal)
        .args(["--no", "--", "quint"])
        .current_dir(formal);
    command
}

/// The pinned Quint version from `tools/versions.toml`.
pub(crate) fn pinned_quint() -> Result<String, String> {
    tools::manifest()?
        .into_iter()
        .find(|t| t.name == "quint")
        .map(|t| t.version)
        .ok_or_else(|| "tools/versions.toml has no quint entry".to_owned())
}

/// The hint for a Quint that is not installed at `version`.
fn install_hint(version: &str) -> String {
    format!(
        "run 'npm ci --prefix formal' (Quint {version} is locked by formal/package-lock.json; \
         Node itself comes from https://nodejs.org/en/download)"
    )
}

/// Decides from `quint --version`'s result (`None`: npx could not run) whether the pinned
/// `version` is usable. Pinned by `should_fail_closed_when_quint_is_missing_or_wrong`.
fn classify(probe: Option<(bool, &str)>, version: &str) -> Result<(), String> {
    match probe {
        None => Err(format!(
            "cannot run npx: missing Node or npm; {}",
            install_hint(version)
        )),
        Some((false, _)) => Err(format!(
            "missing quint {version} in formal/node_modules: {}",
            install_hint(version)
        )),
        Some((true, out)) if out.split_whitespace().any(|t| t == version) => Ok(()),
        Some((true, out)) => Err(format!(
            "wrong version of quint (found '{}', want {version}): {}",
            out.lines().next().unwrap_or("").trim(),
            install_hint(version)
        )),
    }
}

/// Fails with the install hint unless `formal/node_modules` holds the pinned Quint.
pub(crate) fn require_quint(formal: &Path) -> Result<(), String> {
    let version = pinned_quint()?;
    let probe = quint(formal).arg("--version").output().ok().map(|o| {
        (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
        )
    });
    classify(probe.as_ref().map(|(ok, t)| (*ok, t.as_str())), &version)
}

/// Prints a finished command's output and returns whether it exited 0.
pub(crate) fn echo(output: &Output) -> bool {
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    output.status.success()
}

/// Runs `command`, reporting a failure to start it as a failed stage.
pub(crate) fn output_of(mut command: Command, label: &str) -> Option<Output> {
    match command.output() {
        Ok(output) => Some(output),
        Err(err) => {
            eprintln!("xtask: cannot run {label}: {err}");
            None
        }
    }
}

/// Prints `err` and returns the stage's failure code.
pub(crate) fn fail(err: &str) -> i32 {
    eprintln!("xtask: {err}");
    1
}

/// Every `*.qnt` directly in `dir`, sorted.
fn qnt_files(dir: &Path) -> Result<Vec<String>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("cannot read formal/: {e}"))?;
    let mut files: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| name.ends_with(".qnt"))
        .collect();
    files.sort();
    Ok(files)
}

/// The problems of `files` (the `.qnt` files on disk) against `models`: an empty `formal/`, a
/// file no model registers (its tests and invariants would never run), a registered file that
/// is not there. Pinned by `should_name_unregistered_and_missing_files_when_the_registry_differs`.
fn registry_problems(files: &[String], models: &[Model]) -> Vec<String> {
    let mut problems = Vec::new();
    if files.is_empty() {
        problems.push("no .qnt file in formal/".to_owned());
    }
    for file in files {
        if !models.iter().any(|m| m.file == file) {
            problems.push(format!(
                "formal/{file} is not in MODELS (tools/xtask/src/formal.rs): register it, so \
                 that formal-test and formal-verify cover it"
            ));
        }
    }
    for model in models {
        if !files.iter().any(|f| f == model.file) {
            problems.push(format!(
                "MODELS names formal/{}, which does not exist",
                model.file
            ));
        }
    }
    problems
}

fn typecheck() -> i32 {
    let formal = formal_dir();
    if let Err(err) = require_quint(&formal) {
        return fail(&err);
    }
    let files = match qnt_files(&formal) {
        Ok(files) => files,
        Err(err) => return fail(&err),
    };
    let problems = registry_problems(&files, MODELS);
    if !problems.is_empty() {
        return fail(&problems.join("\nxtask: "));
    }
    let outputs = in_parallel(&files, |file| {
        let mut command = quint(&formal);
        command.args(["typecheck", file]);
        output_of(command, "quint typecheck")
    });
    let mut failed = Vec::new();
    for (file, output) in files.iter().zip(outputs) {
        if output.is_some_and(|o| echo(&o)) {
            println!("typecheck ok: formal/{file}");
        } else {
            failed.push(file.as_str());
        }
    }
    if failed.is_empty() {
        0
    } else {
        fail(&format!("quint typecheck failed: {}", failed.join(", ")))
    }
}

/// The number before " passing" in `quint test`'s report, if any.
fn passing(report: &str) -> Option<usize> {
    report.lines().find_map(|line| {
        let (count, rest) = line.trim().split_once(' ')?;
        rest.starts_with("passing").then(|| count.parse().ok())?
    })
}

/// Whether a `quint test` run of one model counts as passed: it exited 0 and ran at least one
/// test. Pinned by `should_fail_a_model_when_quint_test_runs_no_test`.
fn tests_passed(exited_ok: bool, report: &str) -> bool {
    exited_ok && passing(report).is_some_and(|n| n > 0)
}

fn test() -> i32 {
    let formal = formal_dir();
    if let Err(err) = require_quint(&formal) {
        return fail(&err);
    }
    let files = match qnt_files(&formal) {
        Ok(files) => files,
        Err(err) => return fail(&err),
    };
    let problems = registry_problems(&files, MODELS);
    if !problems.is_empty() {
        return fail(&problems.join("\nxtask: "));
    }
    let outputs = in_parallel(MODELS, |model| {
        let mut command = quint(&formal);
        command.arg("test").args(BACKEND).arg(model.file);
        output_of(command, "quint test")
    });
    let mut failed = Vec::new();
    for (model, output) in MODELS.iter().zip(outputs) {
        let Some(output) = output else {
            failed.push(model.file);
            continue;
        };
        let exited = echo(&output);
        if !tests_passed(exited, &String::from_utf8_lossy(&output.stdout)) {
            if exited {
                eprintln!("xtask: formal/{} ran no test", model.file);
            }
            failed.push(model.file);
        }
    }
    if failed.is_empty() {
        0
    } else {
        fail(&format!("quint test failed: {}", failed.join(", ")))
    }
}

/// The run's seed, for simulation: `QUINT_SEED` when set (to repeat a run), else a fresh one,
/// chosen once per process.
pub(crate) fn seed() -> &'static str {
    static SEED: OnceLock<String> = OnceLock::new();
    SEED.get_or_init(|| {
        std::env::var("QUINT_SEED")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos());
                format!("{:#x}", nanos & u128::from(u64::MAX))
            })
    })
}

/// Runs `work` over `items` on as many threads as the machine has cores (each Quint run is
/// one Node process, mostly single-threaded), each thread taking the next item when it is
/// free, and returns the results in item order.
pub(crate) fn in_parallel<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    in_parallel_up_to(usize::MAX, items, work)
}

/// [`in_parallel`] on at most `limit` threads (a JVM run is memory-bound, not core-bound).
pub(crate) fn in_parallel_up_to<T: Sync, R: Send>(
    limit: usize,
    items: &[T],
    work: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let width = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(limit.max(1))
        .min(items.len().max(1));
    let next = AtomicUsize::new(0);
    let done: Mutex<Vec<(usize, R)>> = Mutex::new(Vec::with_capacity(items.len()));
    std::thread::scope(|scope| {
        for _ in 0..width {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    let result = work(item);
                    done.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((i, result));
                }
            });
        }
    });
    let mut done = done
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}

/// The names in a model's `val allInvariants = and { ... }` block: the one list of its
/// invariants, which every stage reads so that a new invariant cannot be left out
/// (verification.md section 6).
pub(crate) fn all_invariants(text: &str) -> Option<Vec<String>> {
    let start = text.find("val allInvariants = and {")?;
    let body = &text[start..];
    let body = &body[body.find('{')? + 1..body.find('}')?];
    Some(
        body.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_run_typecheck_then_test_when_the_formal_stages_are_built() {
        // given
        let built = stages();

        // when
        let names: Vec<_> = built.iter().map(|s| s.name).collect();

        // then
        assert_eq!(names, ["formal-typecheck", "formal-test"]);
        assert!(
            built
                .iter()
                .all(|s| s.tool == Some("node") && s.also_tool == Some("quint"))
        );
    }

    #[test]
    fn should_fail_closed_when_quint_is_missing_or_wrong() {
        // given
        let version = "0.33.1";

        // when
        let no_npx = classify(None, version).unwrap_err();
        let not_installed = classify(Some((false, "")), version).unwrap_err();
        let other = classify(Some((true, "0.30.0\n")), version).unwrap_err();
        let good = classify(Some((true, "0.33.1\n")), version);

        // then
        assert!(
            no_npx.contains("missing Node or npm") && no_npx.contains("npm ci --prefix formal"),
            "{no_npx}"
        );
        assert!(
            not_installed.starts_with("missing quint 0.33.1")
                && not_installed.contains("npm ci --prefix formal"),
            "{not_installed}"
        );
        assert!(
            other.starts_with("wrong version of quint (found '0.30.0', want 0.33.1)"),
            "{other}"
        );
        assert_eq!(good, Ok(()));
    }

    #[test]
    fn should_pin_the_quint_of_the_lockfile_when_the_manifest_is_read() {
        // given
        let lock = std::fs::read_to_string(formal_dir().join("package-lock.json")).unwrap();
        let version = pinned_quint().unwrap();

        // when
        let locked = lock
            .split("\"node_modules/@informalsystems/quint\": {")
            .nth(1)
            .and_then(|rest| rest.split("\"version\": \"").nth(1))
            .and_then(|rest| rest.split('"').next());

        // then
        assert_eq!(locked, Some(version.as_str()));
    }

    #[test]
    fn should_name_unregistered_and_missing_files_when_the_registry_differs() {
        // given
        let models = [Model {
            file: "a.qnt",
            steps: &["step"],
        }];

        // when
        let unregistered = registry_problems(&["a.qnt".into(), "b.qnt".into()], &models);
        let missing = registry_problems(&[], &models);
        let fine = registry_problems(&["a.qnt".into()], &models);

        // then
        assert_eq!(unregistered.len(), 1, "{unregistered:?}");
        assert!(unregistered[0].contains("formal/b.qnt is not in MODELS"));
        assert!(missing.iter().any(|p| p.contains("no .qnt file")));
        assert!(
            missing
                .iter()
                .any(|p| p.contains("formal/a.qnt, which does not exist"))
        );
        assert!(fine.is_empty(), "{fine:?}");
    }

    #[test]
    fn should_register_every_model_file_when_formal_is_read() {
        // given
        let files = qnt_files(&formal_dir()).unwrap();

        // when
        let problems = registry_problems(&files, MODELS);

        // then
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn should_list_at_least_one_invariant_when_a_model_is_registered() {
        // given
        let models = MODELS;

        // when
        let listed: Vec<_> = models
            .iter()
            .map(|m| {
                let text = std::fs::read_to_string(formal_dir().join(m.file)).unwrap();
                (m.file, all_invariants(&text).unwrap_or_default())
            })
            .collect();

        // then
        for (file, invariants) in &listed {
            assert!(
                !invariants.is_empty(),
                "formal/{file} has no 'val allInvariants = and {{ ... }}'"
            );
        }
        assert!(!MODELS.is_empty());
        assert!(
            MODELS.iter().all(|m| !m.steps.is_empty()),
            "every model has a step relation"
        );
    }

    #[test]
    fn should_read_every_conjunct_when_all_invariants_is_listed() {
        // given
        let text = "val allInvariants = and {\n  a,\n  b,\n}\nval other = 1";

        // when
        let names = all_invariants(text);

        // then
        assert_eq!(names, Some(vec!["a".to_owned(), "b".to_owned()]));
        assert_eq!(all_invariants("val x = 1"), None);
    }

    #[test]
    fn should_fail_a_model_when_quint_test_runs_no_test() {
        // given
        let none = "\n  m\n\n  0 passing (1ms)\n";
        let some = "\n  m\n    ok a passed 1 test(s)\n\n  1 passing (7ms)\n";

        // when
        let (zero, one, failed) = (
            tests_passed(true, none),
            tests_passed(true, some),
            tests_passed(false, some),
        );

        // then
        assert!(!zero, "a model with no test passes nothing");
        assert!(one);
        assert!(
            !failed,
            "a non-zero exit is a failure whatever the report says"
        );
        assert_eq!(passing("no summary"), None);
    }

    #[test]
    fn should_run_every_item_once_and_keep_order_when_work_is_spread() {
        // given
        let items: Vec<u32> = (0..50).collect();

        // when
        let doubled = in_parallel(&items, |n| n * 2);
        let limited = in_parallel_up_to(1, &items, |n| n + 1);

        // then
        assert_eq!(doubled, (0..50).map(|n| n * 2).collect::<Vec<_>>());
        assert_eq!(limited, (1..51).collect::<Vec<_>>());
    }
}
