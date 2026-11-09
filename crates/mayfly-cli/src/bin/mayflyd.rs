//! `mayflyd` binary of `mayfly-cli` (shared plane): prints its version until the real entry point lands.

fn main() {
    println!(
        "{}",
        mayfly_cli::version_line("mayflyd", env!("CARGO_PKG_VERSION"))
    );
}
