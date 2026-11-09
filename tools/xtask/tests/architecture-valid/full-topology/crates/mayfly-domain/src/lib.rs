//! Positive fixture: domain code that names only allowed paths. An `Instant` passed in is a
//! value, not a clock read; `core::net` address types own no socket (site-preview ADR 0037); `cfg`
//! branches for every platform and test-only code are read.

mod clock;

#[cfg_attr(target_os = "macos", path = "platform_mac.rs")]
mod platform;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub use clock::{Deadline, Span};

include!("generated.rs");

macro_rules! newtype {
    ($name:ident) => {
        pub struct $name(pub u64);
    };
}

newtype!(Revision);

pub fn remaining(now: Instant, deadline: Deadline) -> Duration {
    deadline.0.saturating_duration_since(now)
}

pub fn is_loopback(addr: core::net::IpAddr) -> bool {
    addr == core::net::IpAddr::V4(core::net::Ipv4Addr::LOCALHOST)
}

pub fn index(names: &[&str]) -> BTreeMap<usize, String> {
    names.iter().map(|n| n.to_string()).enumerate().collect()
}

#[cfg(target_os = "linux")]
pub const PLATFORM: &str = "linux";

#[cfg(not(target_os = "linux"))]
pub const PLATFORM: &str = "other";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_index_names_when_given_a_list() {
        // given
        let names = ["a"];

        // when
        let indexed = index(&names);

        // then
        assert_eq!(indexed.len(), std::env::args().count().min(1));
    }

    proptest::proptest! {
        #[test]
        fn should_keep_every_name_when_list_is_indexed(count in 0usize..8) {
            // given
            let names = vec!["n"; count];

            // when
            let indexed = index(&names);

            // then
            proptest::prop_assert_eq!(indexed.len(), count);
        }
    }
}
