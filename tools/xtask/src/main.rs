//! Repository automation entry point, invoked as `cargo xtask`.
//!
//! Ported from site-preview (<https://github.com/pzmi/site-preview>, same owner, MIT OR
//! Apache-2.0; mayfly D3) and cut to the foundation: the gate (`quick`, `check`), the
//! architecture checker, the pinned-tool doctor, ADRs, `land` and the supply-chain stages.
//! Later tasks register their own stages (nextest and coverage, the formal toolchain,
//! conformance, fuzz, mutation, release); a stage that does not exist yet is absent from the
//! catalog, never a skipped row.

mod action_pins;
mod adr;
mod adr_files;
mod architecture;
mod catalog;
mod check;
mod cli;
mod land;
mod process;
mod quick;
mod repo;
mod security;
mod test;
mod tools;

use std::process::{Command as Proc, ExitCode};

use clap::Parser;
use cli::{AdrAction, Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let code = match &cli.command {
        Command::Quick => quick::run(),
        Command::Test { only } => test::run(only.as_deref()),
        Command::Check { only } => check::run(only),
        Command::Security => security::run(),
        Command::Fmt => run_fmt(),
        Command::Architecture { manifest_path } => architecture::run(manifest_path.as_deref()),
        Command::ListChecks => {
            print!("{}", catalog::to_json(&catalog::all_rows()));
            0
        }
        Command::Doctor { json } => tools::doctor(*json),
        Command::InstallTools { tools } => tools::install(tools),
        Command::Adr { action } => match action {
            AdrAction::New {
                title,
                number,
                date,
                status,
            } => adr::run_new(&adr::NewAdr {
                title,
                number: *number,
                date: date.as_deref(),
                status,
            }),
            AdrAction::Index { fix } => adr::run_index(*fix),
        },
        Command::Land {
            refs,
            push,
            also,
            no_push: _,
            log,
        } => {
            let target = match push.as_deref() {
                Some([remote, branch]) => land::Target::Push {
                    remote: remote.clone(),
                    branch: branch.clone(),
                    also: also.clone(),
                },
                _ => land::Target::NoPush,
            };
            land::run(refs, &target, log.clone())
        }
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// `cargo fmt --all`.
fn run_fmt() -> i32 {
    match Proc::new("cargo").args(["fmt", "--all"]).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!("xtask: cannot run cargo fmt: {err}");
            1
        }
    }
}
