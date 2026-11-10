//! Fixture: a crate with process authority builds a child that inherits the daemon's
//! environment (ADR 0058); test code may.

pub fn clone_vm() {
    let _ = std::process::Command::new("tart").arg("clone").status();
}

#[cfg(test)]
mod tests {
    #[test]
    fn should_run_when_a_test_spawns_with_the_environment() {
        // given
        let mut shell = std::process::Command::new("sh");

        // when
        let status = shell.arg("-c").arg("true").status();

        // then
        assert!(status.is_ok());
    }
}
