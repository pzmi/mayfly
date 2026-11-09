//! The managed VM name `mf-<pool>-<host>-s<slot>-g<gen>-<short>` and its strict parser (plan §8.4).
//!
//! Ownership of a VM needs the journal record, this exact name and its generation; a name that
//! does not parse is never owned, so a delete path built on [`OwnedVmName`] cannot reach it.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::VmNameError;
use crate::fence::Generation;
use crate::ids::{AttemptId, HostId, PoolName};

const PREFIX: &str = "mf-";

/// A slot number on a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Slot(u32);

impl Slot {
    /// Wraps a raw value.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The raw value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A VM name that is exactly one mayfly produced. Round trip pinned by
/// `should_round_trip_when_any_owned_name_is_formatted_and_parsed`; refusals by
/// `should_refuse_vm_name_when_it_is_malformed` and `should_not_parse_foreign_names_as_owned`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OwnedVmName {
    pool: PoolName,
    host: HostId,
    slot: Slot,
    generation: Generation,
    short: String,
}

impl OwnedVmName {
    /// The name of `attempt`'s VM in `slot` at `generation`.
    #[must_use]
    pub fn new(
        pool: PoolName,
        host: HostId,
        slot: Slot,
        generation: Generation,
        attempt: AttemptId,
    ) -> Self {
        Self {
            pool,
            host,
            slot,
            generation,
            short: attempt.short(),
        }
    }

    /// Strict parse: the canonical form only, no trimming, no case folding, no leading zeros.
    ///
    /// # Errors
    /// [`VmNameError`] naming the first part that is wrong; a name without the `mf-` prefix is
    /// [`VmNameError::NotManaged`].
    pub fn parse(name: &str) -> Result<Self, VmNameError> {
        let rest = name.strip_prefix(PREFIX).ok_or(VmNameError::NotManaged)?;
        let parts: Vec<&str> = rest.split('-').collect();
        let [pool, host, slot, generation, short] = parts.as_slice() else {
            return Err(VmNameError::WrongShape);
        };
        let pool = PoolName::new(pool).map_err(|_| VmNameError::Pool)?;
        let host = HostId::new(host).map_err(|_| VmNameError::Host)?;
        let slot = canonical_u32(slot, 's')
            .map(Slot)
            .ok_or(VmNameError::Slot)?;
        let generation = canonical_u32(generation, 'g')
            .map(Generation::new)
            .ok_or(VmNameError::Generation)?;
        if short.len() != 8
            || !short
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(VmNameError::AttemptShort);
        }
        Ok(Self {
            pool,
            host,
            slot,
            generation,
            short: (*short).to_owned(),
        })
    }

    /// The pool.
    #[must_use]
    pub const fn pool(&self) -> &PoolName {
        &self.pool
    }

    /// The host.
    #[must_use]
    pub const fn host(&self) -> &HostId {
        &self.host
    }

    /// The slot.
    #[must_use]
    pub const fn slot(&self) -> Slot {
        self.slot
    }

    /// The generation.
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    /// The eight hex digits of the attempt id.
    #[must_use]
    pub fn attempt_short(&self) -> &str {
        &self.short
    }
}

fn canonical_u32(part: &str, tag: char) -> Option<u32> {
    let digits = part.strip_prefix(tag)?;
    let canonical = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'));
    if canonical { digits.parse().ok() } else { None }
}

impl fmt::Display for OwnedVmName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{PREFIX}{}-{}-s{}-g{}-{}",
            self.pool,
            self.host,
            self.slot.0,
            self.generation.get(),
            self.short
        )
    }
}

impl FromStr for OwnedVmName {
    type Err = VmNameError;
    fn from_str(s: &str) -> Result<Self, VmNameError> {
        Self::parse(s)
    }
}

impl TryFrom<String> for OwnedVmName {
    type Error = VmNameError;
    fn try_from(s: String) -> Result<Self, VmNameError> {
        Self::parse(&s)
    }
}

impl From<OwnedVmName> for String {
    fn from(v: OwnedVmName) -> String {
        v.to_string()
    }
}

/// How an inventory entry's name relates to us, so an unknown `mf-*` VM is reported and never
/// deleted (plan §8.4). Pinned by `should_classify_names_when_inventory_lists_them`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmNameClass {
    /// A well-formed owned name (ownership still needs the journal record).
    Owned(OwnedVmName),
    /// Has the `mf-` prefix but is not a well-formed owned name: report, never delete.
    UnknownManaged,
    /// Not ours at all.
    Foreign,
}

/// Classifies a provider-reported VM name.
#[must_use]
pub fn classify_vm_name(name: &str) -> VmNameClass {
    match OwnedVmName::parse(name) {
        Ok(owned) => VmNameClass::Owned(owned),
        Err(VmNameError::NotManaged) => VmNameClass::Foreign,
        Err(_) => VmNameClass::UnknownManaged,
    }
}
