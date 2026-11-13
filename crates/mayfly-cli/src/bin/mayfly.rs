//! `mayfly` binary of `mayfly-cli` (shared plane): `mayfly vm-shim` runs a VM's shim (ADR 0037),
//! `mayfly image verify <manifest>` checks an image manifest (OPS-02); anything else prints the
//! version until the operator commands land.

use std::process::ExitCode;

use mayfly_cli::MayflyCommand;

fn main() -> ExitCode {
    match MayflyCommand::parse(std::env::args_os().skip(1).collect()) {
        MayflyCommand::VmShim(args) => {
            let code = mayfly_composition::vm_shim::run(&args);
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
        MayflyCommand::ImageVerify(path) => match mayfly_cli::image_verify(&path) {
            Ok(line) => {
                println!("{line}");
                ExitCode::SUCCESS
            }
            Err((code, report)) => {
                eprintln!("{report}");
                ExitCode::from(code)
            }
        },
        MayflyCommand::Usage(usage) => {
            eprintln!("{usage}");
            ExitCode::from(mayfly_cli::USAGE)
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
