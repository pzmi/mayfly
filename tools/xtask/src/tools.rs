//! Pinned development tools: the manifest in `tools/versions.toml`, `doctor`,
//! `install-tools` and `require` (D15: a missing tool fails closed, never passes).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::process::Command;

/// The manifest, embedded so `cargo xtask` works from any directory.
const MANIFEST: &str = include_str!("../../versions.toml");

/// How a tool is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// `cargo install --locked <name>@<version>`.
    Cargo,
    /// `rustup toolchain install <version> --profile minimal`.
    Rustup,
    /// A manual step; the manifest holds the URL.
    Manual(String),
    /// Version owned by a lockfile; noted in the manifest, not checked.
    Lockfile(String),
}

/// One pinned tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    /// Manifest key, for example `cargo-nextest`.
    pub name: String,
    /// Exact pinned version.
    pub version: String,
    /// Installation method.
    pub method: Method,
    /// Program and arguments that print the installed version.
    pub probe: Vec<String>,
    /// Match the version as a substring of the probe output instead of as a token.
    pub contains: bool,
    /// Cargo features `cargo install` enables (cargo-about's binary needs `cli`).
    pub features: Option<String>,
}

/// Result of checking one tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Installed at the pinned version.
    Ok,
    /// Not installed (probe could not run or failed).
    Missing,
    /// Installed with another version; carries the probe's first output line.
    Wrong(String),
}

/// Parses the manifest text into tools, sorted by name.
pub fn parse(text: &str) -> Result<Vec<Tool>, String> {
    let table: toml::Table = text.parse().map_err(|e| format!("versions.toml: {e}"))?;
    let tools = table
        .get("tools")
        .and_then(toml::Value::as_table)
        .ok_or("versions.toml: missing [tools]")?;
    let mut out = BTreeMap::new();
    for (name, value) in tools {
        let t = value
            .as_table()
            .ok_or_else(|| format!("versions.toml: tools.{name} is not a table"))?;
        let field = |key: &str| {
            t.get(key)
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("versions.toml: tools.{name}.{key} missing"))
        };
        let method = match field("method")?.as_str() {
            "cargo" => Method::Cargo,
            "rustup" => Method::Rustup,
            "manual" => Method::Manual(field("url")?),
            "lockfile" => Method::Lockfile(field("lockfile")?),
            other => {
                return Err(format!(
                    "versions.toml: tools.{name}: unknown method {other}"
                ));
            }
        };
        let probe: Vec<String> = t
            .get("probe")
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if probe.is_empty() && !matches!(method, Method::Lockfile(_)) {
            return Err(format!("versions.toml: tools.{name}.probe missing"));
        }
        out.insert(
            name.clone(),
            Tool {
                name: name.clone(),
                version: field("version")?,
                method,
                probe,
                contains: t
                    .get("contains")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(false),
                features: t
                    .get("features")
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned),
            },
        );
    }
    Ok(out.into_values().collect())
}

/// The embedded manifest, parsed.
pub fn manifest() -> Result<Vec<Tool>, String> {
    parse(MANIFEST)
}

/// Whether `output` reports exactly `version`.
fn version_matches(output: &str, version: &str, contains: bool) -> bool {
    if contains {
        return output.contains(version);
    }
    output
        .split_whitespace()
        .any(|tok| tok.trim_start_matches('v') == version)
}

/// Probes a tool, optionally with a replacement `PATH` for the probe process.
pub fn check_with_path(tool: &Tool, path: Option<&OsStr>) -> Status {
    let Some((program, args)) = tool.probe.split_first() else {
        return Status::Ok;
    };
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(path) = path {
        cmd.env("PATH", path);
    }
    let Ok(output) = cmd.output() else {
        return Status::Missing;
    };
    if !output.status.success() {
        return Status::Missing;
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    if version_matches(&text, &tool.version, tool.contains) {
        Status::Ok
    } else {
        let first = text.lines().next().unwrap_or("").trim().to_owned();
        Status::Wrong(first)
    }
}

/// The actionable hint for a tool that is missing or at the wrong version.
pub fn install_hint(tool: &Tool) -> String {
    match &tool.method {
        Method::Cargo | Method::Rustup => format!("run 'cargo xtask install-tools {}'", tool.name),
        Method::Manual(url) => format!("install manually from {url}"),
        Method::Lockfile(file) => format!("run 'npm ci' for the lockfile {file}"),
    }
}

/// The failure line for a tool that is not usable.
fn describe(tool: &Tool, status: &Status) -> String {
    match status {
        Status::Ok => format!("ok {} {}", tool.name, tool.version),
        Status::Missing => format!(
            "missing {} {}: {}",
            tool.name,
            tool.version,
            install_hint(tool)
        ),
        Status::Wrong(found) => format!(
            "wrong version of {} (found '{found}', want {}): {}",
            tool.name,
            tool.version,
            install_hint(tool)
        ),
    }
}

fn require_in(tools: &[Tool], name: &str, path: Option<&OsStr>) -> Result<(), String> {
    let tool = tools
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| format!("unknown tool {name}: add it to tools/versions.toml"))?;
    match check_with_path(tool, path) {
        Status::Ok => Ok(()),
        status => Err(describe(tool, &status)),
    }
}

/// Fails with an install hint unless `name` is installed at its pinned version. Every stage
/// that invokes an external tool calls this first.
pub fn require(name: &str) -> Result<(), String> {
    require_in(&manifest()?, name, None)
}

/// What `doctor` found for one tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// Installed at the pinned version.
    Ok,
    /// Not installed.
    Missing,
    /// Installed at another version; carries what the probe printed.
    Wrong(String),
    /// Not checked: the version is owned by a lockfile no `check` stage needs installed.
    Unchecked,
}

/// One row of the `doctor` report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Manifest key.
    pub tool: String,
    /// The pinned version.
    pub version: String,
    /// What was found.
    pub found: Found,
    /// What to do about it; empty when the tool is usable.
    pub hint: String,
    /// The `check` stages that need it; empty for a tool only other lanes need.
    pub stages: Vec<&'static str>,
}

impl Finding {
    /// Whether the tool blocks a stage: missing or at the wrong version.
    fn bad(&self) -> bool {
        matches!(self.found, Found::Missing | Found::Wrong(_))
    }
}

/// Every tool the `stages` declare (`needs` and `and_needs`), with the stages needing each.
pub fn needed_by(stages: &[crate::process::Stage]) -> BTreeMap<&'static str, Vec<&'static str>> {
    let mut out: BTreeMap<&'static str, Vec<&'static str>> = BTreeMap::new();
    for stage in stages {
        for tool in stage.tool.into_iter().chain(stage.also_tool) {
            out.entry(tool).or_default().push(stage.name);
        }
    }
    out
}

/// How `doctor` checks a tool: runs its manifest probe (tests replace it).
type Probe<'a> = &'a dyn Fn(&Tool) -> Status;

/// The finding for `name`, needed by `stages`.
fn finding(name: &str, stages: Vec<&'static str>, tools: &[Tool], probe: Probe<'_>) -> Finding {
    let tool = tools.iter().find(|t| t.name == name);
    let version = tool.map(|t| t.version.clone()).unwrap_or_default();
    let row = |found, hint: String| Finding {
        tool: name.to_owned(),
        version: version.clone(),
        found,
        hint,
        stages: stages.clone(),
    };
    let Some(tool) = tool else {
        return row(
            Found::Missing,
            format!("unknown tool {name}: add it to tools/versions.toml"),
        );
    };
    if let Method::Lockfile(file) = &tool.method {
        return row(Found::Unchecked, format!("version owned by {file}"));
    }
    match probe(tool) {
        Status::Ok => row(Found::Ok, String::new()),
        Status::Missing => row(Found::Missing, install_hint(tool)),
        Status::Wrong(seen) => row(Found::Wrong(seen), install_hint(tool)),
    }
}

/// The report: every tool `needs` names, by name, then every other pinned tool (a pin no
/// `check` stage needs is reported too, so none hides from `doctor`).
fn findings(
    tools: &[Tool],
    needs: &BTreeMap<&'static str, Vec<&'static str>>,
    probe: Probe<'_>,
) -> Vec<Finding> {
    let mut out: Vec<Finding> = needs
        .iter()
        .map(|(name, stages)| finding(name, stages.clone(), tools, probe))
        .collect();
    out.extend(
        tools
            .iter()
            .filter(|t| !needs.contains_key(t.name.as_str()))
            .map(|t| finding(&t.name, Vec::new(), tools, probe)),
    );
    out
}

/// One line per finding: `ok`, `missing`, `wrong` or `note`, the tool, its pin, the stages
/// that need it and, unless it is usable, the hint.
fn render_text(findings: &[Finding]) -> String {
    let mut out = String::new();
    for f in findings {
        let stages = if f.stages.is_empty() {
            " [not in check]".to_owned()
        } else {
            format!(" [{}]", f.stages.join(", "))
        };
        let head = format!("{} {}{stages}", f.tool, f.version);
        let line = match &f.found {
            Found::Ok => format!("ok      {head}"),
            Found::Missing => format!("missing {head}: {}", f.hint),
            Found::Wrong(seen) => format!("wrong   {head} (found '{seen}'): {}", f.hint),
            Found::Unchecked => format!("note    {head}: {}", f.hint),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// The findings as a JSON array, in the style of `list-checks`.
fn render_json(findings: &[Finding]) -> String {
    use crate::catalog::json_str;
    let rows: Vec<String> = findings
        .iter()
        .map(|f| {
            let (status, seen) = match &f.found {
                Found::Ok => ("ok", None),
                Found::Missing => ("missing", None),
                Found::Wrong(seen) => ("wrong", Some(seen.as_str())),
                Found::Unchecked => ("unchecked", None),
            };
            let stages: Vec<String> = f.stages.iter().map(|s| json_str(s)).collect();
            format!(
                "  {{\"tool\": {}, \"version\": {}, \"status\": {}, \"found\": {}, \"hint\": {}, \"stages\": [{}]}}",
                json_str(&f.tool),
                json_str(&f.version),
                json_str(status),
                seen.map_or_else(|| "null".to_owned(), json_str),
                json_str(&f.hint),
                stages.join(", ")
            )
        })
        .collect();
    format!("[\n{}\n]\n", rows.join(",\n"))
}

/// `cargo xtask doctor [--json]`: checks every pinned tool, first those a `check` stage needs,
/// prints one line each (or JSON), and exits 1 when any is missing or at the wrong version.
/// Run it first when the local gate and CI disagree.
pub fn doctor(json: bool) -> i32 {
    let tools = match manifest() {
        Ok(tools) => tools,
        Err(err) => {
            eprintln!("xtask: {err}");
            return 1;
        }
    };
    let probe = |tool: &Tool| check_with_path(tool, None);
    let report = findings(&tools, &needed_by(&crate::check::stages()), &probe);
    if json {
        print!("{}", render_json(&report));
    } else {
        print!("{}", render_text(&report));
    }
    let bad = report.iter().filter(|f| f.bad()).count();
    if bad > 0 {
        eprintln!("xtask: doctor: {bad} tool(s) missing or at the wrong version");
        1
    } else {
        0
    }
}

/// What `install` does for a Cargo or Rustup tool, given what the probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Pinned version already present: nothing to do (a restored CI cache).
    Skip,
    /// Not present: a plain install.
    Install,
    /// Present at another version: `cargo install` refuses to replace it without `--force`.
    Reinstall,
}

fn plan(status: &Status) -> Plan {
    match status {
        Status::Ok => Plan::Skip,
        Status::Missing => Plan::Install,
        Status::Wrong(_) => Plan::Reinstall,
    }
}

/// `cargo xtask install-tools [tool...]`: installs the named tools, or all installable
/// ones. Binary tools print their manual step. Returns the exit code.
pub fn install(names: &[String]) -> i32 {
    let tools = match manifest() {
        Ok(tools) => tools,
        Err(err) => {
            eprintln!("xtask: {err}");
            return 1;
        }
    };
    let selected: Vec<&Tool> = if names.is_empty() {
        tools.iter().collect()
    } else {
        let mut picked = Vec::new();
        for name in names {
            match tools.iter().find(|t| &t.name == name) {
                Some(tool) => picked.push(tool),
                None => {
                    eprintln!("xtask: unknown tool {name}: see tools/versions.toml");
                    return 2;
                }
            }
        }
        picked
    };
    let mut failed = false;
    for tool in selected {
        let action = plan(&check_with_path(tool, None));
        if action == Plan::Skip && matches!(tool.method, Method::Cargo | Method::Rustup) {
            println!("ok {} {} (already installed)", tool.name, tool.version);
            continue;
        }
        let force = action == Plan::Reinstall;
        let ran = match &tool.method {
            Method::Cargo => {
                let spec = format!("{}@{}", tool.name, tool.version);
                let mut args = vec!["install", "--locked"];
                if force {
                    args.push("--force");
                }
                if let Some(features) = &tool.features {
                    args.extend(["--features", features]);
                }
                args.push(&spec);
                run("cargo", &args)
            }
            Method::Rustup => run(
                "rustup",
                &[
                    "toolchain",
                    "install",
                    &tool.version,
                    "--profile",
                    "minimal",
                ],
            ),
            Method::Manual(url) => {
                println!(
                    "manual step for {} {}: install from {url}",
                    tool.name, tool.version
                );
                continue;
            }
            Method::Lockfile(file) => {
                println!("{}: installed from {file} by 'npm ci'", tool.name);
                continue;
            }
        };
        if !ran {
            eprintln!("xtask: installing {} failed", tool.name);
            failed = true;
        } else if let Err(err) = require(&tool.name) {
            eprintln!("xtask: installed {} but it is not usable: {err}", tool.name);
            failed = true;
        } else {
            println!("ok {} {}", tool.name, tool.version);
        }
    }
    i32::from(failed)
}

fn run(program: &str, args: &[&str]) -> bool {
    eprintln!("xtask: {program} {}", args.join(" "));
    Command::new(program)
        .args(args)
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_need_every_external_tool_of_the_gate_when_check_stages_are_read() {
        // given
        let stages = crate::check::stages();

        // when
        let needs = needed_by(&stages);

        // then
        for want in ["actionlint", "cargo-deny", "gitleaks", "shellcheck"] {
            assert!(needs.contains_key(want), "{want} missing from {needs:?}");
        }
        assert_eq!(needs["gitleaks"], ["secrets"]);
        assert_eq!(needs["actionlint"], ["workflows"]);
        assert_eq!(needs["shellcheck"], ["workflows"]);
        let tools = manifest().unwrap();
        let unknown: Vec<_> = needs
            .keys()
            .filter(|n| !tools.iter().any(|t| t.name == **n))
            .collect();
        assert!(
            unknown.is_empty(),
            "not in tools/versions.toml: {unknown:?}"
        );
    }

    #[test]
    fn should_report_missing_and_wrong_tools_with_hints_when_probes_fail() {
        // given
        let tools = manifest().unwrap();
        let needs = BTreeMap::from([("gitleaks", vec!["secrets"]), ("cargo-deny", vec!["deny"])]);
        let probe = |tool: &Tool| match tool.name.as_str() {
            "gitleaks" => Status::Missing,
            _ => Status::Wrong("cargo-deny 0.1.0".into()),
        };

        // when
        let report = findings(&tools, &needs, &probe);

        // then
        let by = |name: &str| report.iter().find(|f| f.tool == name).unwrap();
        assert_eq!(report.len(), tools.len());
        assert_eq!(
            by("cargo-deny").found,
            Found::Wrong("cargo-deny 0.1.0".into())
        );
        assert_eq!(by("cargo-deny").stages, ["deny"]);
        assert!(by("gitleaks").bad() && by("gitleaks").hint.contains("gitleaks/releases"));
        let text = render_text(&report);
        assert!(
            text.contains("missing gitleaks 8.30.1 [secrets]: install manually from"),
            "{text}"
        );
        assert!(
            text.contains("wrong   cargo-deny 0.20.2 [deny] (found 'cargo-deny 0.1.0')"),
            "{text}"
        );
    }

    #[test]
    fn should_report_an_unknown_tool_as_missing_when_the_manifest_lacks_it() {
        // given
        let needs = BTreeMap::from([("cargo-nope", vec!["x"])]);
        let probe = |_: &Tool| Status::Ok;

        // when
        let report = findings(&[], &needs, &probe);

        // then
        assert_eq!(report[0].found, Found::Missing);
        assert!(report[0].hint.contains("add it to tools/versions.toml"));
    }

    #[test]
    fn should_render_one_json_object_per_tool_when_json_is_asked() {
        // given
        let report = [
            Finding {
                tool: "cargo-deny".into(),
                version: "0.20.2".into(),
                found: Found::Wrong("0.1.0".into()),
                hint: "install \"it\"".into(),
                stages: vec!["deny", "other"],
            },
            Finding {
                tool: "gitleaks".into(),
                version: "8.30.1".into(),
                found: Found::Ok,
                hint: String::new(),
                stages: vec!["secrets"],
            },
        ];

        // when
        let json = render_json(&report);

        // then
        assert!(json.starts_with("[\n") && json.ends_with("\n]\n"), "{json}");
        assert!(
            json.contains(r#"{"tool": "cargo-deny", "version": "0.20.2", "status": "wrong", "found": "0.1.0", "hint": "install \"it\"", "stages": ["deny", "other"]}"#),
            "{json}"
        );
        assert!(json.contains(r#""status": "ok", "found": null"#), "{json}");
    }

    #[test]
    fn should_parse_the_json_flag_when_doctor_is_invoked() {
        use crate::cli::{Cli, Command};
        use clap::Parser;

        // given
        let args = ["xtask", "doctor", "--json"];

        // when
        let cli = Cli::try_parse_from(args).unwrap();

        // then
        assert!(matches!(cli.command, Command::Doctor { json: true }));
    }

    fn deny() -> Tool {
        manifest()
            .unwrap()
            .into_iter()
            .find(|t| t.name == "cargo-deny")
            .unwrap()
    }

    #[test]
    fn should_fail_with_install_hint_when_tool_missing() {
        // given
        let tools = manifest().unwrap();
        let empty = tempfile::TempDir::new().unwrap();

        // when
        let result = require_in(&tools, "cargo-deny", Some(empty.path().as_os_str()));

        // then
        let err = result.unwrap_err();
        assert!(
            err.starts_with("missing cargo-deny ")
                && err.contains("run 'cargo xtask install-tools cargo-deny'"),
            "{err}"
        );
    }

    #[test]
    fn should_skip_install_when_pinned_version_is_present() {
        // given
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("fake-deny");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho \"cargo-deny {}\"\n", deny().version),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut tool = deny();
        tool.probe = vec![script.to_str().unwrap().to_owned()];

        // when
        let status = check_with_path(&tool, None);

        // then
        assert_eq!(plan(&status), Plan::Skip);
    }

    #[test]
    fn should_force_reinstall_when_another_version_is_present() {
        // given
        let status = Status::Wrong("cargo-deny 0.0.1".into());

        // when
        let action = plan(&status);

        // then
        assert_eq!(action, Plan::Reinstall);
        assert_eq!(plan(&Status::Missing), Plan::Install);
    }

    #[test]
    fn should_parse_every_tool_when_manifest_is_loaded() {
        // given
        let loaded = manifest();

        // when
        let tools = loaded.unwrap();

        // then
        let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        for want in ["cargo-deny", "gitleaks", "actionlint", "shellcheck"] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
    }

    #[test]
    fn should_use_exact_versions_when_manifest_is_loaded() {
        // given
        let tools = manifest().unwrap();

        // when
        let loose: Vec<_> = tools
            .iter()
            .filter(|t| {
                !t.version.chars().any(|c| c.is_ascii_digit())
                    || t.version.contains(['^', '~', '*', '>', '<'])
            })
            .collect();

        // then
        assert!(loose.is_empty(), "{loose:?}");
    }

    #[test]
    fn should_match_token_exactly_when_version_is_compared() {
        // given
        let output = "cargo-nextest 0.9.148 (abc 2026-10-01)";

        // when
        let exact = version_matches(output, "0.9.148", false);
        let prefix = version_matches(output, "0.9.14", false);
        let tagged = version_matches("v8.30.1", "8.30.1", false);

        // then
        assert!(exact);
        assert!(!prefix);
        assert!(tagged);
    }

    #[test]
    fn should_report_wrong_version_when_probe_prints_another_version() {
        // given
        let mut tool = deny();
        tool.probe = vec!["echo".into(), "cargo-deny 0.0.1".into()];

        // when
        let status = check_with_path(&tool, None);

        // then
        assert_eq!(status, Status::Wrong("cargo-deny 0.0.1".into()));
    }

    #[test]
    fn should_reject_unknown_method_when_manifest_is_parsed() {
        // given
        let text = "[tools.x]\nversion = \"1\"\nmethod = \"magic\"\nprobe = [\"x\"]\n";

        // when
        let err = parse(text).unwrap_err();

        // then
        assert!(err.contains("unknown method"), "{err}");
    }
}
