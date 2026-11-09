//! Fixture: an event payload field accepts a second name. A sample showing the alias would
//! re-encode under the primary name, and a rename would pass as an alias.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LampLit {
    #[serde(alias = "power")]
    pub watts: u32,
}
