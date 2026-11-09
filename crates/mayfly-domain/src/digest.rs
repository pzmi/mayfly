//! The pinned image identity.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::DigestError;

/// `sha256:` followed by 64 lowercase hex digits; a tag is never an identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ImageDigest(String);

impl ImageDigest {
    /// Validates the canonical form.
    ///
    /// # Errors
    /// [`DigestError`] for anything else.
    pub fn new(value: &str) -> Result<Self, DigestError> {
        match value.strip_prefix("sha256:") {
            Some(hex)
                if hex.len() == 64
                    && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) =>
            {
                Ok(Self(value.to_owned()))
            }
            _ => Err(DigestError),
        }
    }

    /// The canonical text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ImageDigest {
    type Err = DigestError;
    fn from_str(s: &str) -> Result<Self, DigestError> {
        Self::new(s)
    }
}

impl TryFrom<String> for ImageDigest {
    type Error = DigestError;
    fn try_from(s: String) -> Result<Self, DigestError> {
        Self::new(&s)
    }
}

impl From<ImageDigest> for String {
    fn from(v: ImageDigest) -> String {
        v.0
    }
}
