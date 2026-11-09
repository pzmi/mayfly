//! Running external commands as gate stages.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// One gate stage: an external command, or a function of this binary.
#[derive(Debug, Clone)]
pub struct Stage {
    /// Short name printed in the progress line; equals its catalog id.
    pub name: &'static str,
    /// What runs.
    pub action: Action,
    /// Pinned tool that must be installed for the stage to run (checked by [`run_report`]).
    pub tool: Option<&'static str>,
    /// A second pinned tool the stage needs (actionlint runs shellcheck over `run:` scripts and
    /// skips them silently without it), checked the same way.
    pub also_tool: Option<&'static str>,
}

/// How a stage runs.
#[derive(Debug, Clone)]
pub enum Action {
    /// A program and its arguments.
    Command {
        /// Program to execute.
        program: String,
        /// Arguments passed to the program.
        args: Vec<String>,
        /// Environment variables set for the child only.
        env: Vec<(String, String)>,
        /// Stderr substrings that fail the stage even when the program exits 0.
        deny_stderr: Vec<String>,
    },
    /// An xtask function returning an exit code, shown as `display` in the progress line.
    InProcess {
        /// The equivalent command line, for the progress line.
        display: &'static str,
        /// The function.
        run: fn() -> i32,
    },
}

impl Stage {
    /// Builds an external-command stage from string slices.
    pub fn new(name: &'static str, program: &str, args: &[&str]) -> Self {
        Self {
            name,
            action: Action::Command {
                program: program.to_owned(),
                args: args.iter().map(|a| (*a).to_owned()).collect(),
                env: Vec::new(),
                deny_stderr: Vec::new(),
            },
            tool: None,
            also_tool: None,
        }
    }

    /// Sets an environment variable for this stage's child process.
    #[must_use]
    pub fn env(mut self, key: &str, value: &str) -> Self {
        if let Action::Command { env, .. } = &mut self.action {
            env.push((key.to_owned(), value.to_owned()));
        }
        self
    }

    /// Fails the stage when its stderr contains `needle`, even if the program exits 0. Used for
    /// tool warnings that must not pass silently (a warning is a failure at the gate): nextest
    /// ignores a configuration key it does not know with a warning only.
    #[must_use]
    pub fn deny_stderr(mut self, needle: &str) -> Self {
        if let Action::Command { deny_stderr, .. } = &mut self.action {
            deny_stderr.push(needle.to_owned());
        }
        self
    }

    /// Declares the pinned tool this stage needs; without it the stage reports `UNAVAILABLE`.
    #[must_use]
    pub fn needs(mut self, tool: &'static str) -> Self {
        self.tool = Some(tool);
        self
    }

    /// Declares a second pinned tool this stage needs, with the same `UNAVAILABLE` outcome.
    #[must_use]
    pub fn and_needs(mut self, tool: &'static str) -> Self {
        self.also_tool = Some(tool);
        self
    }

    /// Builds a stage that calls `run` in this process.
    pub fn in_process(name: &'static str, display: &'static str, run: fn() -> i32) -> Self {
        Self {
            name,
            action: Action::InProcess { display, run },
            tool: None,
            also_tool: None,
        }
    }

    fn execute(&self) -> i32 {
        match &self.action {
            Action::Command {
                program,
                args,
                env,
                deny_stderr,
            } => {
                eprintln!("xtask: stage {}: {program} {}", self.name, args.join(" "));
                let mut command = Command::new(program);
                command.args(args).envs(env.iter().map(|(k, v)| (k, v)));
                if deny_stderr.is_empty() {
                    return match command.status() {
                        Ok(status) => status.code().unwrap_or(1),
                        Err(err) => {
                            eprintln!("xtask: cannot run {program}: {err}");
                            1
                        }
                    };
                }
                command.stderr(Stdio::piped());
                let mut child = match command.spawn() {
                    Ok(child) => child,
                    Err(err) => {
                        eprintln!("xtask: cannot run {program}: {err}");
                        return 1;
                    }
                };
                let denied = child
                    .stderr
                    .take()
                    .and_then(|stderr| scan_stderr(BufReader::new(stderr), deny_stderr));
                let code = match child.wait() {
                    Ok(status) => status.code().unwrap_or(1),
                    Err(_) => 1,
                };
                match denied {
                    Some(needle) if code == 0 => {
                        eprintln!(
                            "xtask: stage {} FAILED: stderr contains {needle:?}",
                            self.name
                        );
                        1
                    }
                    _ => code,
                }
            }
            Action::InProcess { display, run } => {
                eprintln!("xtask: stage {}: {display}", self.name);
                run()
            }
        }
    }
}

/// Echoes `stderr` line by line and returns the first denied needle any line contains.
///
/// Reads to the end of the stream whatever it holds: a line that is not UTF-8 is decoded
/// lossily rather than ending the scan, so a denied warning after it is still seen and the
/// child never blocks on a full pipe nobody drains. A read error ends the scan (the child's
/// exit code still decides). Pinned by
/// `should_fail_when_denied_text_follows_invalid_utf8_on_stderr`.
fn scan_stderr(mut stderr: impl BufRead, deny: &[String]) -> Option<String> {
    let mut denied = None;
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match stderr.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => return denied,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&raw);
        let line = line.trim_end_matches(['\n', '\r']);
        eprintln!("{line}");
        if denied.is_none() {
            denied = deny.iter().find(|n| line.contains(n.as_str())).cloned();
        }
    }
}

/// How one stage ended in a [`run_report`]. There is no "skipped" (D15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Exit code 0.
    Passed,
    /// Non-zero exit code.
    Failed(i32),
    /// A required tool is missing; carries the actionable message.
    Unavailable(String),
}

impl Status {
    /// The word printed in the summary table.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Passed => "ok",
            Self::Failed(_) => "FAILED",
            Self::Unavailable(_) => "UNAVAILABLE",
        }
    }
}

/// One row of the summary table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Stage name.
    pub name: &'static str,
    /// How it ended.
    pub status: Status,
    /// Wall-clock time (zero for an unavailable stage).
    pub duration: Duration,
}

/// Runs every stage (a failure does not stop later ones, so one run shows all problems) and
/// returns one outcome each. `require` decides whether a stage's tool is usable.
pub fn run_report(stages: &[Stage], require: &dyn Fn(&str) -> Result<(), String>) -> Vec<Outcome> {
    stages
        .iter()
        .map(|stage| {
            let start = Instant::now();
            let missing = stage
                .tool
                .into_iter()
                .chain(stage.also_tool)
                .find_map(|tool| require(tool).err());
            let status = match missing {
                Some(message) => {
                    eprintln!("xtask: stage {} UNAVAILABLE: {message}", stage.name);
                    Status::Unavailable(message)
                }
                None => match stage.execute() {
                    0 => Status::Passed,
                    code => {
                        eprintln!("xtask: stage {} FAILED with exit code {code}", stage.name);
                        Status::Failed(code)
                    }
                },
            };
            Outcome {
                name: stage.name,
                status,
                duration: start.elapsed(),
            }
        })
        .collect()
}

/// Renders the final table: stage, status, duration.
pub fn render_table(outcomes: &[Outcome]) -> String {
    let width = outcomes
        .iter()
        .map(|o| o.name.len())
        .max()
        .unwrap_or(5)
        .max(5);
    let mut out = format!("{:<width$}  {:<11}  {}\n", "stage", "status", "duration");
    for o in outcomes {
        let _ = writeln!(
            out,
            "{:<width$}  {:<11}  {:.1}s",
            o.name,
            o.status.label(),
            o.duration.as_secs_f64()
        );
    }
    out
}

/// Prints the table and returns 0 only when every stage passed, else 1.
pub fn finish(outcomes: &[Outcome]) -> i32 {
    print!("{}", render_table(outcomes));
    i32::from(outcomes.iter().any(|o| o.status != Status::Passed))
}

/// Runs stages in order, streaming their output, and stops at the first failure.
///
/// Returns 0 when all pass, otherwise the failing stage's exit code (1 when the process was
/// killed by a signal or could not be spawned).
pub fn run_stages(stages: &[Stage]) -> i32 {
    for stage in stages {
        let code = stage.execute();
        if code != 0 {
            eprintln!("xtask: stage {} FAILED with exit code {code}", stage.name);
            return code;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(name: &'static str, script: &str) -> Stage {
        Stage::new(name, "sh", &["-c", script])
    }

    fn present(_: &str) -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn should_report_failed_and_exit_nonzero_when_a_stage_fails() {
        // given
        let stages = [
            sh("ok", "exit 0"),
            sh("bad", "exit 7"),
            sh("after", "exit 0"),
        ];

        // when
        let outcomes = run_report(&stages, &present);
        let table = render_table(&outcomes);
        let code = finish(&outcomes);

        // then
        assert_eq!(outcomes[1].status, Status::Failed(7));
        assert_eq!(outcomes[2].status, Status::Passed, "later stages still run");
        assert!(table.contains("bad") && table.contains("FAILED"), "{table}");
        assert_eq!(code, 1);
    }

    #[test]
    fn should_report_unavailable_and_exit_nonzero_when_a_tool_is_missing() {
        // given
        let stages = [sh("needs-tool", "exit 0").needs("cargo-nope")];
        let missing = |tool: &str| Err(format!("missing {tool}: install it"));

        // when
        let outcomes = run_report(&stages, &missing);
        let table = render_table(&outcomes);
        let code = finish(&outcomes);

        // then
        assert!(matches!(outcomes[0].status, Status::Unavailable(_)));
        assert!(table.contains("UNAVAILABLE"), "{table}");
        assert_eq!(code, 1);
    }

    #[test]
    fn should_report_unavailable_when_the_second_tool_is_missing() {
        // given
        let stages = [sh("needs-two", "exit 0").needs("a").and_needs("b")];
        let only_a = |tool: &str| {
            if tool == "a" {
                Ok(())
            } else {
                Err(format!("missing {tool}: install it"))
            }
        };

        // when
        let outcomes = run_report(&stages, &only_a);

        // then
        assert_eq!(
            outcomes[0].status,
            Status::Unavailable("missing b: install it".into())
        );
        assert_eq!(finish(&outcomes), 1);
    }

    #[test]
    fn should_run_when_both_tools_are_present() {
        // given
        let stages = [sh("needs-two", "exit 0").needs("a").and_needs("b")];

        // when
        let outcomes = run_report(&stages, &present);

        // then
        assert_eq!(outcomes[0].status, Status::Passed);
    }

    #[test]
    fn should_exit_zero_when_every_stage_passes() {
        // given
        let stages = [sh("a", "exit 0"), sh("b", "exit 0").needs("tool")];

        // when
        let outcomes = run_report(&stages, &present);

        // then
        assert_eq!(finish(&outcomes), 0);
        assert!(outcomes.iter().all(|o| o.status.label() == "ok"));
    }

    #[test]
    fn should_return_failing_exit_code_when_a_stage_fails() {
        // given
        let stages = [sh("ok", "exit 0"), sh("bad", "exit 7")];

        // when
        let code = run_stages(&stages);

        // then
        assert_eq!(code, 7);
    }

    #[test]
    fn should_skip_later_stages_when_an_earlier_stage_fails() {
        // given
        let dir = tempfile::TempDir::new().unwrap();
        let marker = dir.path().join("ran");
        let marker_arg = marker.to_str().unwrap();
        let stages = [
            sh("bad", "exit 4"),
            Stage::new("later", "sh", &["-c", "touch \"$1\"", "sh", marker_arg]),
        ];

        // when
        let code = run_stages(&stages);

        // then
        assert_eq!(code, 4);
        assert!(!marker.exists(), "later stage must not run");
    }

    #[test]
    fn should_return_zero_when_all_stages_pass() {
        // given
        let stages = [sh("a", "exit 0"), sh("b", "true")];

        // when
        let code = run_stages(&stages);

        // then
        assert_eq!(code, 0);
    }

    #[test]
    fn should_return_function_exit_code_when_in_process_stage_fails() {
        // given
        let stages = [
            sh("ok", "exit 0"),
            Stage::in_process("inner", "inner", || 5),
        ];

        // when
        let code = run_stages(&stages);

        // then
        assert_eq!(code, 5);
    }

    #[test]
    fn should_fail_when_program_does_not_exist() {
        // given
        let stages = [Stage::new("missing", "xtask-no-such-program", &[])];

        // when
        let code = run_stages(&stages);

        // then
        assert_eq!(code, 1);
    }

    #[test]
    fn should_pass_env_to_child_when_stage_sets_env() {
        // given
        let stages = [sh("env", "test \"$XTASK_PROBE\" = 1024").env("XTASK_PROBE", "1024")];

        // when
        let code = run_stages(&stages);

        // then
        assert_eq!(code, 0);
    }

    #[test]
    fn should_fail_when_denied_text_is_on_stderr_despite_exit_zero() {
        // given
        let stages = [
            sh("warn", "echo 'ignoring unknown configuration key: x' >&2")
                .deny_stderr("ignoring unknown configuration key"),
        ];

        // when
        let code = run_stages(&stages);

        // then
        assert_eq!(code, 1);
    }

    #[test]
    fn should_fail_when_denied_text_follows_invalid_utf8_on_stderr() {
        // given: a line that is not UTF-8, then the denied warning
        let stages = [sh(
            "warn",
            "printf 'bad \\377\\376 bytes\\n' >&2; echo 'ignoring unknown configuration key: x' >&2",
        )
        .deny_stderr("ignoring unknown configuration key")];

        // when
        let code = run_stages(&stages);

        // then
        assert_eq!(code, 1);
    }

    #[test]
    fn should_keep_exit_code_when_no_denied_text_is_on_stderr() {
        // given
        let stages = [sh("quiet", "echo fine >&2; exit 3").deny_stderr("ignoring unknown")];

        // when
        let code = run_stages(&stages);

        // then
        assert_eq!(code, 3);
    }
}
