//! [`ConfigDetails`]: what a directive needs beyond its attempt's stream (CP-02's
//! `DirectiveDetails`), from the configuration and the image placements.
//!
//! - **Run options** from each pool's `memory_mib` and the host's `clone_disk_gib`
//!   when configured (else no disk: the data plane counts a clone at its base's manifest disk,
//!   ADR 0052's amendment); a pool the configuration does not name gets none, and the sink
//!   holds its directive.
//! - **Stop grace**: [`STOP_GRACE`] for every pool until the configuration carries P6's
//!   per-guest-OS deadlines.
//! - **The guest challenge** of a `VerifyGuest`: the first 128 bits of SHA-256 over a key drawn
//!   once per process and the directive id, so every resend of one directive carries the same
//!   nonce (the data plane executes a repeat once) and nothing outside the process can predict
//!   one. A restart draws a new key, so a directive resent under the next run's epoch gets
//!   another nonce: it is a new execution there anyway (ADR 0042).
//! - **The image base** a clone is made from: [`Placements`], which the daemon replaces after
//!   every pass of the image placement manager with the bases materialised on this host
//!   (`mayfly_control::placement`, CP-07). An image with no materialised base has none, and its
//!   clone is held `image_base_unknown`, never sent to a guessed base (ADR 0039).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::delivery::DirectiveDetails;
use mayfly_domain::{DirectiveId, ImageDigest, Millis, PoolName, Resources};
use mayfly_protocol::{BaseName, Nonce, RunOptions};
use sha2::{Digest as _, Sha256};

use crate::config::Config;

/// How long a guest may take to shut down on a stop, for every pool.
pub const STOP_GRACE: Millis = Millis::new(30_000);

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// The bases materialised on this host, by the image digest they were made from; clones share
/// it.
#[derive(Debug, Clone, Default)]
pub struct Placements(Arc<Mutex<BTreeMap<ImageDigest, BaseName>>>);

impl Placements {
    /// Records that `base` is the materialised base of `digest` on this host.
    pub fn set(&self, digest: ImageDigest, base: BaseName) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(digest, base);
    }

    /// Replaces every base with `bases`: what the placement manager's last pass read as
    /// materialised on this host (CP-07), so an evicted base is forgotten. A name that is not a
    /// valid `mfb-*` base name is left out, never sent to a clone (ADR 0039). Pinned by
    /// `tests::should_forget_an_evicted_base_when_the_placements_are_replaced`.
    pub fn replace<'n>(&self, bases: impl IntoIterator<Item = (ImageDigest, &'n str)>) {
        let fresh: BTreeMap<ImageDigest, BaseName> = bases
            .into_iter()
            .filter_map(|(digest, name)| Some((digest, BaseName::new(name).ok()?)))
            .collect();
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = fresh;
    }

    fn get(&self, digest: &ImageDigest) -> Option<BaseName> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(digest)
            .cloned()
    }
}

/// The directive details of this daemon.
#[derive(Debug, Clone)]
pub struct ConfigDetails {
    runs: BTreeMap<PoolName, RunOptions>,
    key: [u8; 32],
    placements: Placements,
}

impl ConfigDetails {
    /// The details of `config`'s pools, each clone given `clone_disk_gib` when configured (none
    /// when measured: the data plane counts a clone at its base's manifest disk), challenges
    /// keyed by `key`, bases from `placements`.
    #[must_use]
    pub fn new(
        config: &Config,
        clone_disk_gib: Option<u64>,
        key: [u8; 32],
        placements: Placements,
    ) -> Self {
        let disk = clone_disk_gib.map_or(0, |gib| gib.saturating_mul(GIB));
        let runs = config
            .pools
            .iter()
            .map(|pool| {
                (
                    pool.name.clone(),
                    RunOptions {
                        resources: Resources::new(pool.memory_mib.saturating_mul(MIB), disk),
                    },
                )
            })
            .collect();
        Self {
            runs,
            key,
            placements,
        }
    }
}

impl DirectiveDetails for ConfigDetails {
    fn image_base(&self, _pool: &PoolName, digest: &ImageDigest) -> Option<BaseName> {
        self.placements.get(digest)
    }

    fn run_options(&self, pool: &PoolName) -> Option<RunOptions> {
        self.runs.get(pool).copied()
    }

    fn stop_grace(&self, _pool: &PoolName) -> Millis {
        STOP_GRACE
    }

    /// Pinned by `tests::should_repeat_the_nonce_when_one_directive_is_resent`.
    fn nonce(&self, directive_id: DirectiveId) -> Nonce {
        let mut hasher = Sha256::new();
        hasher.update(b"mayfly-guest-nonce-v1\n");
        hasher.update(self.key);
        hasher.update(directive_id.as_u128().to_be_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        let mut head = [0_u8; 16];
        head.copy_from_slice(&digest[..16]);
        Nonce::from_u128(u128::from_be_bytes(head))
    }
}

#[cfg(test)]
mod tests {
    use mayfly_domain::Sample as _;

    use super::*;

    fn config() -> Config {
        Config::parse(
            r#"
schema_version = 1
state_dir = "/var/lib/mayfly"
[github]
organisation = "example-org"
app_client_id = "Iv1.0123456789abcdef"
installation_id = 1
private_key = "file:/etc/mayfly/app-key.pem"
[provider]
tart_home = "/var/lib/mayfly/tart"
[[pools]]
name = "mac"
tier = "trusted"
image_manifest = "/etc/mayfly/images/mac.toml"
guest_os = "macos"
cpu = 4
memory_mib = 8192
max_running = 2
"#,
        )
        .unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn should_repeat_the_nonce_when_one_directive_is_resent() {
        // given
        let details = ConfigDetails::new(&config(), Some(90), [7; 32], Placements::default());
        let other = ConfigDetails::new(&config(), Some(90), [8; 32], Placements::default());
        let id = DirectiveId::from_u128(42);

        // when
        let (first, again) = (details.nonce(id), details.nonce(id));

        // then
        assert_eq!(first, again);
        assert_ne!(first, details.nonce(DirectiveId::from_u128(43)));
        assert_ne!(first, other.nonce(id));
    }

    #[test]
    fn should_size_the_run_from_the_pool_when_a_vm_starts() {
        // given
        let details = ConfigDetails::new(&config(), Some(90), [7; 32], Placements::default());
        let pool = PoolName::new("mac").unwrap_or_else(|e| panic!("{e}"));

        // when
        let run = details.run_options(&pool);

        // then
        assert_eq!(
            run,
            Some(RunOptions {
                resources: Resources::new(8192 * MIB, 90 * GIB)
            })
        );
        assert_eq!(
            details.run_options(&PoolName::new("other").unwrap_or_else(|e| panic!("{e}"))),
            None
        );
    }

    #[test]
    fn should_know_no_base_when_no_placement_names_one_yet() {
        // given
        let placements = Placements::default();
        let details = ConfigDetails::new(&config(), Some(90), [7; 32], placements.clone());
        let pool = PoolName::new("mac").unwrap_or_else(|e| panic!("{e}"));
        let before = details.image_base(&pool, &ImageDigest::sample());

        // when
        let base = BaseName::new("mfb-base").unwrap_or_else(|e| panic!("{e}"));
        placements.set(ImageDigest::sample(), base.clone());

        // then
        assert_eq!(before, None);
        assert_eq!(
            details.image_base(&pool, &ImageDigest::sample()),
            Some(base)
        );
    }

    #[test]
    fn should_forget_an_evicted_base_when_the_placements_are_replaced() {
        // given
        let placements = Placements::default();
        let details = ConfigDetails::new(&config(), Some(90), [7; 32], placements.clone());
        let pool = PoolName::new("mac").unwrap_or_else(|e| panic!("{e}"));
        placements.replace([(ImageDigest::sample(), "mfb-000000000000-r2-330-0")]);
        let placed = details.image_base(&pool, &ImageDigest::sample());

        // when: the base is evicted; a malformed name is never a base
        placements.replace([(ImageDigest::sample(), "ghcr.io/example/image:latest")]);

        // then
        assert_eq!(
            placed.map(|b| b.to_string()),
            Some("mfb-000000000000-r2-330-0".to_owned())
        );
        assert_eq!(details.image_base(&pool, &ImageDigest::sample()), None);
    }
}
