//! The `tart` stub binary of `mayfly-fake-tart` (test plane): prints a version line until the fake lands.

fn main() {
    println!(
        "{}",
        mayfly_fake_tart::version_line("tart", env!("CARGO_PKG_VERSION"))
    );
}
