//! The report `hw01` writes: versioned JSON, one entry per check (`pass`, `fail` with bounded
//! evidence, or `not_run` with its reason), the host's macOS, Tart and toolchain versions, the
//! image digests, and what cleanup could not delete. Everything passes [`Redactor::value`]
//! before it is written; evidence that fails it fails its check and is dropped. [`validate`] is
//! the schema, checked on every report before it is written and by
//! `should_match_the_schema_when_a_report_holds_every_status`.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::redact::Redactor;

/// The report's `schema` value.
pub const SCHEMA: &str = "mayfly-hw01-report";

/// The report's `version`.
pub const VERSION: u64 = 1;

/// The most evidence items one check keeps.
const MAX_EVIDENCE: usize = 40;

/// The most characters one evidence string keeps.
const MAX_TEXT: usize = 1000;

/// A check's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// It held.
    Pass,
    /// It did not; the evidence says how.
    Fail,
    /// It could not run; the reason says why.
    NotRun(String),
}

/// One check's entry.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckResult {
    /// Stable id (`tart-version`, `IC-01`, …).
    pub id: String,
    /// The task whose assumption it confirms (`TART-01`, `DP-07`, …).
    pub task: &'static str,
    /// The verdict.
    pub status: Status,
    /// Bounded evidence: exit codes, JSON shapes, the wording that matters.
    pub evidence: Vec<Value>,
}

impl CheckResult {
    /// A passing check.
    pub fn pass(id: &str, task: &'static str, evidence: Vec<Value>) -> Self {
        Self {
            id: id.to_owned(),
            task,
            status: Status::Pass,
            evidence,
        }
    }

    /// A failed check.
    pub fn fail(id: &str, task: &'static str, evidence: Vec<Value>) -> Self {
        Self {
            id: id.to_owned(),
            task,
            status: Status::Fail,
            evidence,
        }
    }

    /// A check that could not run.
    pub fn not_run(id: &str, task: &'static str, reason: &str) -> Self {
        Self {
            id: id.to_owned(),
            task,
            status: Status::NotRun(reason.to_owned()),
            evidence: Vec::new(),
        }
    }

    /// Pass when `held`, fail otherwise, with `evidence` either way.
    pub fn verdict(id: &str, task: &'static str, held: bool, evidence: Vec<Value>) -> Self {
        if held {
            Self::pass(id, task, evidence)
        } else {
            Self::fail(id, task, evidence)
        }
    }
}

/// Everything a run reports.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// `macos`, `tart`, `rustc`, `cargo`, `mayfly` (the checkout's commit).
    pub versions: BTreeMap<String, String>,
    /// `--macos`, `--service-mode`.
    pub options: BTreeMap<String, bool>,
    /// (name, repository, digest) of each pinned image.
    pub images: Vec<(String, String, String)>,
    /// Each check, in run order.
    pub checks: Vec<CheckResult>,
    /// What cleanup could not delete.
    pub leftovers: Vec<String>,
    /// Ctrl-C or a termination signal ended the checks early.
    pub interrupted: bool,
}

fn bounded(value: &Value) -> Value {
    match value {
        Value::String(s) if s.chars().count() > MAX_TEXT => {
            Value::String(s.chars().take(MAX_TEXT).chain("…".chars()).collect())
        }
        Value::Array(items) => Value::Array(items.iter().take(MAX_EVIDENCE).map(bounded).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .take(MAX_EVIDENCE)
                .map(|(k, v)| (k.clone(), bounded(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `value` with each scalar replaced by its type's name: the shape of a JSON answer, never its
/// values.
pub fn shape(value: &Value) -> Value {
    match value {
        Value::Null => json!("null"),
        Value::Bool(_) => json!("bool"),
        Value::Number(n) if n.is_i64() || n.is_u64() => json!("integer"),
        Value::Number(_) => json!("number"),
        Value::String(_) => json!("string"),
        Value::Array(items) => Value::Array(items.iter().take(2).map(shape).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), shape(v))).collect())
        }
    }
}

impl Report {
    /// The report as JSON, every string redacted; a check whose evidence leaks fails with its
    /// evidence dropped.
    pub fn to_json(&self, redactor: &Redactor) -> Value {
        let checks: Vec<Value> = self
            .checks
            .iter()
            .map(|check| {
                let id = redactor.text(&check.id);
                let evidence: Vec<Value> = check
                    .evidence
                    .iter()
                    .take(MAX_EVIDENCE)
                    .map(bounded)
                    .collect();
                let redacted = redactor.value(&Value::Array(evidence));
                let (status, evidence, reason) = match (&check.status, redacted) {
                    (_, Err(leak)) => (
                        "fail",
                        Vec::new(),
                        Some(format!("evidence dropped: it failed redaction ({leak})")),
                    ),
                    (Status::Pass, Ok(Value::Array(e))) => ("pass", e, None),
                    (Status::Fail, Ok(Value::Array(e))) => ("fail", e, None),
                    (Status::NotRun(why), Ok(_)) => ("not_run", Vec::new(), Some(why.clone())),
                    (_, Ok(_)) => (
                        "fail",
                        Vec::new(),
                        Some("evidence was not a list".to_owned()),
                    ),
                };
                let reason = reason.map(|r| {
                    let text = redactor.text(&r);
                    if redactor.leak(&text).is_some() {
                        "the reason failed redaction".to_owned()
                    } else {
                        text
                    }
                });
                let mut entry = json!({
                    "id": id,
                    "task": check.task,
                    "status": status,
                    "evidence": evidence,
                });
                if let Some(reason) = reason {
                    entry["reason"] = json!(reason);
                }
                entry
            })
            .collect();
        let scrub = |text: &str| {
            let text = redactor.text(text);
            if redactor.leak(&text).is_some() {
                "<redacted>".to_owned()
            } else {
                text
            }
        };
        json!({
            "schema": SCHEMA,
            "version": VERSION,
            "versions": self.versions.iter().map(|(k, v)| (k.clone(), json!(scrub(v)))).collect::<serde_json::Map<_, _>>(),
            "options": self.options,
            "images": self.images.iter().map(|(name, repository, digest)| json!({
                "name": name, "repository": repository, "digest": digest,
            })).collect::<Vec<_>>(),
            "interrupted": self.interrupted,
            "checks": checks,
            "leftovers": self.leftovers.iter().map(|l| scrub(l)).collect::<Vec<_>>(),
        })
    }
}

fn require<'a>(object: &'a Value, key: &str, kind: &str) -> Result<&'a Value, String> {
    let value = object.get(key).ok_or_else(|| format!("missing `{key}`"))?;
    let ok = match kind {
        "string" => value.is_string(),
        "integer" => value.is_u64(),
        "bool" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    };
    if ok {
        Ok(value)
    } else {
        Err(format!("`{key}` is not {kind}"))
    }
}

/// The report schema, version 1.
///
/// # Errors
/// The first field that does not match.
pub fn validate(report: &Value) -> Result<(), String> {
    let keys = [
        "schema",
        "version",
        "versions",
        "options",
        "images",
        "interrupted",
        "checks",
        "leftovers",
    ];
    let object = report.as_object().ok_or("the report is not an object")?;
    if let Some(extra) = object.keys().find(|k| !keys.contains(&k.as_str())) {
        return Err(format!("unknown key `{extra}`"));
    }
    if require(report, "schema", "string")? != SCHEMA {
        return Err("wrong schema".to_owned());
    }
    if require(report, "version", "integer")? != VERSION {
        return Err("wrong version".to_owned());
    }
    for (_, v) in require(report, "versions", "object")?
        .as_object()
        .into_iter()
        .flatten()
    {
        v.as_str().ok_or("a version is not a string")?;
    }
    for (_, v) in require(report, "options", "object")?
        .as_object()
        .into_iter()
        .flatten()
    {
        v.as_bool().ok_or("an option is not a bool")?;
    }
    require(report, "interrupted", "bool")?;
    for image in require(report, "images", "array")?
        .as_array()
        .into_iter()
        .flatten()
    {
        for key in ["name", "repository", "digest"] {
            require(image, key, "string")?;
        }
        let digest = image["digest"].as_str().unwrap_or_default();
        if !digest.starts_with("sha256:") || digest.len() != 71 {
            return Err(format!("an image digest is not sha256: {digest}"));
        }
    }
    for check in require(report, "checks", "array")?
        .as_array()
        .into_iter()
        .flatten()
    {
        let id = require(check, "id", "string")?;
        require(check, "task", "string")?;
        require(check, "evidence", "array")?;
        let status = require(check, "status", "string")?
            .as_str()
            .unwrap_or_default();
        match status {
            "pass" | "fail" => {}
            "not_run" => {
                require(check, "reason", "string")
                    .map_err(|e| format!("check {id}: not_run needs a reason ({e})"))?;
                if !check["evidence"].as_array().is_some_and(Vec::is_empty) {
                    return Err(format!("check {id}: not_run carries evidence"));
                }
            }
            other => return Err(format!("check {id}: unknown status {other}")),
        }
    }
    for leftover in require(report, "leftovers", "array")?
        .as_array()
        .into_iter()
        .flatten()
    {
        leftover.as_str().ok_or("a leftover is not a string")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw01::redact::Identity;
    use std::path::Path;

    fn report() -> Report {
        Report {
            versions: BTreeMap::from([("tart".to_owned(), "2.41.1".to_owned())]),
            options: BTreeMap::from([("macos".to_owned(), false)]),
            images: vec![(
                "linux".to_owned(),
                "ghcr.io/example/linux".to_owned(),
                format!("sha256:{}", "a".repeat(64)),
            )],
            checks: vec![
                CheckResult::pass("tart-version", "TART-01", vec![json!("2.41.1")]),
                CheckResult::fail(
                    "get-json",
                    "TART-01",
                    vec![shape(&json!({"Memory": 4096, "Disk": null}))],
                ),
                CheckResult::not_run("IC-01", "DP-07", "the contract image was not built: push"),
            ],
            leftovers: vec!["vm hw01-x-linux".to_owned()],
            interrupted: false,
        }
    }

    #[test]
    fn should_match_the_schema_when_a_report_holds_every_status() {
        // given
        let redactor = Redactor::new(&Identity::default(), Path::new("/tmp/run"), &[]);

        // when
        let json = report().to_json(&redactor);

        // then
        validate(&json).expect("the schema holds");
        let statuses: Vec<_> = json["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .map(|c| c["status"].as_str().expect("status").to_owned())
            .collect();
        assert_eq!(statuses, ["pass", "fail", "not_run"]);
        assert_eq!(json["checks"][1]["evidence"][0]["Memory"], "integer");
        assert_eq!(json["checks"][1]["evidence"][0]["Disk"], "null");
    }

    #[test]
    fn should_refuse_a_report_when_a_field_is_wrong_or_unknown() {
        // given
        let redactor = Redactor::new(&Identity::default(), Path::new("/tmp/run"), &[]);
        let good = report().to_json(&redactor);
        let mut unknown = good.clone();
        unknown["hostname"] = json!("x");
        let mut bad_status = good.clone();
        bad_status["checks"][0]["status"] = json!("skipped");
        let mut reasonless = good.clone();
        reasonless["checks"][2]
            .as_object_mut()
            .expect("object")
            .remove("reason");

        // when
        let verdicts = [unknown, bad_status, reasonless].map(|r| validate(&r));

        // then
        for verdict in verdicts {
            assert!(verdict.is_err(), "{verdict:?}");
        }
    }

    #[test]
    fn should_fail_a_check_and_drop_its_evidence_when_it_leaks_after_redaction() {
        // given: a name the redactor replaces keeps the check; a token in a key does not
        let redactor = Redactor::new(&Identity::default(), Path::new("/tmp/run"), &[]);
        let mut leaky = report();
        // A JWT-shaped key, built at run time (no scanner-shaped literal in the repository).
        let jwt = format!(
            "{}{}.{}{}.sig",
            "ey", "JhbGciOiJIUzI1NiJ9", "ey", "JzdWIiOiIxIn0"
        );
        let mut key = serde_json::Map::new();
        key.insert(jwt, json!(1));
        leaky.checks[0].evidence = vec![Value::Object(key)];

        // when
        let json = leaky.to_json(&redactor);

        // then
        let check = &json["checks"][0];
        assert_eq!(check["status"], "fail");
        assert_eq!(check["evidence"], json!([]));
        assert!(
            check["reason"]
                .as_str()
                .expect("reason")
                .contains("redaction")
        );
    }
}
