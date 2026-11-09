//! Positive fixture: the CLI delegates to composition, the only crate it names, and may return
//! an exit code (no process authority).

use std::process::ExitCode;

pub fn main_exit(root: &std::path::Path) -> ExitCode {
    ExitCode::from(mayfly_composition::run(root))
}
