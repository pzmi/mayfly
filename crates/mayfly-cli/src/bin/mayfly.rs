//! `mayfly` binary of `mayfly-cli` (shared plane): prints its version until the real entry point lands.

fn main() {
    println!(
        "{}",
        mayfly_cli::version_line("mayfly", env!("CARGO_PKG_VERSION"))
    );
}
