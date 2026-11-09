//! Identifiers. Ids are supplied by the caller through a port: the domain generates none.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::IdError;

fn valid_name(s: &str) -> bool {
    (1..=16).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

macro_rules! name_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validates `[a-z0-9]{1,16}`, so the `-` separators of a VM name stay unambiguous.
            ///
            /// # Errors
            /// [`IdError::InvalidName`] for anything else.
            pub fn new(value: &str) -> Result<Self, IdError> {
                if valid_name(value) {
                    Ok(Self(value.to_owned()))
                } else {
                    Err(IdError::InvalidName)
                }
            }

            /// The validated text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, IdError> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;
            fn try_from(s: String) -> Result<Self, IdError> {
                if valid_name(&s) { Ok(Self(s)) } else { Err(IdError::InvalidName) }
            }
        }

        impl From<$name> for String {
            fn from(v: $name) -> String {
                v.0
            }
        }
    };
}

name_type!(
    /// A pool name, `[a-z0-9]{1,16}` (plan §8.4); pinned by
    /// `should_refuse_pool_name_when_it_is_malformed`.
    PoolName
);
name_type!(
    /// A host id, `[a-z0-9]{1,16}` (plan §8.4); pinned by
    /// `should_refuse_host_id_when_it_is_malformed`.
    HostId
);

macro_rules! hex_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(u128);

        impl $name {
            /// Wraps a 128-bit value the caller's id port produced.
            #[must_use]
            pub const fn from_u128(value: u128) -> Self {
                Self(value)
            }

            /// The 128-bit value.
            #[must_use]
            pub const fn as_u128(self) -> u128 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:032x}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, IdError> {
                if s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                    u128::from_str_radix(s, 16).map(Self).map_err(|_| IdError::InvalidHexId)
                } else {
                    Err(IdError::InvalidHexId)
                }
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;
            fn try_from(s: String) -> Result<Self, IdError> {
                s.parse()
            }
        }

        impl From<$name> for String {
            fn from(v: $name) -> String {
                v.to_string()
            }
        }
    };
}

hex_id!(
    /// One attempt to run one job on one VM. Its first eight hex digits are the VM name's
    /// short part ([`AttemptId::short`]).
    AttemptId
);
hex_id!(
    /// A control-plane directive; the data plane executes each at most once per epoch.
    DirectiveId
);
hex_id!(
    /// A data-plane observation.
    ObservationId
);
hex_id!(
    /// A command, the key of its receipt (idempotency).
    CommandId
);

impl AttemptId {
    /// The eight lowercase hex digits that end an [`crate::OwnedVmName`]: the id's top 32 bits.
    #[must_use]
    pub fn short(self) -> String {
        format!("{:08x}", self.0 >> 96)
    }
}
