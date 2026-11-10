//! `mayfly-cli` (shared plane): Binaries `mayflyd` (the daemon) and `mayfly` (the operator CLI).
//!
//! The binaries parse and delegate to composition; nothing here decides anything.

use std::ffi::OsString;

/// The line a binary prints for `--version`-style output: `<name> <version>`.
pub fn version_line(name: &str, version: &str) -> String {
    format!("{name} {version}")
}

/// What `mayfly` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MayflyCommand {
    /// `mayfly vm-shim <args>`: run as a VM's shim (ADR 0037), the arguments after `vm-shim`.
    VmShim(Vec<OsString>),
    /// Anything else, for now: print the version.
    Version,
}

impl MayflyCommand {
    /// Reads the arguments after the program name.
    #[must_use]
    pub fn parse(args: Vec<OsString>) -> Self {
        let mut args = args.into_iter();
        match args.next() {
            Some(first) if first == "vm-shim" => Self::VmShim(args.collect()),
            _ => Self::Version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MayflyCommand, version_line};

    #[test]
    fn should_join_name_and_version_when_formatting_the_version_line() {
        // given
        let (name, version) = ("mayfly", "1.2.3");

        // when
        let line = version_line(name, version);

        // then
        assert_eq!(line, "mayfly 1.2.3");
    }

    #[test]
    fn should_pass_the_rest_through_when_the_command_is_vm_shim() {
        // given
        let args = vec!["vm-shim".into(), "--vm".into(), "x".into()];

        // when
        let command = MayflyCommand::parse(args);

        // then
        assert_eq!(
            command,
            MayflyCommand::VmShim(vec!["--vm".into(), "x".into()])
        );
        assert_eq!(MayflyCommand::parse(Vec::new()), MayflyCommand::Version);
    }
}
