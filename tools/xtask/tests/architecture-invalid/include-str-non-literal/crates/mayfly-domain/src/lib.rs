//! Fixture: a production crate embeds a file through a computed path, which the literal-path
//! check cannot place, so it reads another package's source unseen (third checkpoint review
//! L1).

pub const TART: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../mayfly-adapter-tart/src/lib.rs"
));
