//! Fixture: a validated value type (`ImageDigest`) decodes only through its parser but has no
//! `Sample`, so the event shape tracer cannot offer it a value it accepts. `PoolName` is
//! complete; `Generation` is integer-coded and needs neither.

use serde::{Deserialize, Serialize};

pub trait Sample {
    fn sample() -> Self;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PoolName(String);

impl Sample for PoolName {
    fn sample() -> Self {
        Self("pool0".to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ImageDigest(String);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct Generation(u32);
