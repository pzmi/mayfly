//! The fault file: `$TART_HOME/faults.toml`, read afresh by every invocation.
//!
//! ```toml
//! run_mode = "foreground"        # optional: "foreground" (default) or "detached", see `run`
//!
//! [[rule]]
//! id = "stop-hangs-once"         # optional, unique; names the rule's counters (default rule-<index>)
//! command = "stop"               # optional: "--version", create, pull, clone, run, ip, list, get,
//!                                #   stop, delete or exec; absent matches every command
//! vm = "mf-demo-1"               # optional: exact VM name the command acts on (see Command::targets)
//! arg0 = "/usr/local/bin/x"      # optional, exec only: the program the guest is asked to run
//! skip = 0                       # optional: matching invocations that pass before the rule may fire
//! times = 1                      # optional: fire at most this often; absent = sticky
//! delay_ms = 250                 # optional: sleep this long before anything else happens
//! action = "hang"                # none | hang | hang_after_effect | fail_before_effect |
//!                                #   fail_after_effect | crash_after_effect | corrupt_output
//! exit_code = 1                  # fail_*: exit code (default 1)
//! stderr = "boom"                # fail_*: stderr text (default "fake-tart: injected failure")
//! corrupt = "truncate"           # corrupt_output: truncate (default) | garbage | empty
//! version = "9.9.9"              # command "--version" only: the version printed
//! job_ms = 5000                  # exec only: how long the guest "runs a job" after reading stdin
//! job_exit_code = 0              # exec only: the guest command's exit code
//! job_stdout = "done"            # exec only: what the guest command prints on stdout at the end
//! job_stderr = "started"         # exec only: what it prints on stderr when the job starts
//! ```
//!
//! Every rule that matches an invocation counts it (`matched`); the first matching rule that is
//! past its `skip` and under its `times` fires (`fired`), the others only count. Counters live in
//! `state.json` keyed by rule id, so a lab that rewrites the file keeps them for unchanged ids and
//! resets them by choosing a new id. An unknown key or an invalid rule makes the invocation exit
//! 99 rather than run without the fault the lab meant to inject.
//!
//! A `hang` fault writes `fake-tart: hang fault '<id>' is active` to stderr once its `fault` record
//! is in the invocation log, so a test can wait for "the hang has landed" by reading one line
//! instead of polling or sleeping.
//!
//! Pinned by: `tests/cli/faults.rs` (one test per action), `faults::tests`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::cli::Command;
use crate::fail::Fail;

/// How `tart run` represents a running VM.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunMode {
    /// `run` stays alive as the VM's process and returns when the VM stops (real Tart's shape).
    #[default]
    Foreground,
    /// `run` marks the VM running in the state dir and exits at once; nothing is left to kill.
    Detached,
}

/// What a firing rule does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    /// Only the modifiers (delay, version, job) apply.
    #[default]
    None,
    /// Block before any effect until killed.
    Hang,
    /// Apply the effect, then block until killed.
    HangAfterEffect,
    /// Exit non-zero before any effect.
    FailBeforeEffect,
    /// Apply the effect, then exit non-zero.
    FailAfterEffect,
    /// Apply the effect, then die by SIGABRT.
    CrashAfterEffect,
    /// Apply the effect, then damage stdout.
    CorruptOutput,
}

impl Action {
    /// The spelling used in `faults.toml` and in the invocation log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Hang => "hang",
            Self::HangAfterEffect => "hang_after_effect",
            Self::FailBeforeEffect => "fail_before_effect",
            Self::FailAfterEffect => "fail_after_effect",
            Self::CrashAfterEffect => "crash_after_effect",
            Self::CorruptOutput => "corrupt_output",
        }
    }
}

/// How `corrupt_output` damages stdout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Corrupt {
    /// Keep the first half of the bytes.
    #[default]
    Truncate,
    /// Replace the output with bytes no parser accepts.
    Garbage,
    /// Print nothing.
    Empty,
}

/// One `[[rule]]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rule {
    pub id: Option<String>,
    pub command: Option<String>,
    pub vm: Option<String>,
    pub arg0: Option<String>,
    #[serde(default)]
    pub skip: u64,
    pub times: Option<u64>,
    #[serde(default)]
    pub delay_ms: u64,
    #[serde(default)]
    pub action: Action,
    pub exit_code: Option<i32>,
    pub stderr: Option<String>,
    #[serde(default)]
    pub corrupt: Corrupt,
    pub version: Option<String>,
    #[serde(default)]
    pub job_ms: u64,
    #[serde(default)]
    pub job_exit_code: i32,
    pub job_stdout: Option<String>,
    pub job_stderr: Option<String>,
}

/// The parsed `faults.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FaultFile {
    #[serde(default)]
    pub run_mode: RunMode,
    #[serde(default, rename = "rule")]
    pub rules: Vec<Rule>,
}

/// How often a rule matched and fired, persisted in `state.json`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Counters {
    pub matched: u64,
    pub fired: u64,
}

const COMMANDS: &[&str] = &[
    "--version",
    "create",
    "pull",
    "clone",
    "run",
    "ip",
    "list",
    "get",
    "stop",
    "delete",
    "exec",
];

impl Rule {
    /// The rule's id, defaulting to its position.
    pub fn id_at(&self, index: usize) -> String {
        self.id.clone().unwrap_or_else(|| format!("rule-{index}"))
    }

    fn matches(&self, command: &Command) -> bool {
        self.command.as_deref().is_none_or(|c| c == command.name())
            && self
                .vm
                .as_deref()
                .is_none_or(|v| command.targets().contains(&v))
            && self
                .arg0
                .as_deref()
                .is_none_or(|a| command.arg0() == Some(a))
    }
}

impl FaultFile {
    /// Parses and validates a fault file.
    pub fn parse(text: &str) -> Result<Self, Fail> {
        let file: Self = toml::from_str(text)
            .map_err(|e| Fail::fake(&format!("faults.toml is invalid: {}", e.message())))?;
        let mut seen = Vec::new();
        for (index, rule) in file.rules.iter().enumerate() {
            let id = rule.id_at(index);
            let bad = |why: &str| Fail::fake(&format!("faults.toml rule '{id}': {why}"));
            if seen.contains(&id) {
                return Err(bad("duplicate id"));
            }
            if let Some(c) = &rule.command
                && !COMMANDS.contains(&c.as_str())
            {
                return Err(bad("unknown command"));
            }
            let command = rule.command.as_deref();
            if (rule.arg0.is_some()
                || rule.job_ms > 0
                || rule.job_stdout.is_some()
                || rule.job_exit_code != 0)
                && command != Some("exec")
            {
                return Err(bad("arg0 and job_* need command = \"exec\""));
            }
            if rule.version.is_some() && command != Some("--version") {
                return Err(bad("version needs command = \"--version\""));
            }
            if rule.times == Some(0) {
                return Err(bad("times = 0 never fires"));
            }
            seen.push(id);
        }
        Ok(file)
    }

    /// Counts `command` against every rule and returns the index of the one that fires, if any.
    pub fn select(
        &self,
        counters: &mut BTreeMap<String, Counters>,
        command: &Command,
    ) -> Option<usize> {
        let mut firing = None;
        for (index, rule) in self.rules.iter().enumerate() {
            if !rule.matches(command) {
                continue;
            }
            let entry = counters.entry(rule.id_at(index)).or_default();
            entry.matched += 1;
            let eligible = entry.matched > rule.skip && rule.times.is_none_or(|t| entry.fired < t);
            if eligible && firing.is_none() {
                entry.fired += 1;
                firing = Some(index);
            }
        }
        firing
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, FaultFile};
    use crate::cli::parse_command;
    use std::collections::BTreeMap;

    fn command(text: &str) -> crate::cli::Command {
        let args: Vec<String> = text.split(' ').map(str::to_owned).collect();
        parse_command(&args).unwrap()
    }

    #[test]
    fn should_fire_only_once_when_the_rule_is_one_shot() {
        // given
        let file = FaultFile::parse("[[rule]]\ncommand = \"stop\"\ntimes = 1\naction = \"hang\"\n")
            .unwrap();
        let mut counters = BTreeMap::new();
        let stop = command("stop vm1");

        // when
        let first = file.select(&mut counters, &stop);
        let second = file.select(&mut counters, &stop);

        // then
        assert_eq!((first, second), (Some(0), None));
        assert_eq!(file.rules[0].action, Action::Hang);
        assert_eq!(counters["rule-0"].matched, 2);
        assert_eq!(counters["rule-0"].fired, 1);
    }

    #[test]
    fn should_fire_every_time_when_the_rule_is_sticky() {
        // given
        let file = FaultFile::parse("[[rule]]\naction = \"fail_before_effect\"\n").unwrap();
        let mut counters = BTreeMap::new();

        // when
        let hits: Vec<_> = (0..3)
            .map(|_| file.select(&mut counters, &command("list")))
            .collect();

        // then
        assert_eq!(hits, vec![Some(0), Some(0), Some(0)]);
    }

    #[test]
    fn should_let_the_first_invocations_pass_when_skip_is_set() {
        // given
        let file = FaultFile::parse("[[rule]]\ncommand = \"ip\"\nskip = 2\ntimes = 1\n").unwrap();
        let mut counters = BTreeMap::new();

        // when
        let hits: Vec<_> = (0..4)
            .map(|_| file.select(&mut counters, &command("ip vm1")))
            .collect();

        // then
        assert_eq!(hits, vec![None, None, Some(0), None]);
    }

    #[test]
    fn should_match_only_the_named_vm_and_program_when_the_rule_narrows() {
        // given
        let file = FaultFile::parse(
            "[[rule]]\ncommand = \"exec\"\nvm = \"a\"\narg0 = \"launch\"\njob_ms = 5\n",
        )
        .unwrap();
        let mut counters = BTreeMap::new();

        // when
        let wrong_vm = file.select(&mut counters, &command("exec b launch"));
        let wrong_program = file.select(&mut counters, &command("exec a true"));
        let right = file.select(&mut counters, &command("exec a launch x"));

        // then
        assert_eq!((wrong_vm, wrong_program, right), (None, None, Some(0)));
    }

    #[test]
    fn should_count_a_shadowed_rule_without_firing_it_when_an_earlier_rule_fires() {
        // given
        let file = FaultFile::parse(
            "[[rule]]\nid = \"a\"\ncommand = \"list\"\n[[rule]]\nid = \"b\"\ncommand = \"list\"\n",
        )
        .unwrap();
        let mut counters = BTreeMap::new();

        // when
        let hit = file.select(&mut counters, &command("list"));

        // then
        assert_eq!(hit, Some(0));
        assert_eq!(
            (
                counters["a"].fired,
                counters["b"].fired,
                counters["b"].matched
            ),
            (1, 0, 1)
        );
    }

    #[test]
    fn should_refuse_a_fault_file_when_it_is_malformed() {
        // given
        let bad = [
            "unknown = 1",
            "[[rule]]\ntypo = 1",
            "[[rule]]\naction = \"explode\"",
            "[[rule]]\ncommand = \"reboot\"",
            "[[rule]]\nid = \"x\"\n[[rule]]\nid = \"x\"",
            "[[rule]]\ncommand = \"stop\"\njob_ms = 1",
            "[[rule]]\narg0 = \"x\"",
            "[[rule]]\ncommand = \"stop\"\nversion = \"1\"",
            "[[rule]]\ntimes = 0",
        ];

        for text in bad {
            // when
            let failure = FaultFile::parse(text).expect_err(text);

            // then
            assert_eq!(failure.code, 99, "{text}");
        }
    }

    #[test]
    fn should_parse_an_empty_file_when_no_rules_are_given() {
        // given
        let text = "";

        // when
        let file = FaultFile::parse(text).unwrap();

        // then
        assert!(file.rules.is_empty());
    }
}
