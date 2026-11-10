//! `mayfly-conformance <manifest>...`: replays every trace of each manifest
//! ([`mayfly_conformance::run::report`]). `cargo xtask conformance` and the `conformance-replay`
//! stage run it; nothing else does.

use std::process::ExitCode;

fn main() -> ExitCode {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(mayfly_conformance::run::report(
        &paths,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ))
}
