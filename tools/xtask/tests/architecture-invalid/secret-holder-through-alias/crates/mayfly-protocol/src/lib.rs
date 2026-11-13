//! Fixture: a protocol message holds the JIT config through another crate's alias and
//! derives Serialize: the alias is a holder like the type it names (D13).

#[derive(serde::Serialize)]
pub struct Launch {
    pub jit: mayfly_domain::JitConfig,
}
