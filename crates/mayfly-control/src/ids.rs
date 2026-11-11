//! Derived ids: the first 128 bits of SHA-256 over a domain tag and length-prefixed parts, so a
//! use case that runs again over the same inputs issues the same command ids and is answered by
//! its receipts, and two inputs whose parts concatenate alike never share an id.

use sha2::{Digest, Sha256};

/// The id derived from `domain` and `parts`.
pub(crate) fn derive(domain: &[u8], parts: &[&[u8]]) -> u128 {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for part in parts {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let mut head = [0_u8; 16];
    head.copy_from_slice(&digest[..16]);
    u128::from_be_bytes(head)
}

#[cfg(test)]
mod tests {
    use super::derive;

    #[test]
    fn should_differ_when_parts_concatenate_alike() {
        // given
        let domain = b"test\n";

        // when
        let split = derive(domain, &[b"ab", b"c"]);
        let joined = derive(domain, &[b"a", b"bc"]);

        // then
        assert_ne!(split, joined);
        assert_eq!(split, derive(domain, &[b"ab", b"c"]));
    }
}
