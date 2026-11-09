//! Fixture: an event payload carries an enum coded as a string. The shape tracer sees only a
//! string leaf, so no golden sample would have to show its variants.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LampLit {
    pub mode: Mode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Mode {
    Strict,
    Loose,
}
