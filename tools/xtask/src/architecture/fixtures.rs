//! Policy self-tests (foundation.md section 3.4). A negative fixture is a mini workspace with
//! an `expected.txt` that names the rule it must violate; a positive fixture is a mini
//! workspace that must pass with no diagnostic at all.
//!
//! `expected.txt` holds `key: value` lines: `rule:` (required, a rule id), `at:` (required, a
//! substring of the diagnostic's path), any number of `contains:` (substrings of the
//! message) and an optional `only: true`. One diagnostic must satisfy all of them; other
//! diagnostics may also appear unless `only` is set, which requires every diagnostic to be
//! of the expected rule.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use super::check;
use super::report::Diagnostic;

/// What one fixture must produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    /// Rule id, for example `ARCH-unknown-crate`.
    pub rule: String,
    /// Substring of the diagnostic's path.
    pub at: String,
    /// Substrings of the diagnostic's message.
    pub contains: Vec<String>,
    /// Every diagnostic must be of `rule`.
    pub only: bool,
}

impl Expected {
    /// Parses `expected.txt`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (mut rule, mut at, mut contains, mut only) = (None, None, Vec::new(), false);
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| format!("line without `key: value`: {line}"))?;
            let value = value.trim().to_owned();
            match key.trim() {
                "rule" => rule = Some(value),
                "at" => at = Some(value),
                "contains" => contains.push(value),
                "only" => {
                    only = match value.as_str() {
                        "true" => true,
                        "false" => false,
                        other => return Err(format!("`only:` takes true or false, not `{other}`")),
                    }
                }
                other => return Err(format!("unknown key `{other}`")),
            }
        }
        Ok(Self {
            rule: rule.ok_or("missing `rule:`")?,
            at: at.ok_or("missing `at:`")?,
            contains,
            only,
        })
    }

    /// Whether `diagnostic` satisfies every expectation.
    pub fn matches(&self, diagnostic: &Diagnostic) -> bool {
        diagnostic.rule.id() == self.rule
            && diagnostic.at.contains(&self.at)
            && self.contains.iter().all(|c| diagnostic.message.contains(c))
    }
}

/// Checks every negative fixture under `dir`; returns how many passed, or one error per
/// failure, in case order.
pub fn verify(dir: &Path) -> Result<usize, Vec<String>> {
    verify_each(dir, verify_case)
}

/// Checks every positive fixture under `dir`: each must produce no diagnostic.
pub fn verify_valid(dir: &Path) -> Result<usize, Vec<String>> {
    verify_each(dir, |case| {
        let got: Vec<String> = check(&case.join("Cargo.toml"))
            .diagnostics
            .iter()
            .map(ToString::to_string)
            .collect();
        if got.is_empty() {
            Ok(())
        } else {
            Err(format!("expected no diagnostics, got: {}", got.join("; ")))
        }
    })
}

/// Runs `verify_case` on every fixture under `dir` on a few threads.
fn verify_each(
    dir: &Path,
    verify_case: impl Fn(&Path) -> Result<(), String> + Sync,
) -> Result<usize, Vec<String>> {
    let cases = list_cases(dir).map_err(|e| vec![e])?;
    let next = AtomicUsize::new(0);
    let workers = thread::available_parallelism().map_or(2, |n| n.get().min(8));
    let mut results: Vec<(usize, Result<(), String>)> = thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(case) = cases.get(i) else { break };
                        done.push((i, verify_case(&dir.join(case))));
                    }
                    done
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    results.sort_by_key(|(i, _)| *i);
    if results.len() != cases.len() {
        return Err(vec!["a fixture worker panicked".to_owned()]);
    }
    let errors: Vec<String> = results
        .into_iter()
        .filter_map(|(i, r)| r.err().map(|e| format!("fixture `{}`: {e}", cases[i])))
        .collect();
    if errors.is_empty() {
        Ok(cases.len())
    } else {
        Err(errors)
    }
}

fn list_cases(dir: &Path) -> Result<Vec<String>, String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("cannot list fixtures: {e}"))?;
    let mut cases: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .collect();
    cases.sort();
    if cases.is_empty() {
        return Err("no fixtures found".to_owned());
    }
    Ok(cases)
}

fn verify_case(case: &Path) -> Result<(), String> {
    let text = fs::read_to_string(case.join("expected.txt"))
        .map_err(|e| format!("cannot read expected.txt: {e}"))?;
    let expected = Expected::parse(&text).map_err(|e| format!("expected.txt: {e}"))?;
    let outcome = check(&case.join("Cargo.toml"));
    let extra = expected.only
        && outcome
            .diagnostics
            .iter()
            .any(|d| d.rule.id() != expected.rule);
    if !extra && outcome.diagnostics.iter().any(|d| expected.matches(d)) {
        return Ok(());
    }
    let got: Vec<String> = outcome
        .diagnostics
        .iter()
        .map(ToString::to_string)
        .collect();
    Err(format!(
        "expected `{}`{} at `{}`, got: {}",
        expected.rule,
        if expected.only { " only" } else { "" },
        expected.at,
        if got.is_empty() {
            "no diagnostics".to_owned()
        } else {
            got.join("; ")
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architecture::report::Rule;

    #[test]
    fn should_match_diagnostic_when_rule_path_and_message_agree() {
        // given
        let expected =
            Expected::parse("rule: ARCH-unknown-crate\nat: crates/x\ncontains: `x`\n").unwrap();
        let diagnostic = Diagnostic::new(Rule::UnknownCrate, "crate `x`", "crates/x/Cargo.toml");

        // when
        let matched = expected.matches(&diagnostic);

        // then
        assert!(matched);
    }

    #[test]
    fn should_not_match_diagnostic_when_rule_differs() {
        // given
        let expected = Expected::parse("rule: ARCH-missing-crate\nat: crates/x\n").unwrap();
        let diagnostic = Diagnostic::new(Rule::UnknownCrate, "crate `x`", "crates/x/Cargo.toml");

        // when
        let matched = expected.matches(&diagnostic);

        // then
        assert!(!matched);
    }

    #[test]
    fn should_require_single_rule_when_only_is_set() {
        // given
        let expected =
            Expected::parse("rule: ARCH-unknown-crate\nat: crates/x\nonly: true\n").unwrap();

        // when
        let parsed_only = expected.only;
        let bad = Expected::parse("rule: ARCH-x\nat: y\nonly: maybe\n");

        // then
        assert!(parsed_only);
        assert!(bad.unwrap_err().contains("true or false"));
    }

    #[test]
    fn should_reject_expectation_when_key_is_unknown() {
        // given
        let text = "rule: ARCH-x\nat: y\npath: z\n";

        // when
        let err = Expected::parse(text).unwrap_err();

        // then
        assert!(err.contains("unknown key `path`"), "{err}");
    }

    #[test]
    fn should_fail_closed_when_fixture_directory_is_empty() {
        // given
        let dir = tempfile::TempDir::new().unwrap();

        // when
        let result = verify(dir.path());

        // then
        assert_eq!(result, Err(vec!["no fixtures found".to_owned()]));
    }
}
