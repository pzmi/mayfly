//! Shared helpers for the integration tests: a fake `TART_HOME` in a tempdir and the built `tart`.
//! Waiting is done by the commands under test (`ip --wait`, blocking reads), never by a sleep here.

use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

/// A fresh state directory and the means to run the fake against it.
pub struct Fake {
    pub dir: TempDir,
}

/// The result of one finished invocation.
pub struct Ran {
    pub output: Output,
}

impl Ran {
    pub fn code(&self) -> Option<i32> {
        self.output.status.code()
    }

    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }

    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }
}

impl Fake {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    pub fn home(&self) -> &Path {
        self.dir.path()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tart"));
        command.args(args).env("TART_HOME", self.home());
        command
    }

    /// Runs `tart args` to completion with an empty stdin.
    pub fn tart(&self, args: &[&str]) -> Ran {
        let output = self
            .command(args)
            .stdin(Stdio::null())
            .output()
            .expect("run tart");
        Ran { output }
    }

    /// Runs `tart args` to completion, feeding it `stdin`.
    pub fn tart_with_stdin(&self, args: &[&str], stdin: &[u8]) -> Ran {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tart");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(stdin)
            .expect("write stdin");
        Ran {
            output: child.wait_with_output().expect("wait"),
        }
    }

    /// Starts `tart args` and leaves it running, with stdin held open and stderr piped.
    pub fn spawn(&self, args: &[&str]) -> Child {
        self.command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tart")
    }

    /// Writes `faults.toml`.
    pub fn faults(&self, toml: &str) {
        std::fs::write(self.home().join("faults.toml"), toml).expect("write faults");
    }

    /// Every record of `invocations.jsonl`.
    pub fn log(&self) -> Vec<Value> {
        let text =
            std::fs::read_to_string(self.home().join("invocations.jsonl")).unwrap_or_default();
        text.lines()
            .map(|l| serde_json::from_str(l).expect("log line is json"))
            .collect()
    }

    /// The log records of one event kind.
    pub fn events(&self, event: &str) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter(|r| r["event"] == event)
            .collect()
    }

    /// The stored record of a VM, if it exists.
    pub fn vm(&self, name: &str) -> Option<Value> {
        let path = self.home().join("vms").join(format!("{name}.json"));
        std::fs::read(path)
            .ok()
            .map(|b| serde_json::from_slice(&b).expect("vm json"))
    }

    /// Creates a base VM and a clone of it, both stopped.
    pub fn with_vm(&self, name: &str) {
        assert_eq!(self.tart(&["create", "base"]).code(), Some(0));
        assert_eq!(self.tart(&["clone", "base", name]).code(), Some(0));
    }

    /// Makes `run` return at once and leave the VM running (see the crate docs).
    pub fn detached(&self) {
        self.faults("run_mode = \"detached\"\n");
    }
}
