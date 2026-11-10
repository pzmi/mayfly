//! Conformance (verification.md section 6.2, VER-01): seeded trace generation with `quint run
//! --mbt`, the freshness of the committed regression traces, and replay of both through the
//! drivers of `mayfly-conformance`.
//!
//! - `formal-freshness` (stage): regenerates every trace of
//!   `formal/traces/regression/manifest.toml` with the locked Quint, from the repository root,
//!   and requires it to equal the committed file byte for byte, `#meta.description` and
//!   `#meta.timestamp` (the creation time Quint writes) excepted;
//! - `conformance-replay` (stage): generates [`GATE_TRACES`] traces of [`GATE_STEPS`] steps per
//!   step relation of every driven model under one printed seed, then replays the regression
//!   manifest and the generated one with `cargo run -p mayfly-conformance`;
//! - `cargo xtask conformance [--model M] [--seed S] [--traces K] [--steps N]`: the same
//!   generation and replay with any budget, which is how a failed run is repeated (its seed is
//!   always printed) and what the weekly lane runs with a larger budget.
//!
//! The negative controls and the per-field sensitivity checks are tests of
//! `mayfly-conformance` (the `integration-tests` stage), over the committed traces, so they need
//! no Node. Quint is run as `formal` runs it (`npx --no`, the TypeScript backend); a model
//! invariant violated while generating fails the run like a divergence.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::formal::{BACKEND, echo, fail, formal_dir, in_parallel, output_of, require_quint};
use crate::process::Stage;
use crate::repo;

/// Traces per step relation of each driven model in the gate: three relations of the
/// lifecycle model make the 30 traces of 25 steps per model of verification.md section 6.2.
pub const GATE_TRACES: u32 = 10;
/// Steps per generated trace in the gate.
pub const GATE_STEPS: u32 = 25;

/// A model with a driver, and the relations its traces are generated from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Driven {
    /// The main module (`formal/<name>.qnt`).
    pub name: &'static str,
    /// Its init relation.
    pub init: &'static str,
    /// The step relations traces are generated from.
    pub steps: &'static [&'static str],
}

/// The models `mayfly-conformance` has a driver for (its `run::DRIVEN`). VER-02 and ES-07
/// add theirs.
pub const DRIVEN: &[Driven] = &[Driven {
    name: "lifecycle",
    init: "init",
    steps: &["step", "serveStep", "activationStep"],
}];

/// The budget of one generation run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    /// The seed every relation is run with.
    pub seed: String,
    /// Traces per relation.
    pub traces: u32,
    /// Steps per trace.
    pub steps: u32,
}

/// The stages `check` runs for conformance.
pub fn stages() -> Vec<Stage> {
    vec![
        Stage::in_process(
            "formal-freshness",
            "regenerate every trace of formal/traces/regression/manifest.toml with the locked Quint and compare it with the committed file",
            freshness,
        )
        .needs("node")
        .and_needs("quint"),
        Stage::in_process(
            "conformance-replay",
            "quint run --mbt: 10 traces of 25 steps per relation of each driven model under a printed seed, then cargo run -p mayfly-conformance over them and the regression manifest",
            gate_replay,
        )
        .needs("node")
        .and_needs("quint"),
    ]
}

/// `cargo xtask conformance`: generate under `budget` for `model` (all driven models when
/// `None`), then replay the generated and the regression traces.
pub fn run(model: Option<&str>, seed: Option<&str>, traces: u32, steps: u32) -> i32 {
    let models: Vec<Driven> = match model {
        None => DRIVEN.to_vec(),
        Some(name) => {
            let name = name.strip_suffix(".qnt").unwrap_or(name);
            match DRIVEN.iter().find(|d| d.name == name) {
                Some(d) => vec![*d],
                None => {
                    let names: Vec<_> = DRIVEN.iter().map(|d| d.name).collect();
                    eprintln!(
                        "xtask: no conformance driver for model {name}; choose from {}",
                        names.join(", ")
                    );
                    return 2;
                }
            }
        }
    };
    let budget = Budget {
        seed: seed.map_or_else(|| crate::formal::seed().to_owned(), str::to_owned),
        traces,
        steps,
    };
    generate_and_replay(&models, &budget)
}

fn gate_replay() -> i32 {
    let budget = Budget {
        seed: crate::formal::seed().to_owned(),
        traces: GATE_TRACES,
        steps: GATE_STEPS,
    };
    generate_and_replay(DRIVEN, &budget)
}

/// The command that repeats a run of `budget`.
fn repeat_hint(budget: &Budget) -> String {
    format!(
        "cargo xtask conformance --seed {} --traces {} --steps {}",
        budget.seed, budget.traces, budget.steps
    )
}

fn generate_and_replay(models: &[Driven], budget: &Budget) -> i32 {
    println!(
        "conformance: seed {} ({} traces of {} steps per relation; repeat with '{}')",
        budget.seed,
        budget.traces,
        budget.steps,
        repeat_hint(budget)
    );
    let root = repo::root();
    if let Err(err) = require_quint(&formal_dir()) {
        return fail(&err);
    }
    let dir = root
        .join("target")
        .join("conformance")
        .join(sanitise(&budget.seed));
    let manifest = match generate(&root, &dir, models, budget) {
        Ok(manifest) => manifest,
        Err(err) => {
            return fail(&format!(
                "{err} (traces kept in {}; repeat with '{}')",
                dir.display(),
                repeat_hint(budget)
            ));
        }
    };
    let regression = root.join("formal/traces/regression/manifest.toml");
    let code = replay(&root, &[regression, manifest]);
    if code != 0 {
        eprintln!(
            "xtask: conformance failed; traces kept in {}; repeat with '{}'",
            dir.display(),
            repeat_hint(budget)
        );
    }
    code
}

/// A seed as a directory name.
fn sanitise(seed: &str) -> String {
    seed.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// The `quint run` arguments that make `traces` traces of one relation (run from the
/// repository root, so the traces name `formal/<model>.qnt` as their source, as the committed
/// ones do). Pinned by `should_generate_seeded_mbt_traces_when_a_relation_is_run`.
fn run_args(model: &Driven, step: &str, budget: &Budget, out: &Path) -> Vec<String> {
    let mut args: Vec<String> = vec!["run".to_owned()];
    args.extend(BACKEND.iter().map(|s| (*s).to_owned()));
    args.extend(
        [
            "--main",
            model.name,
            "--init",
            model.init,
            "--step",
            step,
            "--invariant",
            "allInvariants",
            "--mbt",
        ]
        .map(str::to_owned),
    );
    args.extend([
        "--seed".to_owned(),
        budget.seed.clone(),
        "--max-steps".to_owned(),
        budget.steps.to_string(),
        "--max-samples".to_owned(),
        budget.traces.to_string(),
        "--n-traces".to_owned(),
        budget.traces.to_string(),
        "--out-itf".to_owned(),
        out.join(format!("{}-{step}_{{seq}}.itf.json", model.name))
            .display()
            .to_string(),
        format!("formal/{}.qnt", model.name),
    ]);
    args
}

/// The `npx` command running the locked Quint from the repository root.
fn quint_at_root(root: &Path) -> Command {
    let mut command = Command::new("npx");
    command
        .arg("--prefix")
        .arg(root.join("formal"))
        .args(["--no", "--", "quint"])
        .current_dir(root);
    command
}

/// One manifest entry, as `mayfly-conformance` reads it.
fn entry(file: &str, model: &str, init: &str, step: &str, steps: u32, seed: &str) -> String {
    format!(
        "[[trace]]\nfile = \"{file}\"\nmodel = \"{model}.qnt\"\nmain = \"{model}\"\ninit = \"{init}\"\nstep = \"{step}\"\nmax_steps = {steps}\nseed = \"{seed}\"\ninvariant = \"allInvariants\"\n"
    )
}

/// Generates the traces of `budget` into `dir` (emptied first) and writes `dir/manifest.toml`.
fn generate(
    root: &Path,
    dir: &Path,
    models: &[Driven],
    budget: &Budget,
) -> Result<PathBuf, String> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|e| format!("cannot clear {}: {e}", dir.display()))?;
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let jobs: Vec<(Driven, &str)> = models
        .iter()
        .flat_map(|m| m.steps.iter().map(move |s| (*m, *s)))
        .collect();
    let outputs = in_parallel(&jobs, |(model, step)| {
        let mut command = quint_at_root(root);
        command.args(run_args(model, step, budget, dir));
        output_of(command, "quint run")
    });
    let mut failed = Vec::new();
    let mut manifest = String::from(
        "# Generated by `cargo xtask conformance`; not committed (verification.md section 6.2).\n",
    );
    for ((model, step), output) in jobs.iter().zip(outputs) {
        if !output.is_some_and(|o| echo(&o)) {
            failed.push(format!("{}::{step}", model.name));
            continue;
        }
        let mut files = Vec::new();
        for seq in 0..budget.traces {
            let file = format!("{}-{step}_{seq}.itf.json", model.name);
            if dir.join(&file).exists() {
                files.push(file);
            }
        }
        if files.is_empty() {
            failed.push(format!("{}::{step} wrote no trace", model.name));
        }
        for file in files {
            manifest.push('\n');
            manifest.push_str(&entry(
                &file,
                model.name,
                model.init,
                step,
                budget.steps,
                &budget.seed,
            ));
        }
    }
    if !failed.is_empty() {
        return Err(format!(
            "quint run failed (an invariant violation or a tool error): {}",
            failed.join(", ")
        ));
    }
    let path = dir.join("manifest.toml");
    std::fs::write(&path, manifest).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// Replays `manifests` with the `mayfly-conformance` binary.
fn replay(root: &Path, manifests: &[PathBuf]) -> i32 {
    let status = Command::new("cargo")
        .args([
            "run",
            "--locked",
            "--quiet",
            "-p",
            "mayfly-conformance",
            "--",
        ])
        .args(manifests)
        .current_dir(root)
        .status();
    match status {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => fail(&format!("cannot run cargo: {err}")),
    }
}

/// One regression entry, as far as regenerating it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Regression {
    file: String,
    model: String,
    main: String,
    init: String,
    step: String,
    max_steps: i64,
    seed: String,
    invariant: String,
}

fn regressions(text: &str) -> Result<Vec<Regression>, String> {
    let table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    let traces = table
        .get("trace")
        .and_then(toml::Value::as_array)
        .ok_or("no [[trace]] entry")?;
    traces
        .iter()
        .enumerate()
        .map(|(i, raw)| {
            let text = |key: &str| {
                raw.get(key)
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| format!("trace {i} has no {key}"))
            };
            Ok(Regression {
                file: text("file")?,
                model: text("model")?,
                main: text("main")?,
                init: text("init")?,
                step: text("step")?,
                max_steps: raw
                    .get("max_steps")
                    .and_then(toml::Value::as_integer)
                    .ok_or_else(|| format!("trace {i} has no max_steps"))?,
                seed: text("seed")?,
                invariant: text("invariant")?,
            })
        })
        .collect()
}

/// The trace text with the two values Quint writes from the clock blanked: the first
/// `"description":"…"` and `"timestamp":…` (both in the leading `#meta`). Pinned by
/// `should_ignore_only_the_creation_time_when_traces_are_compared`.
fn normalise(text: &str) -> String {
    let mut out = text.to_owned();
    if let Some(start) = out.find("\"description\":\"") {
        let from = start + "\"description\":\"".len();
        if let Some(len) = out[from..].find('"') {
            out.replace_range(from..from + len, "");
        }
    }
    if let Some(start) = out.find("\"timestamp\":") {
        let from = start + "\"timestamp\":".len();
        let len = out[from..]
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(out.len() - from);
        out.replace_range(from..from + len, "");
    }
    out
}

fn freshness() -> i32 {
    let root = repo::root();
    if let Err(err) = require_quint(&formal_dir()) {
        return fail(&err);
    }
    let dir = root.join("formal/traces/regression");
    let text = match std::fs::read_to_string(dir.join("manifest.toml")) {
        Ok(text) => text,
        Err(err) => return fail(&format!("cannot read the regression manifest: {err}")),
    };
    let entries = match regressions(&text) {
        Ok(entries) => entries,
        Err(err) => return fail(&format!("regression manifest: {err}")),
    };
    let scratch = root.join("target").join("conformance").join("freshness");
    let _ = std::fs::remove_dir_all(&scratch);
    if let Err(err) = std::fs::create_dir_all(&scratch) {
        return fail(&format!("cannot create {}: {err}", scratch.display()));
    }
    let results = in_parallel(&entries, |e| {
        let out = scratch.join(&e.file);
        let mut command = quint_at_root(&root);
        command.arg("run").args(BACKEND).args([
            "--main",
            &e.main,
            "--init",
            &e.init,
            "--step",
            &e.step,
            "--max-steps",
            &e.max_steps.to_string(),
            "--mbt",
            "--seed",
            &e.seed,
            "--invariant",
            &e.invariant,
            "--out-itf",
        ]);
        command.arg(&out).arg(format!("formal/{}", e.model));
        let ran = output_of(command, "quint run").is_some_and(|o| o.status.success() || echo(&o));
        if !ran {
            return Err(format!("{}: quint run failed", e.file));
        }
        let fresh = std::fs::read_to_string(&out).map_err(|err| format!("{}: {err}", e.file))?;
        let committed = std::fs::read_to_string(dir.join(&e.file))
            .map_err(|err| format!("{}: {err}", e.file))?;
        if normalise(&fresh) == normalise(&committed) {
            Ok(())
        } else {
            Err(format!(
                "{} is stale: regenerated {} differs; re-export it (see the manifest's header)",
                e.file,
                out.display()
            ))
        }
    });
    let failed: Vec<String> = results.into_iter().filter_map(Result::err).collect();
    if failed.is_empty() {
        println!(
            "formal-freshness: {} regression traces are fresh",
            entries.len()
        );
        0
    } else {
        fail(&failed.join("\nxtask: "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_generate_seeded_mbt_traces_when_a_relation_is_run() {
        // given
        let budget = Budget {
            seed: "0x2a".to_owned(),
            traces: 3,
            steps: 7,
        };

        // when
        let args = run_args(&DRIVEN[0], "serveStep", &budget, Path::new("out")).join(" ");

        // then
        assert_eq!(
            args,
            "run --backend typescript --main lifecycle --init init --step serveStep --invariant allInvariants --mbt --seed 0x2a --max-steps 7 --max-samples 3 --n-traces 3 --out-itf out/lifecycle-serveStep_{seq}.itf.json formal/lifecycle.qnt"
        );
    }

    #[test]
    fn should_ignore_only_the_creation_time_when_traces_are_compared() {
        // given
        let a =
            r##"{"#meta":{"description":"Created by Quint on Mon","timestamp":17},"states":[1]}"##;
        let b = r##"{"#meta":{"description":"Created by Quint on Tue","timestamp":1800},"states":[1]}"##;
        let c = r##"{"#meta":{"description":"Created by Quint on Tue","timestamp":1800},"states":[2]}"##;

        // when
        let (same, different) = (normalise(a) == normalise(b), normalise(a) == normalise(c));

        // then
        assert!(same);
        assert!(!different);
    }

    #[test]
    fn should_drive_only_registered_relations_when_driven_models_are_listed() {
        // given
        let models = crate::formal::MODELS;

        // when
        let unregistered: Vec<_> = DRIVEN
            .iter()
            .flat_map(|d| d.steps.iter().map(move |s| (d.name, *s)))
            .filter(|(name, step)| {
                !models
                    .iter()
                    .any(|m| m.file == format!("{name}.qnt") && m.steps.contains(step))
            })
            .collect();

        // then
        assert!(unregistered.is_empty(), "{unregistered:?}");
    }

    #[test]
    fn should_parse_every_regression_entry_when_the_committed_manifest_is_read() {
        // given
        let text =
            std::fs::read_to_string(repo::root().join("formal/traces/regression/manifest.toml"))
                .unwrap_or_else(|e| panic!("{e}"));

        // when
        let entries = regressions(&text).unwrap_or_else(|e| panic!("{e}"));

        // then
        assert!(entries.iter().any(|e| e.model == "lifecycle.qnt"));
        assert!(entries.iter().all(|e| {
            repo::root()
                .join("formal/traces/regression")
                .join(&e.file)
                .exists()
        }));
    }
}
