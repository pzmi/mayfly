//! `mayfly-faultlab` (tool plane): Jepsen-style harness with controller, history, nemesis and checker;
//! it imports no production crate, so it stays an independent oracle.

/// The line the stub `faultlab` binary prints for its version.
pub fn version_line(name: &str, version: &str) -> String {
    format!("{name} {version}")
}

#[cfg(test)]
mod tests {
    use super::version_line;

    #[test]
    fn should_join_name_and_version_when_formatting_the_version_line() {
        // given
        let (name, version) = ("faultlab", "1.2.3");

        // when
        let line = version_line(name, version);

        // then
        assert_eq!(line, "faultlab 1.2.3");
    }
}
