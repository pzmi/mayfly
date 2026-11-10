//! Another module of the same crate names a process API: refused.

pub fn spawn() {
    let _ = std::process::Command::new("/bin/true").env_clear().status();
}
