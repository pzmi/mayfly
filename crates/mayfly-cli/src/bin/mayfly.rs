//! `mayfly` binary of `mayfly-cli` (shared plane): `mayfly vm-shim` runs a VM's shim (ADR 0037);
//! anything else prints the version until the operator commands land.

use std::process::ExitCode;

use mayfly_cli::MayflyCommand;

fn main() -> ExitCode {
    match MayflyCommand::parse(std::env::args_os().skip(1).collect()) {
        MayflyCommand::VmShim(args) => {
            let code = mayfly_composition::vm_shim::run(&args);
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
        MayflyCommand::Version => {
            println!(
                "{}",
                mayfly_cli::version_line("mayfly", env!("CARGO_PKG_VERSION"))
            );
            ExitCode::SUCCESS
        }
    }
}
