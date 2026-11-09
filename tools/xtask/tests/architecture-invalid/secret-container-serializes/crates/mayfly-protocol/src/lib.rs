//! Fixture: a protocol message holds the JIT config itself and derives Serialize, so the
//! secret would cross the link and land in logs (D13).

#[derive(serde::Serialize)]
pub struct Launch {
    pub jit: mayfly_domain::Secret<String>,
}
