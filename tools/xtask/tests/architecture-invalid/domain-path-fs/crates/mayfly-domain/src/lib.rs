//! Fixture: a pure crate reads the file system through `std::path`, and replaces the
//! process-global panic hook (second checkpoint review H2).

use std::path::Path;

pub fn present(path: &Path) -> bool {
    Path::exists(path)
}

pub fn quiet() {
    std::panic::set_hook(Box::new(|_| {}));
}
