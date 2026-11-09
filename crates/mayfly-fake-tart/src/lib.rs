//! `mayfly-fake-tart` (test plane): Test-only: a `tart`-compatible CLI over a state directory, with fault knobs.

/// The line the stub `tart` binary prints for its version.
pub fn version_line(name: &str, version: &str) -> String {
    format!("{name} {version}")
}

#[cfg(test)]
mod tests {
    use super::version_line;

    #[test]
    fn should_join_name_and_version_when_formatting_the_version_line() {
        // given
        let (name, version) = ("tart", "1.2.3");

        // when
        let line = version_line(name, version);

        // then
        assert_eq!(line, "tart 1.2.3");
    }
}
