//! Fixture: a protocol message whose generic parameter defaults to a holder derives
//! Serialize: `Launch` with no argument holds the JIT config (D13).

#[derive(serde::Serialize)]
pub struct Launch<J = mayfly_domain::Jit> {
    pub jits: Vec<J>,
}
