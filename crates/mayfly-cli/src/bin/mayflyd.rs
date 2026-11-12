//! `mayflyd` binary of `mayfly-cli` (shared plane): parses its arguments and runs the daemon
//! (`mayfly_composition::daemon`); every exit code is the daemon's (ADR 0031).

use std::process::ExitCode;

use mayfly_cli::{DaemonArgs, Role};

fn main() -> ExitCode {
    let args = match DaemonArgs::parse(std::env::args_os().skip(1).collect()) {
        Ok(DaemonArgs::Version) => {
            println!(
                "{}",
                mayfly_cli::version_line("mayflyd", env!("CARGO_PKG_VERSION"))
            );
            return ExitCode::SUCCESS;
        }
        Ok(DaemonArgs::Run { config, role }) => (config, role),
        Err(usage) => {
            eprintln!("mayflyd: {usage}");
            return ExitCode::from(mayfly_cli::USAGE);
        }
    };
    let (config, role) = args;
    if role != Role::All {
        eprintln!(
            "mayflyd: --role control and --role data arrive with the socket link (LNK-01); use --role all"
        );
        return ExitCode::from(mayfly_cli::USAGE);
    }
    let options = match mayfly_cli::daemon_options(
        config,
        env!("CARGO_PKG_VERSION"),
        mayfly_composition::paths::PlatformEnv::from_process(),
    ) {
        Ok(options) => options,
        Err(why) => {
            eprintln!("mayflyd: {why}");
            return ExitCode::from(mayfly_composition::exit_code::FAILED);
        }
    };
    match mayfly_composition::daemon::serve(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mayflyd: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}
