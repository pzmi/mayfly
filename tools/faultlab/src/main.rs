//! The `faultlab` binary (tool plane): prints its version until the harness lands.

fn main() {
    println!(
        "{}",
        mayfly_faultlab::version_line("faultlab", env!("CARGO_PKG_VERSION"))
    );
}
