//! Fixture: the control plane spawns a process; only the Tart and guest adapters and
//! composition may (an exit code and test code are fine).

use std::process::ExitCode;

pub fn clone_vm() -> ExitCode {
    let _ = std::process::Command::new("tart").arg("clone").status();
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    #[test]
    fn should_find_the_shell_when_a_test_spawns_it() {
        // given
        let mut shell = std::process::Command::new("sh");

        // when
        let status = shell.arg("-c").arg("true").status();

        // then
        assert!(status.is_ok());
    }
}
