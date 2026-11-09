//! The `tart` binary of `mayfly-fake-tart` (test plane): the process shell around
//! [`mayfly_fake_tart::run`]. It only gathers the process's arguments, environment and streams and
//! carries out the termination `run` asks for.

use std::process::ExitCode;

use mayfly_fake_tart::{Invocation, Termination, run};

fn main() -> ExitCode {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let termination = run(Invocation {
        argv: std::env::args().skip(1).collect(),
        env: std::env::vars().collect(),
        pid: std::process::id(),
        stdin: &mut stdin.lock(),
        stdout: &mut stdout.lock(),
        stderr: &mut stderr.lock(),
    });
    match termination {
        Termination::Exit(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Termination::Hang => loop {
            std::thread::park();
        },
        Termination::Abort => std::process::abort(),
    }
}
