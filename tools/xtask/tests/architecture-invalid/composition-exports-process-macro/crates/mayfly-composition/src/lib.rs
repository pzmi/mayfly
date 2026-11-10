//! Fixture: composition, which may spawn processes, exports a macro that spawns one.

#[macro_export]
macro_rules! shell {
    () => {
        std::process::Command::new("sh").env_clear().status()
    };
}
