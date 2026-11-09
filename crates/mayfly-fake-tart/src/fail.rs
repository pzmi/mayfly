//! A command's failure: the exit code and the stderr text a caller sees.
//!
//! Shapes mirror Tart's `RuntimeError` handling (Sources/tart/VMStorageHelper.swift and Root.swift
//! on the `main` branch of the public repository, read 2026-11-09): errors that carry an exit code
//! (`VMDoesNotExist`, `VMNotRunning`, `VMAlreadyRunning`) print their bare description and exit 2;
//! every other runtime error goes through swift-argument-parser and prints `Error: <description>`
//! with exit 1; a `ValidationError` prints `Error: <message>` and exits 64 (`EX_USAGE`; this one is
//! argument-parser's convention, not read from Tart's source, so it is marked unverified).
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
        Self::generic(&format!("VM \"{name}\" is running"))
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
