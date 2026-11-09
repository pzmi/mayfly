//! Fixture: an event payload flattens another struct into itself, which needs a
//! self-describing format the shape tracer does not offer.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LampLit {
    pub watts: u32,
    #[serde(flatten)]
    pub place: Place,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Place {
    pub room: String,
}
