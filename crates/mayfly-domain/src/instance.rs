//! The provider's identity of one VM incarnation (ADR 0026).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::IdError;
use crate::sample::Sample;

/// The provider's identity of one VM incarnation, recorded when the clone is observed: for
/// Tart, the random MAC address set on every clone (`tart set --random-mac`, ADR 0026). It
/// tells "our VM" from "another VM with the same name": a VM is owned only while the observed
/// instance id equals the recorded one.
///
/// Opaque to the domain, compared only for equality, and validated as 1 to 64 characters of
/// `[0-9a-z:._-]` so a value read from the provider can never carry a separator, a path or a
/// shell metacharacter into a log or an argv. Deserialises only through [`ProviderInstanceId::new`]
/// (pinned by `should_refuse_instance_id_when_it_is_malformed`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProviderInstanceId(String);

fn valid_instance(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b":._-".contains(&b))
}

impl ProviderInstanceId {
    /// Validates `[0-9a-z:._-]{1,64}`.
    ///
    /// # Errors
    /// [`IdError::InvalidInstanceId`] for anything else.
    pub fn new(value: &str) -> Result<Self, IdError> {
        if valid_instance(value) {
            Ok(Self(value.to_owned()))
        } else {
            Err(IdError::InvalidInstanceId)
        }
    }

    /// The validated text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderInstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ProviderInstanceId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        Self::new(s)
    }
}

impl TryFrom<String> for ProviderInstanceId {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, IdError> {
        if valid_instance(&s) {
            Ok(Self(s))
        } else {
            Err(IdError::InvalidInstanceId)
        }
    }
}

impl From<ProviderInstanceId> for String {
    fn from(v: ProviderInstanceId) -> String {
        v.0
    }
}

impl Sample for ProviderInstanceId {
    fn sample() -> Self {
        Self("0a:00:00:00:00:01".to_owned())
    }
}
