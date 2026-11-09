//! `cargo xtask formal-verify`: the deep lane for the Quint models (verification.md section 6.3,
//! foundation.md section 9). It is not part of `check`: Apalache is a JVM tool and its runs take
//! minutes, so `.github/workflows/formal.yml` runs it on changes under `formal/` and weekly.
//!
//! - `formal-verify-bounded`: `quint verify` (Apalache, symbolic bounded model checking) of
//!   each model's `allInvariants` under each of its step relations, to the depth in
//!   [`BOUNDED`]. A pass means no state reachable within that many steps violates an
//!   invariant, for every choice of every nondeterministic value (not a sample);
//! - `formal-verify-simulate`: `quint run` of the same invariants over [`SIM_SEEDS`] seeds of
//!   [`SIM_SAMPLES`] traces of [`SIM_STEPS`] steps each, beyond the depth Apalache reaches.
//!
//! Apalache is the version `tools/versions.toml` pins. Before Quint runs, the xtask fetches
//! that release from GitHub, refuses it unless its SHA-256 is the pinned one, and unpacks it
//! into `~/.quint/apalache-dist-<version>` (`crate::apalache`), where Quint finds it and starts
//! it as a local server instead of downloading it unchecked; the xtask passes
//! `--apalache-version` so a Quint upgrade cannot change the pin, and one run per model and
//! relation gets its own server port. A missing JVM, a JVM older than [`MIN_JAVA`] or an
//! Apalache that cannot be downloaded fails with what to do about it (D15); it is never a skip,
//! and it is never reported as a verification result: only Quint's own `[violation]` is a
//! counterexample, any other failure is a tooling failure that verified nothing.
//!
//! Every run's output is kept in `target/formal-verify/<label>.log`, Apalache's own run
//! directories (with a counterexample as ITF and TLA+ when it finds one) under
//! `target/formal-verify/_apalache-out/`, and a failing simulation is repeated with
//! `--out-itf` so its trace is a file; the workflow uploads that directory.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use crate::formal::{
    BACKEND, MODELS, all_invariants, echo, fail, formal_dir, in_parallel, in_parallel_up_to,
    output_of, quint, require_quint, seed,
};
use crate::process::{Stage, finish, run_report};
use crate::{apalache, repo, tools};

/// One bounded verification: a model under one step relation, to `steps` steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bound {
    /// File name in `formal/`.
    pub file: &'static str,
    /// The step relation (`quint verify --step`).
    pub step: &'static str,
    /// Depth: every trace of up to this many steps is covered.
    pub steps: u32,
}

/// The bounded verifications, one per step relation of every entry of [`MODELS`]. Apalache
/// explores every enabled action at every step, so the cost grows with the number of actions
/// in the relation and the size of the state: a model's depth is the one that finishes in a few
/// minutes on a hosted runner, recorded with its duration in `doc/formal-models.md`.
/// Pinned by `should_bound_every_step_relation_when_models_are_registered`.
pub const BOUNDED: &[Bound] = &[
    Bound {
        file: "smoke.qnt",
        step: "step",
        steps: 6,
    },
    Bound {
        file: "lifecycle.qnt",
        step: "step",
        steps: 8,
    },
    Bound {
        file: "lifecycle.qnt",
        step: "serveStep",
        steps: 8,
    },
];

/// Seeds of the multi-seed simulation, derived from one printed base seed.
pub const SIM_SEEDS: u32 = 3;

/// Traces per seed, model and relation: a deep run, far beyond what a per-push gate samples.
pub const SIM_SAMPLES: u32 = 5_000;

/// Steps per trace.
pub const SIM_STEPS: u32 = 50;

/// The oldest JVM the pinned Apalache 0.62.1 runs on (class file version 65).
pub const MIN_JAVA: u32 = 21;

/// JVM runs in parallel: each takes up to [`JVM_ARGS`]'s heap, so the cores are not the limit.
const PARALLEL_JVMS: usize = 2;

/// Added to `JVM_ARGS` for the Apalache server: a thread stack deep enough for a model's
/// deeply nested `match` (the JVM's default overflows in Apalache's translation, which
/// surfaces as an empty `error:` after the parser pass) and a heap that two servers can
/// share on a 16 GB runner.
const JVM_ARGS: &str = "-Xss64m -Xmx3g";

/// Apalache's tuning for every bounded run, as `--apalache-config`: check the invariants once
/// after each step instead of before every candidate transition. The default re-checks them
/// along each of the exponentially many paths and took about three times as long at the same
/// depth in the measurements recorded in `doc/formal-models.md`; the verdict is the same.
const APALACHE_CONFIG: &str = r#"{"checker":{"tuning":{"search.invariant.mode":"after"}}}"#;

/// The file name of [`APALACHE_CONFIG`] in [`out_dir`].
const APALACHE_CONFIG_FILE: &str = "apalache-config.json";

/// Apalache's output directory, a child of [`out_dir`].
const APALACHE_OUT: &str = "_apalache-out";

/// A prefix of the port search; every run gets a free port at or above it.
const FIRST_PORT: u16 = 8822;

/// The stages `formal-verify` runs, in order.
pub fn stages() -> Vec<Stage> {
    vec![
        Stage::in_process(
            "formal-verify-bounded",
            "npx --prefix formal --no -- quint verify --step <relation> --invariants <all> --max-steps <depth> --apalache-version <pinned> formal/<model>.qnt",
            bounded,
        )
        .needs("node")
        .and_needs("quint"),
        Stage::in_process(
            "formal-verify-simulate",
            "npx --prefix formal --no -- quint run --backend typescript --step <relation> --invariants <all> --max-samples 5000 --max-steps 50 --seed <seed 1..3> formal/<model>.qnt",
            simulate,
        )
        .needs("node")
        .and_needs("quint"),
    ]
}

/// Runs `formal-verify` and returns the exit code.
pub fn run() -> i32 {
    finish(&run_report(&stages(), &tools::require))
}

fn out_dir() -> PathBuf {
    repo::root().join("target").join("formal-verify")
}

/// The pinned Apalache version from `tools/versions.toml`.
fn pinned_apalache() -> Result<String, String> {
    tools::manifest()?
        .into_iter()
        .find(|t| t.name == "apalache")
        .map(|t| t.version)
        .ok_or_else(|| "tools/versions.toml has no apalache entry".to_owned())
}

/// The first number of the JVM's major version in `java -version`'s output (`"21.0.1"` is 21,
/// `"1.8.0_392"` is 8), skipping any `Picked up ...` line before it.
fn java_major(text: &str) -> Option<u32> {
    let line = text.lines().find(|l| l.contains("version \""))?;
    let version = line.split('"').nth(1)?;
    let mut parts = version.split(['.', '_', '-', '+']);
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 {
        parts.next()?.parse().ok()
    } else {
        Some(first)
    }
}

/// Decides from `java -version`'s result (`None`: `java` could not run).
fn classify_java(probe: Option<(bool, &str)>) -> Result<(), String> {
    let hint = format!(
        "install a JDK {MIN_JAVA} or newer (for example `apt-get install openjdk-21-jre-headless`, \
         or https://adoptium.net) and put `java` on PATH; Apalache is a JVM tool and runs only in \
         the test environment, never in the shipped binary"
    );
    match probe {
        None | Some((false, _)) => Err(format!("cannot run java: {hint}")),
        Some((true, text)) => match java_major(text) {
            Some(major) if major >= MIN_JAVA => Ok(()),
            Some(major) => Err(format!(
                "java {major} is too old for Apalache (need {MIN_JAVA} or newer): {hint}"
            )),
            None => Err(format!(
                "cannot read the version of java from '{}': {hint}",
                text.lines().next().unwrap_or("").trim()
            )),
        },
    }
}

/// `java -version` writes to stderr.
pub(crate) fn require_java() -> Result<(), String> {
    let probe = std::process::Command::new("java")
        .arg("-version")
        .output()
        .ok()
        .map(|o| {
            let text = String::from_utf8_lossy(&o.stderr).into_owned()
                + &String::from_utf8_lossy(&o.stdout);
            (o.status.success(), text)
        });
    classify_java(probe.as_ref().map(|(ok, t)| (*ok, t.as_str())))
}

/// Where Quint runs the Apalache release from (`$QUINT_HOME`, else `~/.quint`), and where
/// [`apalache::ensure_pinned`] unpacks the verified release; `doctor` probes it.
pub fn apalache_binary(version: &str) -> Option<PathBuf> {
    apalache::dist_dir(version).map(|dist| dist.join("apalache").join("bin").join("apalache-mc"))
}

/// The D9 hint for an Apalache release that cannot be fetched.
fn apalache_hint(version: &str) -> String {
    let place = apalache_binary(version).map_or_else(
        || format!("~/.quint/apalache-dist-{version}"),
        |p| p.display().to_string(),
    );
    format!(
        "formal-verify fetches Apalache {version} from \
         https://github.com/apalache-mc/apalache/releases/tag/v{version} (the release and SHA-256 \
         pinned in tools/versions.toml) and unpacks it so that {place} exists; allow github.com"
    )
}

/// Everything the stages need, checked once.
struct Context {
    formal: PathBuf,
    out: PathBuf,
    apalache: String,
    /// Whether the Apalache release is already on disk (else the first run downloads it).
    fetched: bool,
}

/// Checks Node and Quint, the JVM and the pinned Apalache, and creates the output directory.
fn preflight() -> Result<Context, String> {
    let formal = formal_dir();
    require_quint(&formal)?;
    require_java()?;
    let apalache = pinned_apalache()?;
    // Fetched and checked against the pinned SHA-256 here, so Quint never downloads it.
    apalache::ensure_pinned(&apalache)?;
    let out = out_dir();
    std::fs::create_dir_all(out.join(APALACHE_OUT))
        .map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    std::fs::write(out.join(APALACHE_CONFIG_FILE), APALACHE_CONFIG)
        .map_err(|e| format!("cannot write the Apalache config in {}: {e}", out.display()))?;
    let fetched = apalache_binary(&apalache).is_some_and(|p| p.is_file());
    Ok(Context {
        formal,
        out,
        apalache,
        fetched,
    })
}

/// A port nothing listens on, found by binding port 0, at or above [`FIRST_PORT`] when the
/// operating system allows (it does not matter which: Quint starts a server where none runs).
fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map_or(FIRST_PORT, |a| a.port())
}

/// `quint verify` arguments for `bound`, checking `invariants`.
fn verify_args(
    file: &Path,
    bound: &Bound,
    invariants: &[String],
    apalache: &str,
    config: &Path,
    port: u16,
) -> Vec<String> {
    let mut args = vec![
        "verify".to_owned(),
        file.display().to_string(),
        "--step".to_owned(),
        bound.step.to_owned(),
        "--invariants".to_owned(),
    ];
    args.extend(invariants.iter().cloned());
    args.extend([
        "--max-steps".to_owned(),
        bound.steps.to_string(),
        "--apalache-version".to_owned(),
        apalache.to_owned(),
        "--apalache-config".to_owned(),
        config.display().to_string(),
        "--server-endpoint".to_owned(),
        format!("localhost:{port}"),
    ]);
    args
}

/// The caller's `JVM_ARGS` followed by ours, so a CI heap limit still applies.
fn jvm_args() -> String {
    match std::env::var("JVM_ARGS") {
        Ok(existing) if !existing.trim().is_empty() => format!("{existing} {JVM_ARGS}"),
        _ => JVM_ARGS.to_owned(),
    }
}

/// A file-name-safe label of a run.
fn label(prefix: &str, file: &str, step: &str, suffix: &str) -> String {
    let model = file.trim_end_matches(".qnt");
    format!("{prefix}-{model}-{step}{suffix}")
}

/// Keeps a run's output next to the others for the workflow's artifact.
fn keep_log(out: &Path, label: &str, output: &Output) {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if let Err(err) = std::fs::write(out.join(format!("{label}.log")), text) {
        eprintln!("xtask: cannot keep the log of {label}: {err}");
    }
}

/// The result line of a successful `quint verify` or `quint run`.
fn verdict(output: &Output) -> String {
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find(|l| l.starts_with("[ok]"))
        .unwrap_or("[ok]")
        .to_owned()
}

/// What a failed run was: a counterexample (the tool says `[violation]`) or a failure of the
/// tooling, which verifies nothing.
fn failure_kind(output: &Output) -> &'static str {
    let text = String::from_utf8_lossy(&output.stdout);
    if text.contains("[violation]") {
        "counterexample"
    } else {
        "tooling failure (nothing was verified)"
    }
}

/// Prints the last lines of a failed run's output.
fn echo_tail(output: &Output) {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let lines: Vec<&str> = text.lines().collect();
    let from = lines.len().saturating_sub(80);
    for line in &lines[from..] {
        eprintln!("{line}");
    }
}

/// One bounded run to execute.
struct Job<'a> {
    bound: &'a Bound,
    invariants: Vec<String>,
}

/// Runs one bounded verification and returns its output and duration.
fn verify_one(ctx: &Context, job: &Job<'_>) -> (Option<Output>, Duration) {
    let start = Instant::now();
    let file = ctx.formal.join(job.bound.file);
    let mut command = quint(&ctx.formal);
    command
        .args(verify_args(
            &file,
            job.bound,
            &job.invariants,
            &ctx.apalache,
            &ctx.out.join(APALACHE_CONFIG_FILE),
            free_port(),
        ))
        .current_dir(&ctx.out)
        .env("JVM_ARGS", jvm_args());
    let output = output_of(command, "quint verify");
    if let Some(output) = &output {
        keep_log(
            &ctx.out,
            &label("verify", job.bound.file, job.bound.step, ""),
            output,
        );
    }
    (output, start.elapsed())
}

/// The invariants listed in `val allInvariants = and { ... }` of each model file.
fn invariants_of(formal: &Path, file: &str) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(formal.join(file))
        .map_err(|e| format!("cannot read formal/{file}: {e}"))?;
    all_invariants(&text)
        .filter(|i| !i.is_empty())
        .ok_or_else(|| {
            format!(
                "formal/{file} has no 'val allInvariants = and {{ ... }}' listing its invariants"
            )
        })
}

fn bounded() -> i32 {
    let ctx = match preflight() {
        Ok(ctx) => ctx,
        Err(err) => return fail(&err),
    };
    let mut jobs = Vec::new();
    for bound in BOUNDED {
        match invariants_of(&ctx.formal, bound.file) {
            Ok(invariants) => jobs.push(Job { bound, invariants }),
            Err(err) => return fail(&err),
        }
    }
    println!(
        "formal-verify-bounded: Apalache {} ({} runs, up to {PARALLEL_JVMS} at a time, logs in {})",
        ctx.apalache,
        jobs.len(),
        ctx.out.display()
    );
    let mut results = Vec::new();
    let (first, rest) = if ctx.fetched {
        (&jobs[..0], &jobs[..])
    } else {
        // The first run downloads the release; two at once would unpack it twice.
        println!("formal-verify-bounded: Apalache is not on disk; the first run downloads it");
        jobs.split_at(1)
    };
    results.extend(first.iter().map(|job| verify_one(&ctx, job)));
    results.extend(in_parallel_up_to(PARALLEL_JVMS, rest, |job| {
        verify_one(&ctx, job)
    }));
    let mut failed = Vec::new();
    for (job, (output, took)) in jobs.iter().zip(results) {
        let name = format!("{} --step {}", job.bound.file, job.bound.step);
        let depth = job.bound.steps;
        match output {
            Some(output) if output.status.success() => println!(
                "{name} to {depth} steps: {} in {:.0}s",
                verdict(&output),
                took.as_secs_f64()
            ),
            Some(output) => {
                echo_tail(&output);
                eprintln!(
                    "xtask: {name} to {depth} steps: {} after {:.0}s",
                    failure_kind(&output),
                    took.as_secs_f64()
                );
                failed.push(name);
            }
            None => failed.push(name),
        }
    }
    if failed.is_empty() {
        return 0;
    }
    let hint = if ctx.fetched {
        String::new()
    } else {
        format!("; {}", apalache_hint(&ctx.apalache))
    };
    fail(&format!(
        "bounded verification failed for {}; logs and Apalache's counterexamples are in {}{hint}",
        failed.join(", "),
        ctx.out.display()
    ))
}

/// The seeds of the multi-seed simulation: `base` and its successors, spread apart.
fn sim_seeds(base: &str) -> Result<Vec<String>, String> {
    let parsed = base
        .strip_prefix("0x")
        .map_or_else(|| base.parse::<u64>(), |hex| u64::from_str_radix(hex, 16))
        .map_err(|_| format!("QUINT_SEED '{base}' is not a number (decimal or 0x hex)"))?;
    Ok((0..SIM_SEEDS)
        .map(|i| {
            let spread = parsed.wrapping_add(u64::from(i).wrapping_mul(0x9e37_79b9_7f4a_7c15));
            format!("{spread:#x}")
        })
        .collect())
}

/// `quint run` arguments for one model, relation and seed.
fn sim_args(file: &str, step: &str, invariants: &[String], seed: &str) -> Vec<String> {
    let mut args: Vec<String> = ["run", file, BACKEND[0], BACKEND[1], "--step", step]
        .map(str::to_owned)
        .to_vec();
    args.push("--invariants".to_owned());
    args.extend(invariants.iter().cloned());
    args.extend([
        "--max-samples".to_owned(),
        SIM_SAMPLES.to_string(),
        "--max-steps".to_owned(),
        SIM_STEPS.to_string(),
        "--seed".to_owned(),
        seed.to_owned(),
    ]);
    args
}

fn simulate() -> i32 {
    let formal = formal_dir();
    if let Err(err) = require_quint(&formal) {
        return fail(&err);
    }
    let out = out_dir();
    if let Err(err) = std::fs::create_dir_all(&out) {
        return fail(&format!("cannot create {}: {err}", out.display()));
    }
    let seeds = match sim_seeds(seed()) {
        Ok(seeds) => seeds,
        Err(err) => return fail(&err),
    };
    println!(
        "formal-verify-simulate: seeds {} (QUINT_SEED={} repeats them)",
        seeds.join(", "),
        seed()
    );
    let mut runs = Vec::new();
    for model in MODELS {
        let invariants = match invariants_of(&formal, model.file) {
            Ok(invariants) => invariants,
            Err(err) => return fail(&err),
        };
        for step in model.steps {
            for seed in &seeds {
                runs.push((model.file, *step, invariants.clone(), seed.clone()));
            }
        }
    }
    let outputs = in_parallel(&runs, |(file, step, invariants, seed)| {
        let mut command = quint(&formal);
        command.args(sim_args(file, step, invariants, seed));
        let output = output_of(command, "quint run");
        if let Some(output) = &output {
            keep_log(
                &out,
                &label("simulate", file, step, &format!("-{seed}")),
                output,
            );
        }
        output
    });
    let mut failed = Vec::new();
    for ((file, step, invariants, seed), output) in runs.iter().zip(outputs) {
        let name = format!("{file} --step {step} --seed {seed}");
        match output {
            Some(output) if output.status.success() => {
                println!("{name}: {}", verdict(&output));
            }
            other => {
                if let Some(output) = &other {
                    echo(output);
                }
                // The same seed finds the same trace: repeat it for a file of the trace.
                let itf = out.join(label("simulate", file, step, &format!("-{seed}.itf.json")));
                let mut again = quint(&formal);
                again
                    .args(sim_args(file, step, invariants, seed))
                    .arg("--out-itf")
                    .arg(&itf);
                let _ = output_of(again, "quint run --out-itf");
                failed.push(name);
            }
        }
    }
    if failed.is_empty() {
        0
    } else {
        fail(&format!(
            "invariant violated or simulation failed: {}; traces in {}",
            failed.join(", "),
            out.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_bound_every_step_relation_when_models_are_registered() {
        // given
        let registered: Vec<(&str, &str)> = MODELS
            .iter()
            .flat_map(|m| m.steps.iter().map(|s| (m.file, *s)))
            .collect();

        // when
        let bounded: Vec<(&str, &str)> = BOUNDED.iter().map(|b| (b.file, b.step)).collect();

        // then
        assert_eq!(registered, bounded);
        assert!(BOUNDED.iter().all(|b| b.steps > 0));
    }

    #[test]
    fn should_keep_the_stages_out_of_check_when_the_lane_is_listed() {
        // given
        let stages = stages();

        // when
        let rows: Vec<_> = stages
            .iter()
            .map(|s| crate::catalog::find(s.name).expect("listed in the catalog"))
            .collect();

        // then
        assert!(rows.iter().all(|r| !r.in_check && r.owner == "FND-06"));
        assert!(crate::catalog::find("formal-verify").is_some_and(|c| !c.in_check));
    }

    #[test]
    fn should_run_the_bounded_and_the_simulation_stages_when_formal_verify_is_built() {
        // given
        // when
        let names: Vec<&str> = stages().iter().map(|s| s.name).collect();

        // then
        assert_eq!(names, ["formal-verify-bounded", "formal-verify-simulate"]);
    }

    #[test]
    fn should_pin_the_apalache_release_when_the_manifest_is_read() {
        // given
        // when
        let version = pinned_apalache().unwrap();

        // then
        assert!(
            version.chars().next().unwrap().is_ascii_digit(),
            "{version}"
        );
    }

    #[test]
    fn should_read_the_major_when_java_prints_its_version() {
        // given
        let modern =
            "Picked up JAVA_TOOL_OPTIONS: -Dx=y\nopenjdk version \"21.0.12.1\" 2026-08-18\n";
        let legacy = "java version \"1.8.0_392\"\n";
        let early_access = "openjdk version \"25-ea\" 2026-09-15\n";

        // when
        // then
        assert_eq!(java_major(modern), Some(21));
        assert_eq!(java_major(legacy), Some(8));
        assert_eq!(java_major(early_access), Some(25));
        assert_eq!(java_major("command output without a version"), None);
    }

    #[test]
    fn should_explain_the_fix_when_java_is_missing_or_too_old() {
        // given
        let old = "openjdk version \"11.0.2\" 2019-01-15\n";

        // when
        let missing = classify_java(None).unwrap_err();
        let too_old = classify_java(Some((true, old))).unwrap_err();
        let unreadable = classify_java(Some((true, "no version here"))).unwrap_err();

        // then
        assert!(missing.contains("cannot run java") && missing.contains("install a JDK 21"));
        assert!(too_old.contains("java 11 is too old") && too_old.contains("need 21 or newer"));
        assert!(unreadable.contains("cannot read the version"));
        assert!(classify_java(Some((true, "openjdk version \"21.0.1\"\n"))).is_ok());
        assert!(classify_java(Some((true, "openjdk version \"25\" 2026-09-15\n"))).is_ok());
    }

    #[test]
    fn should_name_the_release_and_the_place_when_apalache_cannot_be_fetched() {
        // given
        // when
        let hint = apalache_hint("0.62.1");

        // then
        assert!(hint.contains("releases/tag/v0.62.1"), "{hint}");
        assert!(hint.contains("apalache-dist-0.62.1"), "{hint}");
    }

    #[test]
    fn should_pass_the_pin_the_depth_and_a_port_when_verification_args_are_built() {
        // given
        let bound = Bound {
            file: "m.qnt",
            step: "next",
            steps: 7,
        };
        let invariants = vec!["a".to_owned(), "b".to_owned()];

        // when
        let args = verify_args(
            Path::new("/f/m.qnt"),
            &bound,
            &invariants,
            "0.62.1",
            Path::new("/o/c.json"),
            9000,
        );

        // then
        assert_eq!(
            args,
            [
                "verify",
                "/f/m.qnt",
                "--step",
                "next",
                "--invariants",
                "a",
                "b",
                "--max-steps",
                "7",
                "--apalache-version",
                "0.62.1",
                "--apalache-config",
                "/o/c.json",
                "--server-endpoint",
                "localhost:9000",
            ]
        );
    }

    #[test]
    fn should_spread_distinct_seeds_when_a_base_seed_is_given() {
        // given
        // when
        let hex = sim_seeds("0x10").unwrap();
        let decimal = sim_seeds("16").unwrap();
        let mut unique = hex.clone();
        unique.sort();
        unique.dedup();

        // then
        assert_eq!(hex.len(), SIM_SEEDS as usize);
        assert_eq!(hex[0], "0x10");
        assert_eq!(hex, decimal);
        assert_eq!(unique.len(), hex.len());
        assert!(sim_seeds("seed").is_err());
    }

    #[test]
    fn should_use_the_typescript_backend_when_simulation_args_are_built() {
        // given
        let invariants = vec!["a".to_owned()];

        // when
        let args = sim_args("m.qnt", "step", &invariants, "0x1");

        // then
        assert!(args.windows(2).any(|w| w == ["--max-samples", "5000"]));
        assert!(args.windows(2).any(|w| w == ["--max-steps", "50"]));
        assert!(args.windows(2).any(|w| w == ["--seed", "0x1"]));
        assert!(args.windows(2).any(|w| w == BACKEND));
    }

    #[test]
    fn should_keep_the_callers_jvm_args_when_the_stack_is_raised() {
        // given
        // when
        let args = jvm_args();

        // then
        assert!(args.ends_with(JVM_ARGS), "{args}");
    }

    #[cfg(unix)]
    fn finished(code: i32, stdout: &str, stderr: &str) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    #[cfg(unix)]
    fn should_tell_a_counterexample_from_a_tooling_failure_when_a_run_fails() {
        // given
        let violation = finished(1, "[violation] Found an issue (12ms).\n", "");
        let crash = finished(1, "", "error: Ran out of heap memory\n");
        let pass = finished(0, "noise\n[ok] No violation found (5ms).\n", "");

        // when
        // then
        assert_eq!(failure_kind(&violation), "counterexample");
        assert!(failure_kind(&crash).starts_with("tooling failure"));
        assert_eq!(verdict(&pass), "[ok] No violation found (5ms).");
        assert_eq!(verdict(&crash), "[ok]");
    }

    #[test]
    #[cfg(unix)]
    fn should_keep_the_log_of_a_run_when_it_finishes() {
        // given
        let dir = tempfile::tempdir().unwrap();
        let output = finished(0, "out\n", "err\n");

        // when
        keep_log(dir.path(), "verify-m-step", &output);
        echo_tail(&output);

        // then
        let text = std::fs::read_to_string(dir.path().join("verify-m-step.log")).unwrap();
        assert_eq!(text, "out\nerr\n");
    }

    #[test]
    fn should_label_runs_by_model_relation_and_seed_when_logs_are_named() {
        // given
        // when
        let verify = label("verify", "lifecycle.qnt", "serverStep", "");
        let sim = label("simulate", "reload.qnt", "step", "-0x1.itf.json");

        // then
        assert_eq!(verify, "verify-lifecycle-serverStep");
        assert_eq!(sim, "simulate-reload-step-0x1.itf.json");
    }

    #[test]
    fn should_read_the_invariants_when_a_model_lists_them() {
        // given
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.qnt"),
            "module a {\n  val allInvariants = and {\n    one,\n    two,\n  }\n}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("b.qnt"), "module b {}\n").unwrap();

        // when
        let listed = invariants_of(dir.path(), "a.qnt").unwrap();
        let none = invariants_of(dir.path(), "b.qnt").unwrap_err();
        let missing = invariants_of(dir.path(), "c.qnt").unwrap_err();

        // then
        assert_eq!(listed, ["one", "two"]);
        assert!(none.contains("allInvariants"), "{none}");
        assert!(missing.contains("cannot read"), "{missing}");
    }

    #[test]
    fn should_look_for_the_release_in_the_quint_home_when_the_binary_is_located() {
        // given
        // when
        let path = apalache_binary("0.62.1");

        // then
        if let Some(path) = path {
            assert!(path.ends_with("apalache-dist-0.62.1/apalache/bin/apalache-mc"));
        }
        assert!(free_port() > 0);
        assert!(APALACHE_CONFIG.contains("search.invariant.mode"));
    }
}
