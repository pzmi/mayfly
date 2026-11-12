//! A command's failure: the exit code and the stderr text a caller sees.
//!
//! Shapes mirror Tart's error handling (`Sources/tart/VMStorageHelper.swift`, `Root.swift` and
//! `Commands/Run.swift` at the `2.41.1` release tag, read 2026-11-11): every `RuntimeError` has an
//! exit code and prints its bare description, 2 for `VMDoesNotExist`, `VMNotRunning` and
//! `VMAlreadyRunning`, 1 for the others (`VMIsRunning`, `NoIPAddressFound`, …); any other error
//! goes through swift-argument-parser and prints `Error: <description>` with exit 1; a
//! `ValidationError` prints `Error: <message>` and exits 64 (`EX_USAGE`; argument-parser's
//! convention, not read from Tart's source, so it is marked unverified).
//!
//! Known gaps against Tart 2.41.1, none of which the adapter relies on (it reads only exit codes
//! and the "does not exist" text of exit 2):
//! - a manifest layout refusal prints `Error: <message>`; real Tart throws
//!   `OCIManifestValidationError`, whose text is Swift's rendering of the enum case
//!   (`invalidLayout("…")`), not read from a binary;
//! - a `ValidationError` prints one line; swift-argument-parser also prints the usage text;
//! - `set --mac` conflicts (`--random-mac` with `--mac`, a non-unicast MAC) are refused while
//!   parsing, before the VM is looked up; real Tart opens the VM first, so a missing VM there
//!   answers exit 2 instead;
//! - `pull` does not collect the cache after it links a name (`VMStorageOCI.link` runs `gc`);
//!   the fake collects only after a `delete`.
//!
//! Exit 99 is the fake's own: its refusals and its configuration errors, a code real Tart never
//! uses, so a test can tell "the fake refused" from "Tart said no".

/// Exit code of swift-argument-parser's `ValidationError` (unverified against a real binary).
pub(crate) const EXIT_USAGE: i32 = 64;

/// Exit code reserved for the fake's own refusals and configuration errors.
pub(crate) const EXIT_FAKE: i32 = 99;

/// A failed command: exit code plus the full stderr text (no trailing newline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fail {
    pub code: i32,
    pub message: String,
}

impl Fail {
    pub fn does_not_exist(name: &str) -> Self {
        Self {
            code: 2,
            message: format!("the specified VM \"{name}\" does not exist"),
        }
    }

    pub fn not_running(name: &str) -> Self {
        Self {
            code: 2,
            message: format!("VM \"{name}\" is not running"),
        }
    }

    pub fn already_running(name: &str) -> Self {
        Self {
            code: 2,
            message: format!("VM \"{name}\" is already running!"),
        }
    }

    pub fn is_running(name: &str) -> Self {
        Self::runtime(&format!("VM \"{name}\" is running"))
    }

    /// A `RuntimeError` without an exit code of its own: bare, exit 1.
    pub fn runtime(message: &str) -> Self {
        Self {
            code: 1,
            message: message.to_owned(),
        }
    }

    pub fn validation(message: &str) -> Self {
        Self {
            code: EXIT_USAGE,
            message: format!("Error: {message}"),
        }
    }

    pub fn generic(message: &str) -> Self {
        Self {
            code: 1,
            message: format!("Error: {message}"),
        }
    }

    pub fn fake(message: &str) -> Self {
        Self {
            code: EXIT_FAKE,
            message: format!("mayfly-fake-tart: {message}"),
        }
    }
}
