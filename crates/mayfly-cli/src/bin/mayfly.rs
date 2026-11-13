//! `mayfly` binary of `mayfly-cli` (shared plane): the operator CLI over `mayflyd`'s socket
//! (RT-03: `status`, `pool`, `host`, `vm list`, `events`, `doctor`), `mayfly config validate`
//! and `mayfly image verify` offline (OPS-02), and `mayfly vm-shim`, a VM's shim (ADR 0037).

use std::process::ExitCode;

use mayfly_cli::MayflyCommand;
use mayfly_composition::paths::PlatformEnv;

fn checked(result: Result<String, (u8, String)>) -> ExitCode {
    match result {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err((code, report)) => {
            eprintln!("{report}");
            ExitCode::from(code)
        }
    }
}

fn main() -> ExitCode {
    match MayflyCommand::parse(std::env::args_os().skip(1).collect()) {
        MayflyCommand::VmShim(args) => {
            let code = mayfly_composition::vm_shim::run(&args);
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
        MayflyCommand::ImageVerify(path) => checked(mayfly_cli::image_verify(&path)),
        MayflyCommand::ConfigValidate(path) => checked(mayfly_cli::config_validate(&path)),
        MayflyCommand::Operator {
            target,
            request,
            json,
        } => ExitCode::from(mayfly_cli::operate(
            &target,
            &request,
            json,
            &PlatformEnv::from_process(),
            &mut std::io::stdout().lock(),
            &mut std::io::stderr().lock(),
        )),
        MayflyCommand::Doctor { target, json } => ExitCode::from(mayfly_cli::doctor(
            &target,
            json,
            &PlatformEnv::from_process(),
            std::env::var_os("PATH").as_deref(),
            &mut std::io::stdout().lock(),
            &mut std::io::stderr().lock(),
        )),
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
