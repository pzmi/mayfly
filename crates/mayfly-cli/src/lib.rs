//! `mayfly-cli` (shared plane): Binaries `mayflyd` (the daemon) and `mayfly` (the operator CLI).

/// The line a binary prints for `--version`-style output: `<name> <version>`.
pub fn version_line(name: &str, version: &str) -> String {
    format!("{name} {version}")
}

#[cfg(test)]
mod tests {
    use super::version_line;

    #[test]
    fn should_join_name_and_version_when_formatting_the_version_line() {
        // given
        let (name, version) = ("mayfly", "1.2.3");

        // when
        let line = version_line(name, version);

        // then
        assert_eq!(line, "mayfly 1.2.3");
    }
}
