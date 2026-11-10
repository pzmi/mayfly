//! The one runner: process authority by `PROCESS_MODULES`.

pub fn run() {
    let _ = std::process::Command::new("/bin/true").env_clear().status();
}
