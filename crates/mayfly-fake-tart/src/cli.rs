//! Argument parsing for the Tart surface mayfly uses.
//!
//! Hand-written because the fake must reject exactly the flags real Tart rejects: an adapter that
//! passes a flag Tart does not have fails here as it would there. Flag names come from the
//! `@Option`/`@Flag` declarations in Tart's `Sources/tart/Commands/*.swift` (public repository,
//! `main` branch, read 2026-11-09). Flags that only matter to a real hypervisor are accepted and
//! ignored.

use crate::fail::Fail;

/// How `list` and `get` render their output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Text,
    Json,
}

/// A parsed Tart invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Version,
    Create {
        name: String,
    },
    Pull {
        reference: String,
    },
    Clone {
        source: String,
        name: String,
        overwrite: bool,
    },
    Run {
        name: String,
    },
    Ip {
        name: String,
        wait: u64,
    },
    List {
        format: Format,
        source: Option<String>,
        quiet: bool,
    },
    Get {
        name: String,
        format: Format,
    },
    Stop {
        name: String,
        timeout: u64,
    },
    Delete {
        names: Vec<String>,
    },
    Exec {
        name: String,
        interactive: bool,
        argv: Vec<String>,
    },
}

impl Command {
    /// The name a fault rule's `command` matches.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Version => "--version",
            Self::Create { .. } => "create",
            Self::Pull { .. } => "pull",
            Self::Clone { .. } => "clone",
            Self::Run { .. } => "run",
            Self::Ip { .. } => "ip",
            Self::List { .. } => "list",
            Self::Get { .. } => "get",
            Self::Stop { .. } => "stop",
            Self::Delete { .. } => "delete",
            Self::Exec { .. } => "exec",
        }
    }

    /// The VM names a fault rule's `vm` matches; a clone's is the new VM, not the source.
    pub fn targets(&self) -> Vec<&str> {
        match self {
            Self::Version | Self::List { .. } => Vec::new(),
            Self::Create { name }
            | Self::Clone { name, .. }
            | Self::Run { name }
            | Self::Ip { name, .. }
            | Self::Get { name, .. }
            | Self::Stop { name, .. }
            | Self::Exec { name, .. } => vec![name],
            Self::Pull { reference } => vec![reference],
            Self::Delete { names } => names.iter().map(String::as_str).collect(),
        }
    }

    /// The program an `exec` runs inside the guest, what a fault rule's `arg0` matches.
    pub fn arg0(&self) -> Option<&str> {
        match self {
            Self::Exec { argv, .. } => argv.first().map(String::as_str),
            _ => None,
        }
    }
}

struct Flag {
    long: &'static str,
    short: Option<char>,
    takes_value: bool,
}

const fn switch(long: &'static str) -> Flag {
    Flag {
        long,
        short: None,
        takes_value: false,
    }
}

const fn option(long: &'static str) -> Flag {
    Flag {
        long,
        short: None,
        takes_value: true,
    }
}

const RUN_FLAGS: &[Flag] = &[
    switch("--no-graphics"),
    switch("--serial"),
    switch("--graphics"),
    switch("--no-audio"),
    switch("--no-usb-accessories"),
    switch("--no-clipboard"),
    switch("--recovery"),
    switch("--vnc"),
    switch("--vnc-experimental"),
    switch("--nested"),
    switch("--net-softnet"),
    switch("--net-host"),
    switch("--suspendable"),
    switch("--capture-system-keys"),
    switch("--no-trackpad"),
    switch("--no-pointer"),
    switch("--no-keyboard"),
    option("--serial-path"),
    option("--disk"),
    option("--rosetta"),
    option("--dir"),
    option("--net-bridged"),
    option("--net-fd"),
    option("--net-softnet-allow"),
    option("--net-softnet-block"),
    option("--net-softnet-control-fd"),
    option("--net-softnet-expose"),
    option("--root-disk-opts"),
    option("--provisioning-opts"),
];

const CLONE_FLAGS: &[Flag] = &[
    switch("--insecure"),
    switch("--deduplicate"),
    switch("--stacked"),
    switch("--overwrite"),
    option("--concurrency"),
    option("--prune-limit"),
];

const IP_FLAGS: &[Flag] = &[option("--wait"), option("--resolver")];
const LIST_FLAGS: &[Flag] = &[
    option("--source"),
    option("--format"),
    Flag {
        long: "--quiet",
        short: Some('q'),
        takes_value: false,
    },
];
const GET_FLAGS: &[Flag] = &[option("--format")];
const STOP_FLAGS: &[Flag] = &[Flag {
    long: "--timeout",
    short: Some('t'),
    takes_value: true,
}];
const CREATE_FLAGS: &[Flag] = &[
    switch("--linux"),
    option("--disk-size"),
    option("--from-ipsw"),
];
const PULL_FLAGS: &[Flag] = &[
    switch("--insecure"),
    option("--concurrency"),
    switch("--deduplicate"),
];
const EXEC_FLAGS: &[Flag] = &[
    Flag {
        long: "--interactive",
        short: Some('i'),
        takes_value: false,
    },
    Flag {
        long: "--tty",
        short: Some('t'),
        takes_value: false,
    },
];
const NO_FLAGS: &[Flag] = &[];

struct Parsed {
    positionals: Vec<String>,
    flags: Vec<(&'static str, Option<String>)>,
    rest: Vec<String>,
}

impl Parsed {
    fn has(&self, long: &str) -> bool {
        self.flags.iter().any(|(l, _)| *l == long)
    }

    fn value(&self, long: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(l, _)| *l == long)
            .and_then(|(_, v)| v.as_deref())
    }
}

/// Parses `args` against `flags`. With `command_follows` the first positional ends option parsing
/// and everything after it, an optional leading `--` dropped, is `rest` (`tart exec`'s
/// pass-through).
fn parse(args: &[String], flags: &'static [Flag], command_follows: bool) -> Result<Parsed, Fail> {
    let mut parsed = Parsed {
        positionals: Vec::new(),
        flags: Vec::new(),
        rest: Vec::new(),
    };
    let mut iter = args.iter();
    let mut options_done = false;
    while let Some(arg) = iter.next() {
        if command_follows && !parsed.positionals.is_empty() {
            parsed.rest.push(arg.clone());
            parsed.rest.extend(iter.by_ref().cloned());
            if parsed.rest.first().is_some_and(|a| a == "--") {
                parsed.rest.remove(0);
            }
            break;
        }
        if options_done || !arg.starts_with('-') || arg == "-" {
            parsed.positionals.push(arg.clone());
            continue;
        }
        if arg == "--" {
            options_done = true;
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if arg.starts_with("--") => (n, Some(v.to_owned())),
            _ => (arg.as_str(), None),
        };
        let flag = flags
            .iter()
            .find(|f| {
                f.long == name
                    || f.short
                        .is_some_and(|s| name.len() == 2 && name == format!("-{s}"))
            })
            .ok_or_else(|| Fail::validation(&format!("Unknown option '{name}'")))?;
        let value = if flag.takes_value {
            match inline {
                Some(v) => Some(v),
                None => Some(
                    iter.next()
                        .ok_or_else(|| {
                            Fail::validation(&format!("Missing value for '{}'", flag.long))
                        })?
                        .clone(),
                ),
            }
        } else if inline.is_some() {
            return Err(Fail::validation(&format!(
                "The option '{}' does not take a value",
                flag.long
            )));
        } else {
            None
        };
        parsed.flags.push((flag.long, value));
    }
    Ok(parsed)
}

fn exactly(parsed: &Parsed, count: usize, what: &str) -> Result<(), Fail> {
    match parsed.positionals.len().cmp(&count) {
        std::cmp::Ordering::Less => Err(Fail::validation(&format!(
            "Missing expected argument '<{what}>'"
        ))),
        std::cmp::Ordering::Greater => Err(Fail::validation(&format!(
            "Unexpected argument '{}'",
            parsed.positionals[count]
        ))),
        std::cmp::Ordering::Equal => Ok(()),
    }
}

fn number(parsed: &Parsed, long: &str, default: u64) -> Result<u64, Fail> {
    match parsed.value(long) {
        None => Ok(default),
        Some(text) => text
            .parse()
            .map_err(|_| Fail::validation(&format!("The value '{text}' is invalid for '{long}'"))),
    }
}

fn format_of(parsed: &Parsed) -> Result<Format, Fail> {
    match parsed.value("--format") {
        None | Some("text") => Ok(Format::Text),
        Some("json") => Ok(Format::Json),
        Some(other) => Err(Fail::validation(&format!(
            "The value '{other}' is invalid for '--format'"
        ))),
    }
}

/// Parses everything after `argv[0]`.
pub(crate) fn parse_command(args: &[String]) -> Result<Command, Fail> {
    let Some((first, rest)) = args.split_first() else {
        return Err(Fail::validation("Missing expected subcommand"));
    };
    match first.as_str() {
        "--version" => Ok(Command::Version),
        "create" => {
            let p = parse(rest, CREATE_FLAGS, false)?;
            exactly(&p, 1, "name")?;
            Ok(Command::Create {
                name: p.positionals[0].clone(),
            })
        }
        "pull" => {
            let p = parse(rest, PULL_FLAGS, false)?;
            exactly(&p, 1, "remote-name")?;
            Ok(Command::Pull {
                reference: p.positionals[0].clone(),
            })
        }
        "clone" => {
            let p = parse(rest, CLONE_FLAGS, false)?;
            exactly(
                &p,
                2,
                if p.positionals.is_empty() {
                    "source-name"
                } else {
                    "new-name"
                },
            )?;
            if p.positionals[1].contains('/') {
                return Err(Fail::validation("<new-name> should be a local name"));
            }
            Ok(Command::Clone {
                source: p.positionals[0].clone(),
                name: p.positionals[1].clone(),
                overwrite: p.has("--overwrite"),
            })
        }
        "run" => {
            let p = parse(rest, RUN_FLAGS, false)?;
            exactly(&p, 1, "name")?;
            if p.has("--graphics") && p.has("--no-graphics") {
                return Err(Fail::validation(
                    "--graphics and --no-graphics are mutually exclusive",
                ));
            }
            Ok(Command::Run {
                name: p.positionals[0].clone(),
            })
        }
        "ip" => {
            let p = parse(rest, IP_FLAGS, false)?;
            exactly(&p, 1, "name")?;
            Ok(Command::Ip {
                name: p.positionals[0].clone(),
                wait: number(&p, "--wait", 0)?,
            })
        }
        "list" => {
            let p = parse(rest, LIST_FLAGS, false)?;
            exactly(&p, 0, "")?;
            let source = p.value("--source").map(str::to_owned);
            if let Some(s) = &source
                && s != "local"
                && s != "oci"
            {
                return Err(Fail::validation(&format!("'{s}' is not a valid <source>")));
            }
            Ok(Command::List {
                format: format_of(&p)?,
                source,
                quiet: p.has("--quiet"),
            })
        }
        "get" => {
            let p = parse(rest, GET_FLAGS, false)?;
            exactly(&p, 1, "name")?;
            Ok(Command::Get {
                name: p.positionals[0].clone(),
                format: format_of(&p)?,
            })
        }
        "stop" => {
            let p = parse(rest, STOP_FLAGS, false)?;
            exactly(&p, 1, "name")?;
            Ok(Command::Stop {
                name: p.positionals[0].clone(),
                timeout: number(&p, "--timeout", 30)?,
            })
        }
        "delete" => {
            let p = parse(rest, NO_FLAGS, false)?;
            if p.positionals.is_empty() {
                return Err(Fail::validation("Missing expected argument '<name> ...'"));
            }
            Ok(Command::Delete {
                names: p.positionals,
            })
        }
        "exec" => {
            let p = parse(rest, EXEC_FLAGS, true)?;
            exactly(&p, 1, "name")?;
            if p.rest.is_empty() {
                return Err(Fail::validation(
                    "Missing expected argument '<command> ...'",
                ));
            }
            Ok(Command::Exec {
                name: p.positionals[0].clone(),
                interactive: p.has("--interactive"),
                argv: p.rest,
            })
        }
        other => Err(Fail::validation(&format!(
            "The subcommand '{other}' is not recognized"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Format, parse_command};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn should_parse_every_command_when_given_its_documented_shape() {
        // given
        let cases: Vec<(Vec<&str>, Command)> = vec![
            (vec!["--version"], Command::Version),
            (
                vec!["create", "base", "--linux"],
                Command::Create {
                    name: "base".into(),
                },
            ),
            (
                vec!["pull", "ghcr.io/x/y:1"],
                Command::Pull {
                    reference: "ghcr.io/x/y:1".into(),
                },
            ),
            (
                vec!["clone", "base", "vm1", "--overwrite"],
                Command::Clone {
                    source: "base".into(),
                    name: "vm1".into(),
                    overwrite: true,
                },
            ),
            (
                vec!["run", "--no-graphics", "--dir=a:b", "vm1"],
                Command::Run { name: "vm1".into() },
            ),
            (
                vec!["ip", "vm1", "--wait", "5"],
                Command::Ip {
                    name: "vm1".into(),
                    wait: 5,
                },
            ),
            (
                vec!["list", "--format", "json", "--source=local", "-q"],
                Command::List {
                    format: Format::Json,
                    source: Some("local".into()),
                    quiet: true,
                },
            ),
            (
                vec!["get", "vm1", "--format", "json"],
                Command::Get {
                    name: "vm1".into(),
                    format: Format::Json,
                },
            ),
            (
                vec!["stop", "vm1", "-t", "7"],
                Command::Stop {
                    name: "vm1".into(),
                    timeout: 7,
                },
            ),
            (
                vec!["delete", "a", "b"],
                Command::Delete {
                    names: vec!["a".into(), "b".into()],
                },
            ),
            (
                vec!["exec", "-i", "vm1", "--", "sh", "-c", "x"],
                Command::Exec {
                    name: "vm1".into(),
                    interactive: true,
                    argv: args(&["sh", "-c", "x"]),
                },
            ),
        ];

        for (input, expected) in cases {
            // when
            let parsed = parse_command(&args(&input));

            // then
            assert_eq!(parsed, Ok(expected), "{input:?}");
        }
    }

    #[test]
    fn should_pass_option_like_words_through_when_they_follow_the_exec_name() {
        // given
        let input = args(&["exec", "vm1", "ls", "-i", "--version"]);

        // when
        let parsed = parse_command(&input);

        // then
        assert_eq!(
            parsed,
            Ok(Command::Exec {
                name: "vm1".into(),
                interactive: false,
                argv: args(&["ls", "-i", "--version"])
            })
        );
    }

    #[test]
    fn should_refuse_with_the_usage_code_when_the_arguments_are_wrong() {
        // given
        let bad: Vec<Vec<&str>> = vec![
            vec![],
            vec!["bogus"],
            vec!["run"],
            vec!["run", "a", "b"],
            vec!["run", "--bogus", "a"],
            vec!["run", "--graphics", "--no-graphics", "a"],
            vec!["run", "--dir"],
            vec!["run", "--no-graphics=1", "a"],
            vec!["clone", "only"],
            vec!["clone", "a", "x/y"],
            vec!["ip", "a", "--wait", "soon"],
            vec!["list", "--format", "yaml"],
            vec!["list", "--source", "remote"],
            vec!["list", "extra"],
            vec!["delete"],
            vec!["exec", "vm1"],
            vec!["exec"],
        ];

        for input in bad {
            // when
            let failure = parse_command(&args(&input)).expect_err("must be refused");

            // then
            assert_eq!(failure.code, 64, "{input:?}");
            assert!(failure.message.starts_with("Error: "), "{input:?}");
        }
    }

    #[test]
    fn should_expose_the_name_targets_and_program_when_a_fault_rule_matches() {
        // given
        let exec = parse_command(&args(&["exec", "vm1", "launcher"])).unwrap();
        let delete = parse_command(&args(&["delete", "a", "b"])).unwrap();
        let clone = parse_command(&args(&["clone", "base", "vm1"])).unwrap();
        let list = parse_command(&args(&["list"])).unwrap();
        let pull = parse_command(&args(&["pull", "r"])).unwrap();

        // when
        let exec_view = (exec.name(), exec.targets(), exec.arg0());

        // then
        assert_eq!(exec_view, ("exec", vec!["vm1"], Some("launcher")));
        assert_eq!(delete.targets(), vec!["a", "b"]);
        assert_eq!((clone.targets(), clone.arg0()), (vec!["vm1"], None));
        assert_eq!(list.targets(), Vec::<&str>::new());
        assert_eq!(pull.targets(), vec!["r"]);
    }
}
