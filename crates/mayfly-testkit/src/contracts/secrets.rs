//! The `SecretProvider` contract suite (ver §5, ADR 0033): one set of cases every provider
//! passes, written once over a [`SecretSourceFactory`], plus the cases of the two schemes whose
//! refusals are the point of the scheme: `file:` ([`FileSourceFactory`]) and `exec:`
//! ([`ExecSourceFactory`]).
//!
//! | Id | Case |
//! |:--|:--|
//! | SP-01 | a reference resolves to exactly the value installed |
//! | SP-02 | a second resolution after a rotation returns the new value (no cache) |
//! | SP-03 | an absent source is `NotFound` |
//! | SP-04 | neither the secret nor the result prints the value through `Debug` or `Display` |
//! | SP-05 | an empty source is refused |
//! | SP-06 | exactly one trailing `\n` or `\r\n` is dropped |
//! | SP-07 | a source above the limit is refused and the refusal does not echo it |
//! | SP-08 | a multi-line value (a PEM key) resolves byte for byte |
//! | FS-01 | a file readable or writable by group or others is refused, without echo |
//! | FS-02 | a file in a directory open to group or others is refused |
//! | FS-03 | a file of another user is refused by its own owner check, not an ancestor's |
//! | FS-04 | a symlink to a good file is refused |
//! | FS-05 | a good file in a directory reached through a symlink is refused |
//! | FS-06 | a relative path is refused |
//! | FS-07 | a directory is not a secret file |
//! | FS-08 | narrower modes (`0400` in `0500`) resolve |
//! | FS-09 | a file below an ancestor writable by group or others (no sticky bit) is refused |
//! | FS-10 | an ancestor symlink owned by the daemon's user is followed |
//! | EX-01 | a non-zero exit is refused and nothing the command printed is echoed |
//! | EX-02 | a command that never ends is cut at the deadline |
//! | EX-03 | the path runs as argv: shell syntax in it is not interpreted |
//! | EX-04 | the command does not inherit the daemon's environment |
//! | EX-05 | a relative path is refused |
//! | EX-06 | a command killed by a signal is refused |
//! | EX-07 | an executable, or its directory, writable by group or others is refused unrun |
//! | EX-08 | output above the limit is refused at once, not at the deadline |
//!
//! [`check_secret_contract`], [`check_file_contract`] and [`check_exec_contract`] return a
//! report of failed cases instead of panicking: the suite's own tests use them to show the
//! deliberately broken `PermissiveFileProvider` and `ShellExecProvider` failing named cases
//! (`mayfly-adapter-secrets/tests/contract.rs`). No case is skipped: a factory that cannot
//! build what a case needs fails it.

use std::fmt::{self, Display};
use std::time::Duration;

use mayfly_control::secrets::{MAX_SECRET_BYTES, Refusal, SecretError, SecretProvider};
use mayfly_domain::{Secret, SecretRef};
use mayfly_testkit_shared::child_env;

/// Builds sources of one scheme and the provider under test.
pub trait SecretSourceFactory {
    /// The implementation under test.
    type Provider: SecretProvider;

    /// The provider as it stands now (after the last [`rotate`](Self::rotate)).
    fn provider(&self) -> &Self::Provider;

    /// A new source holding `value`, safe by the scheme's rules, and its reference.
    fn install(&mut self, value: &str) -> SecretRef;

    /// Replaces the value behind `reference` with `value`, as an operator rotating it would.
    fn rotate(&mut self, reference: &SecretRef, value: &str);

    /// A reference of this scheme with nothing behind it.
    fn absent(&mut self) -> SecretRef;
}

/// What the `file:` cases need besides the generic ones.
pub trait FileSourceFactory: SecretSourceFactory {
    /// A file holding `value` with `file_mode`, alone in a new directory with `dir_mode`, both
    /// owned by the daemon's user.
    fn install_file(&mut self, value: &str, file_mode: u32, dir_mode: u32) -> SecretRef;

    /// A symlink, in a `0700` directory, to a `0600` file holding `value`.
    fn install_symlink(&mut self, value: &str) -> SecretRef;

    /// A `0600` file holding `value` in a `0700` directory, named through a symlink to that
    /// directory.
    fn install_in_symlinked_directory(&mut self, value: &str) -> SecretRef;

    /// A `0700` directory named as if it were the secret file.
    fn directory(&mut self) -> SecretRef;

    /// A `0600` file holding `value` in a `0700` directory, below a directory of mode `0777`
    /// without the sticky bit.
    fn install_under_open_ancestor(&mut self, value: &str) -> SecretRef;

    /// A `0600` file holding `value` in a `0700` directory, named through a symlink (owned by
    /// the daemon's user) to that directory's parent.
    fn install_under_symlinked_ancestor(&mut self, value: &str) -> SecretRef;

    /// A provider that expects the files to belong to another user than the one
    /// [`install`](SecretSourceFactory::install) creates them as, and still accepts the
    /// directories above the secret's directory, so that FS-03 is decided by the secret's own
    /// owner (its file or directory) and not by who runs the tests: an ancestor refusal fails it.
    fn foreign_owner_provider(&self) -> Self::Provider;
}

/// What the `exec:` cases need besides the generic ones.
pub trait ExecSourceFactory: SecretSourceFactory {
    /// An executable `/bin/sh` script named `name` (in a new directory) whose body is `body`.
    fn install_script(&mut self, name: &str, body: &str) -> SecretRef;

    /// Like [`install_script`](Self::install_script), with the script's mode and its
    /// directory's mode.
    fn install_script_with_modes(
        &mut self,
        name: &str,
        body: &str,
        file_mode: u32,
        dir_mode: u32,
    ) -> SecretRef;

    /// The provider with another deadline.
    fn provider_with_deadline(&self, deadline: Duration) -> Self::Provider;
}

/// One case of the contract (the table in the module documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SecretCase {
    /// SP-01.
    Resolves,
    /// SP-02.
    ReResolvesOnRefresh,
    /// SP-03.
    AbsentIsNotFound,
    /// SP-04.
    NoValueInDebug,
    /// SP-05.
    EmptyRefused,
    /// SP-06.
    OneTrailingLineBreakDropped,
    /// SP-07.
    TooLargeRefusedWithoutEcho,
    /// SP-08.
    MultiLineValue,
    /// FS-01.
    WideFileModeRefused,
    /// FS-02.
    WideDirectoryRefused,
    /// FS-03.
    ForeignOwnerRefused,
    /// FS-04.
    SymlinkRefused,
    /// FS-05.
    SymlinkedDirectoryRefused,
    /// FS-06.
    RelativeFileRefused,
    /// FS-07.
    NotRegularFileRefused,
    /// FS-08.
    NarrowModesResolve,
    /// FS-09.
    OpenAncestorRefused,
    /// FS-10.
    OwnedAncestorSymlinkFollowed,
    /// EX-01.
    NonZeroExitRefusedWithoutEcho,
    /// EX-02.
    DeadlineCutsHungCommand,
    /// EX-03.
    ArgvNotShell,
    /// EX-04.
    NoInheritedEnvironment,
    /// EX-05.
    RelativeCommandRefused,
    /// EX-06.
    SignalRefused,
    /// EX-07.
    OpenExecutableRefused,
    /// EX-08.
    OversizedOutputRefusedAtOnce,
}

impl SecretCase {
    /// The cases every provider passes.
    pub const GENERIC: [Self; 8] = [
        Self::Resolves,
        Self::ReResolvesOnRefresh,
        Self::AbsentIsNotFound,
        Self::NoValueInDebug,
        Self::EmptyRefused,
        Self::OneTrailingLineBreakDropped,
        Self::TooLargeRefusedWithoutEcho,
        Self::MultiLineValue,
    ];

    /// The `file:` cases.
    pub const FILE: [Self; 10] = [
        Self::WideFileModeRefused,
        Self::WideDirectoryRefused,
        Self::ForeignOwnerRefused,
        Self::SymlinkRefused,
        Self::SymlinkedDirectoryRefused,
        Self::RelativeFileRefused,
        Self::NotRegularFileRefused,
        Self::NarrowModesResolve,
        Self::OpenAncestorRefused,
        Self::OwnedAncestorSymlinkFollowed,
    ];

    /// The `exec:` cases.
    pub const EXEC: [Self; 8] = [
        Self::NonZeroExitRefusedWithoutEcho,
        Self::DeadlineCutsHungCommand,
        Self::ArgvNotShell,
        Self::NoInheritedEnvironment,
        Self::RelativeCommandRefused,
        Self::SignalRefused,
        Self::OpenExecutableRefused,
        Self::OversizedOutputRefusedAtOnce,
    ];

    /// The case's id in the module table.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Resolves => "SP-01",
            Self::ReResolvesOnRefresh => "SP-02",
            Self::AbsentIsNotFound => "SP-03",
            Self::NoValueInDebug => "SP-04",
            Self::EmptyRefused => "SP-05",
            Self::OneTrailingLineBreakDropped => "SP-06",
            Self::TooLargeRefusedWithoutEcho => "SP-07",
            Self::MultiLineValue => "SP-08",
            Self::WideFileModeRefused => "FS-01",
            Self::WideDirectoryRefused => "FS-02",
            Self::ForeignOwnerRefused => "FS-03",
            Self::SymlinkRefused => "FS-04",
            Self::SymlinkedDirectoryRefused => "FS-05",
            Self::RelativeFileRefused => "FS-06",
            Self::NotRegularFileRefused => "FS-07",
            Self::NarrowModesResolve => "FS-08",
            Self::OpenAncestorRefused => "FS-09",
            Self::OwnedAncestorSymlinkFollowed => "FS-10",
            Self::NonZeroExitRefusedWithoutEcho => "EX-01",
            Self::DeadlineCutsHungCommand => "EX-02",
            Self::ArgvNotShell => "EX-03",
            Self::NoInheritedEnvironment => "EX-04",
            Self::RelativeCommandRefused => "EX-05",
            Self::SignalRefused => "EX-06",
            Self::OpenExecutableRefused => "EX-07",
            Self::OversizedOutputRefusedAtOnce => "EX-08",
        }
    }
}

/// A failed case and what the provider did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretCaseFailure {
    /// The case.
    pub case: SecretCase,
    /// What was expected and what happened (never a resolved value).
    pub detail: String,
}

/// The outcome of a contract run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecretContractReport {
    /// Every failed case, in run order.
    pub failures: Vec<SecretCaseFailure>,
}

impl SecretContractReport {
    /// Whether every case passed.
    #[must_use]
    pub fn is_green(&self) -> bool {
        self.failures.is_empty()
    }

    /// The ids of the failed cases, in run order.
    #[must_use]
    pub fn failed_ids(&self) -> Vec<&'static str> {
        self.failures.iter().map(|f| f.case.id()).collect()
    }
}

impl Display for SecretContractReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.failures.is_empty() {
            return f.write_str("secret provider contract: every case passed");
        }
        writeln!(
            f,
            "secret provider contract: {} case(s) failed",
            self.failures.len()
        )?;
        for failure in &self.failures {
            writeln!(f, "- {}: {}", failure.case.id(), failure.detail)?;
        }
        Ok(())
    }
}

/// Runs the generic cases.
pub async fn check_secret_contract<F: SecretSourceFactory>(
    factory: &mut F,
) -> SecretContractReport {
    let mut report = SecretContractReport::default();
    for case in SecretCase::GENERIC {
        record(&mut report, case, generic_case(factory, case).await);
    }
    report
}

/// Runs the generic cases, then the `file:` ones.
pub async fn check_file_contract<F: FileSourceFactory>(factory: &mut F) -> SecretContractReport {
    let mut report = check_secret_contract(factory).await;
    for case in SecretCase::FILE {
        record(&mut report, case, file_case(factory, case).await);
    }
    report
}

/// Runs the generic cases, then the `exec:` ones.
pub async fn check_exec_contract<F: ExecSourceFactory>(factory: &mut F) -> SecretContractReport {
    let mut report = check_secret_contract(factory).await;
    for case in SecretCase::EXEC {
        record(&mut report, case, exec_case(factory, case).await);
    }
    report
}

fn record(report: &mut SecretContractReport, case: SecretCase, outcome: Result<(), String>) {
    if let Err(detail) = outcome {
        report.failures.push(SecretCaseFailure { case, detail });
    }
}

/// The resolved value as text, for comparison only; never put in a failure detail.
async fn value_of<P: SecretProvider>(
    provider: &P,
    reference: &SecretRef,
) -> Result<String, SecretError> {
    provider
        .resolve(reference)
        .await
        .map(|secret| secret.expose().clone())
}

fn expect_value(got: Result<String, SecretError>, want: &str, what: &str) -> Result<(), String> {
    match got {
        Ok(value) if value == want => Ok(()),
        Ok(value) => Err(format!(
            "{what}: resolved a different value ({} bytes, wanted {})",
            value.len(),
            want.len()
        )),
        Err(err) => Err(format!("{what}: refused: {err}")),
    }
}

/// Checks a refusal: its kind matches and neither form of the error contains `canary`.
fn expect_refusal(
    got: Result<Secret<String>, SecretError>,
    matches: impl Fn(&Refusal) -> bool,
    canary: &str,
    what: &str,
) -> Result<(), String> {
    match got {
        Ok(_) => Err(format!("{what}: resolved; it must be refused")),
        Err(err) if !matches(&err.refusal) => Err(format!("{what}: wrong refusal: {err}")),
        Err(err) => {
            let shown = format!("{err} {err:?}");
            if !canary.is_empty() && shown.contains(canary) {
                Err(format!("{what}: the refusal echoes the source's content"))
            } else {
                Ok(())
            }
        }
    }
}

async fn generic_case<F: SecretSourceFactory>(
    factory: &mut F,
    case: SecretCase,
) -> Result<(), String> {
    match case {
        SecretCase::Resolves => {
            let reference = factory.install("synthetic-sp01-value");
            let got = value_of(factory.provider(), &reference).await;
            expect_value(got, "synthetic-sp01-value", "installed value")
        }
        SecretCase::ReResolvesOnRefresh => {
            let reference = factory.install("synthetic-sp02-first");
            let first = value_of(factory.provider(), &reference).await;
            expect_value(first, "synthetic-sp02-first", "before rotation")?;
            factory.rotate(&reference, "synthetic-sp02-second");
            let second = value_of(factory.provider(), &reference).await;
            expect_value(second, "synthetic-sp02-second", "after rotation")
        }
        SecretCase::AbsentIsNotFound => {
            let reference = factory.absent();
            let got = factory.provider().resolve(&reference).await;
            expect_refusal(got, |r| *r == Refusal::NotFound, "", "absent source")
        }
        SecretCase::NoValueInDebug => {
            let reference = factory.install("synthetic-sp04-value");
            let got = factory.provider().resolve(&reference).await;
            let shown = format!("{got:?} {got:#?}");
            let secret = got.map_err(|err| format!("refused: {err}"))?;
            let shown = format!("{shown} {secret:?} {secret}");
            if shown.contains("synthetic-sp04-value") || shown.contains("sp04") {
                Err("the value is printed through Debug or Display".to_owned())
            } else {
                Ok(())
            }
        }
        SecretCase::EmptyRefused => {
            let reference = factory.install("");
            let got = factory.provider().resolve(&reference).await;
            expect_refusal(got, |r| *r == Refusal::Empty, "", "empty source")?;
            let reference = factory.install("\n");
            let got = factory.provider().resolve(&reference).await;
            expect_refusal(got, |r| *r == Refusal::Empty, "", "a lone line break")
        }
        SecretCase::OneTrailingLineBreakDropped => {
            for (installed, want) in [
                ("synthetic-sp06\n", "synthetic-sp06"),
                ("synthetic-sp06\r\n", "synthetic-sp06"),
                ("synthetic-sp06\n\n", "synthetic-sp06\n"),
                ("synthetic-sp06 ", "synthetic-sp06 "),
            ] {
                let reference = factory.install(installed);
                let got = value_of(factory.provider(), &reference).await;
                expect_value(got, want, &format!("{installed:?}"))?;
            }
            Ok(())
        }
        SecretCase::TooLargeRefusedWithoutEcho => {
            let mut value = String::from("canary-sp07-");
            while value.len() <= MAX_SECRET_BYTES {
                value.push('x');
            }
            let reference = factory.install(&value);
            let got = factory.provider().resolve(&reference).await;
            expect_refusal(
                got,
                |r| matches!(r, Refusal::TooLarge { .. }),
                "canary-sp07",
                "a source above the limit",
            )?;
            let fits = &value[..MAX_SECRET_BYTES];
            let reference = factory.install(fits);
            let got = value_of(factory.provider(), &reference).await;
            expect_value(got, fits, "a source at the limit")
        }
        SecretCase::MultiLineValue => {
            let pem =
                "-----BEGIN SYNTHETIC KEY-----\nc3ludGhldGlj\r\nAAAA\n-----END SYNTHETIC KEY-----";
            let reference = factory.install(&format!("{pem}\n"));
            let got = value_of(factory.provider(), &reference).await;
            expect_value(got, pem, "a multi-line value")
        }
        _ => Err(format!("{} is not a generic case", case.id())),
    }
}

async fn file_case<F: FileSourceFactory>(factory: &mut F, case: SecretCase) -> Result<(), String> {
    match case {
        SecretCase::WideFileModeRefused => {
            for mode in [0o644, 0o640, 0o604, 0o620, 0o602, 0o4600] {
                let reference = factory.install_file("canary-fs01", mode, 0o700);
                let got = factory.provider().resolve(&reference).await;
                expect_refusal(
                    got,
                    |r| matches!(r, Refusal::TooOpen { .. }),
                    "canary-fs01",
                    &format!("a file of mode {mode:04o}"),
                )?;
            }
            Ok(())
        }
        SecretCase::WideDirectoryRefused => {
            for mode in [0o755, 0o750, 0o701, 0o770] {
                let reference = factory.install_file("canary-fs02", 0o600, mode);
                let got = factory.provider().resolve(&reference).await;
                expect_refusal(
                    got,
                    |r| matches!(r, Refusal::DirectoryTooOpen { .. }),
                    "canary-fs02",
                    &format!("a directory of mode {mode:04o}"),
                )?;
            }
            Ok(())
        }
        SecretCase::ForeignOwnerRefused => {
            let reference = factory.install("canary-fs03");
            let got = factory.foreign_owner_provider().resolve(&reference).await;
            expect_refusal(
                got,
                |r| {
                    matches!(
                        r,
                        Refusal::WrongOwner { .. } | Refusal::DirectoryWrongOwner { .. }
                    )
                },
                "canary-fs03",
                "a file of another owner",
            )
        }
        SecretCase::SymlinkRefused => {
            let reference = factory.install_symlink("canary-fs04");
            let got = factory.provider().resolve(&reference).await;
            expect_refusal(got, |r| *r == Refusal::Symlink, "canary-fs04", "a symlink")
        }
        SecretCase::SymlinkedDirectoryRefused => {
            let reference = factory.install_in_symlinked_directory("canary-fs05");
            let got = factory.provider().resolve(&reference).await;
            expect_refusal(
                got,
                |r| *r == Refusal::Symlink,
                "canary-fs05",
                "a file under a symlinked directory",
            )
        }
        SecretCase::RelativeFileRefused => {
            let reference = SecretRef::File {
                path: "relative/secret".into(),
            };
            let got = factory.provider().resolve(&reference).await;
            expect_refusal(got, |r| *r == Refusal::RelativePath, "", "a relative path")
        }
        SecretCase::NotRegularFileRefused => {
            let reference = factory.directory();
            let got = factory.provider().resolve(&reference).await;
            expect_refusal(got, |r| *r == Refusal::NotRegularFile, "", "a directory")
        }
        SecretCase::NarrowModesResolve => {
            let reference = factory.install_file("synthetic-fs08", 0o400, 0o500);
            let got = value_of(factory.provider(), &reference).await;
            expect_value(got, "synthetic-fs08", "a 0400 file in a 0500 directory")
        }
        SecretCase::OpenAncestorRefused => {
            let reference = factory.install_under_open_ancestor("canary-fs09");
            let got = factory.provider().resolve(&reference).await;
            expect_refusal(
                got,
                |r| matches!(r, Refusal::AncestorTooOpen { .. }),
                "canary-fs09",
                "a file below a 0777 directory",
            )
        }
        SecretCase::OwnedAncestorSymlinkFollowed => {
            let reference = factory.install_under_symlinked_ancestor("synthetic-fs10");
            let got = value_of(factory.provider(), &reference).await;
            expect_value(
                got,
                "synthetic-fs10",
                "a file below an owned ancestor symlink",
            )
        }
        _ => Err(format!("{} is not a file case", case.id())),
    }
}

async fn exec_case<F: ExecSourceFactory>(factory: &mut F, case: SecretCase) -> Result<(), String> {
    match case {
        SecretCase::NonZeroExitRefusedWithoutEcho => {
            let reference = factory.install_script(
                "fails",
                "printf 'canary-ex01'\nprintf 'canary-ex01' >&2\nexit 3",
            );
            let got = factory.provider().resolve(&reference).await;
            expect_refusal(
                got,
                |r| *r == Refusal::Exited { code: Some(3) },
                "canary-ex01",
                "a command exiting 3",
            )
        }
        SecretCase::DeadlineCutsHungCommand => {
            // The command never ends by itself: the provider's deadline is the only way out.
            let reference =
                factory.install_script("hangs", "printf 'canary-ex02'\nexec tail -f /dev/null");
            let hasty = factory.provider_with_deadline(Duration::from_millis(200));
            let got = hasty.resolve(&reference).await;
            expect_refusal(
                got,
                |r| matches!(r, Refusal::TimedOut { .. }),
                "canary-ex02",
                "a command that never ends",
            )
        }
        SecretCase::ArgvNotShell => {
            let reference = factory.install_script("get;false", "printf 'synthetic-ex03'");
            let got = value_of(factory.provider(), &reference).await;
            expect_value(got, "synthetic-ex03", "a path holding shell syntax")
        }
        SecretCase::NoInheritedEnvironment => {
            let Some(name) = child_env::inherited_variable() else {
                return Err("the test process has no variable to check inheritance with".into());
            };
            let reference = factory.install_script("environment", &child_env::probe_script(&name));
            let got = value_of(factory.provider(), &reference).await;
            expect_value(
                got,
                child_env::NOT_INHERITED,
                &format!("the daemon's ${name} in the child"),
            )
        }
        SecretCase::RelativeCommandRefused => {
            let reference = SecretRef::Exec {
                path: "relative/command".into(),
            };
            let got = factory.provider().resolve(&reference).await;
            expect_refusal(got, |r| *r == Refusal::RelativePath, "", "a relative path")
        }
        SecretCase::SignalRefused => {
            let reference = factory.install_script("killed", "printf 'canary-ex06'\nkill -9 $$");
            let got = factory.provider().resolve(&reference).await;
            expect_refusal(
                got,
                |r| *r == Refusal::Exited { code: None },
                "canary-ex06",
                "a command killed by a signal",
            )
        }
        SecretCase::OpenExecutableRefused => {
            let script = "printf 'canary-ex07' > \"$(dirname \"$0\")/ran\"\nprintf 'canary-ex07'";
            for (file_mode, dir_mode, what) in [
                (0o720, 0o700, "a group-writable command"),
                (0o702, 0o700, "a world-writable command"),
                (0o700, 0o770, "a command in a group-writable directory"),
            ] {
                let reference =
                    factory.install_script_with_modes("open", script, file_mode, dir_mode);
                let got = factory.provider().resolve(&reference).await;
                expect_refusal(
                    got,
                    |r| {
                        matches!(
                            r,
                            Refusal::TooOpen { .. } | Refusal::DirectoryTooOpen { .. }
                        )
                    },
                    "canary-ex07",
                    what,
                )?;
            }
            Ok(())
        }
        SecretCase::OversizedOutputRefusedAtOnce => {
            // Prints more than the limit, then never ends: only a provider that stops at the
            // limit answers TooLarge; one that reads on answers TimedOut after a minute.
            let reference = factory.install_script(
                "floods",
                &format!(
                    "head -c {} /dev/zero | tr '\\0' x\nexec tail -f /dev/null",
                    MAX_SECRET_BYTES + 1
                ),
            );
            let patient = factory.provider_with_deadline(Duration::from_secs(60));
            let got = patient.resolve(&reference).await;
            expect_refusal(
                got,
                |r| matches!(r, Refusal::TooLarge { .. }),
                "",
                "a command whose output passes the limit",
            )
        }
        _ => Err(format!("{} is not an exec case", case.id())),
    }
}
