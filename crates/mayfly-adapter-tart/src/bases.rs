//! The registry of the image bases this deployment made: what makes a base "service-owned"
//! (ADR 0039 decision 1, ADR 0087).
//!
//! One JSON file in a private directory of the daemon's state (`0700`, checked at start-up by
//! [`crate::verify::check_private`]), written `0600` and durably: a temporary file, `fsync`,
//! `rename`, then the directory's `fsync`, so a reader sees the old record or the new one. The raw
//! manifest of each verified image is kept beside it (`manifests/`), checked against its digest
//! whenever it is read.
//!
//! **Fails closed.** A record that does not parse, is too large or of another version is
//! [`Unreadable`]: every clone, materialisation, eviction and listing is refused. A missing record
//! is an empty one: nothing is owned, so nothing is cloned from or deleted, and a base on disk is
//! never rediscovered by its name.
//!
//! **Write-ahead.** A base's entry moves through [`BaseState`], each state written before the
//! step it permits; a job clone's name is written as an unresolved intent before `tart clone`.
//! A crash leaves the record naming more than exists, never less, and recovery reads what exists
//! (`images::TartImageStore::recover`).
//!
//! **One writer.** [`BaseRecords::update`] serialises writes; [`BaseRecords::admit`] serialises
//! clone admission against eviction, so an eviction never reads the references while a clone is
//! being added; [`BaseRecords::hold`] keeps one materialisation per base. Lock order: hold, then
//! admit. Pinned by `tests::should_read_back_the_record_when_it_was_written`,
//! `tests::should_refuse_a_record_when_it_does_not_parse_or_has_another_version` and the fault
//! cases of `tests/tart/images.rs`.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use mayfly_domain::ImageDigest;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::oci::matches;
use crate::verify::{FileStamp, Fingerprint, Identity, MANIFEST_LIMIT};

/// The record's schema version.
pub const VERSION: u32 = 1;

/// The version of the build recipe (clone, own MAC, verify, boot once, install, stop,
/// fingerprint) a base was made by; a base of another recipe is never cloned from.
pub const RECIPE_VERSION: u32 = 1;

/// The most bytes the record may have; more is [`Unreadable`].
const LIMIT: u64 = 4 << 20;

/// Where a base is (ADR 0087's state machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseState {
    /// The name is reserved; Tart does not list it (checked on a complete listing).
    Reserved,
    /// `tart clone` may have run; nothing proves what is there.
    Cloning,
    /// Cloned, its own MAC and file identity recorded, its content verified or being verified,
    /// booted for the runner install.
    Customizing,
    /// Stopped after the install; the runner check and the fingerprint follow.
    Verifying,
    /// Stopped, its runner checked, its final fingerprint recorded: the only state clones are
    /// made from, after revalidation.
    Ready,
    /// Its deletion was decided; an observed complete absence settles it.
    Retiring,
    /// Unproven or changed: never cloned from and never deleted by mayfly, whatever its
    /// identity (pinned by
    /// `tests/tart/images.rs::should_never_evict_a_quarantined_base_when_its_identity_still_holds`);
    /// only an operator resolves it.
    Quarantined,
}

/// The signature policy of a source (ADR 0087): the digest is always required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignaturePolicy {
    /// The pinned digest and full content verification only (the default).
    DigestOnly,
    /// A signature is checked when there is one; a source without one is accepted.
    Optional,
    /// A valid signature from the configured trust anchor is required.
    Required,
}

/// What the signature check found, recorded next to the source digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureResult {
    /// Not checked: the policy is digest-only, or no verifier is built yet (SIG-01).
    NotChecked,
}

/// A source's signature policy and its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// The policy in force when it was pulled.
    pub policy: SignaturePolicy,
    /// What the check found.
    pub result: SignatureResult,
}

/// One base this deployment made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseEntry {
    /// A random id, distinct from the name and the MAC: a base rebuilt under the same name is
    /// another base.
    pub base_id: String,
    /// The deployment that made it.
    pub deployment_id: String,
    /// `<repository>@<digest>`.
    pub source_ref: String,
    /// The full digest; the twelve digits in the name are only a name, and another digest with
    /// the same prefix is refused.
    pub source_digest: ImageDigest,
    /// `tart --version` when it was built.
    pub tart_version: String,
    /// The runner version installed.
    pub runner_version: String,
    /// The pinned SHA-256 of that runner's package.
    pub runner_sha256: String,
    /// [`RECIPE_VERSION`] when it was built.
    pub build_recipe_version: u32,
    /// Where it is.
    pub state: BaseState,
    /// Why it is quarantined, when it is.
    #[serde(default)]
    pub quarantine: Option<String>,
    /// Its own MAC, a secondary check (a MAC can be copied or set).
    #[serde(default)]
    pub vm_mac: Option<String>,
    /// Its directory's and disk's identity, recorded right after its MAC.
    #[serde(default)]
    pub vm_file_identity: Option<Identity>,
    /// Its files' stamps once `Ready`.
    #[serde(default)]
    pub final_fingerprint: Option<Fingerprint>,
    /// The source's signature policy and result.
    pub signature: Signature,
    /// Job clones made from it, by name: their instance id once known, `None` while the clone
    /// is an unresolved intent. Each blocks eviction until Tart no longer lists it.
    #[serde(default)]
    pub clones: BTreeMap<String, Option<String>>,
    /// The disk ceiling Tart printed for each job clone right after it was made, while it was
    /// stopped, by name and with the instance id it was read for (TART-01): the inventory's
    /// value when a later `tart get` cannot read the disk (`Disk` null) for that same
    /// incarnation.
    #[serde(default)]
    pub clone_disks: BTreeMap<String, CloneDisk>,
}

/// A job clone's disk ceiling as read when it was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneDisk {
    /// The instance id (MAC) of the incarnation it was read for.
    pub instance_id: String,
    /// The ceiling, in bytes.
    pub disk_bytes: u64,
}

impl Record {
    /// The disk ceiling recorded for the clone `name` when it was made, if `instance_id` is
    /// still the incarnation it was read for. Pinned by
    /// `tests/tart/calls.rs::should_count_the_disk_recorded_at_clone_time_when_tart_prints_none`.
    #[must_use]
    pub fn clone_disk(&self, name: &str, instance_id: &str) -> Option<u64> {
        self.bases
            .values()
            .filter_map(|entry| entry.clone_disks.get(name))
            .find(|disk| disk.instance_id == instance_id)
            .map(|disk| disk.disk_bytes)
    }
}

/// One verified image in Tart's OCI cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageEntry {
    /// `<repository>@<digest>`.
    pub source_ref: String,
    /// The cache entry's files as verified: a change makes it unverified again.
    pub files: [FileStamp; 3],
    /// The signature policy and result.
    pub signature: Signature,
}

/// The whole record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Schema version, [`VERSION`].
    pub v: u32,
    /// The bases, by name.
    pub bases: BTreeMap<String, BaseEntry>,
    /// The verified images, by digest.
    #[serde(default)]
    pub images: BTreeMap<ImageDigest, ImageEntry>,
}

impl Default for Record {
    fn default() -> Self {
        Self {
            v: VERSION,
            bases: BTreeMap::new(),
            images: BTreeMap::new(),
        }
    }
}

/// The record could not be read or written: callers refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unreadable;

/// The record file and the locks around it, shared by the provider and the image store.
#[derive(Debug)]
pub struct BaseRecords {
    path: PathBuf,
    write: Mutex<()>,
    admission: Arc<AsyncMutex<()>>,
    held: Mutex<BTreeMap<String, Arc<AsyncMutex<()>>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A fresh random id (128 bits from the operating system's generator, lowercase hex).
///
/// # Errors
/// The generator could not be read.
pub fn random_id() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

impl BaseRecords {
    /// The record kept at `path` (absolute, in a private directory).
    #[must_use]
    pub fn open(path: PathBuf) -> Self {
        Self {
            path,
            write: Mutex::new(()),
            admission: Arc::default(),
            held: Mutex::new(BTreeMap::new()),
        }
    }

    /// Where it is kept.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Its directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("/"))
    }

    /// Waits until no other effect holds `base`, then holds it until the guard drops.
    pub async fn hold(&self, base: &str) -> OwnedMutexGuard<()> {
        let mutex = Arc::clone(lock(&self.held).entry(base.to_owned()).or_default());
        mutex.lock_owned().await
    }

    /// The single admission lock: clone admission and eviction never run at once.
    pub async fn admit(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.admission).lock_owned().await
    }

    /// The record as it is on disk; empty when there is none.
    ///
    /// # Errors
    /// [`Unreadable`]: it cannot be read, is too large, does not parse or is of another version.
    pub fn read(&self) -> Result<Record, Unreadable> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Record::default()),
            Err(_) => return Err(Unreadable),
        };
        let mut bytes = Vec::new();
        file.take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Unreadable)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > LIMIT {
            return Err(Unreadable);
        }
        let record: Record = serde_json::from_slice(&bytes).map_err(|_| Unreadable)?;
        if record.v == VERSION {
            Ok(record)
        } else {
            Err(Unreadable)
        }
    }

    /// Reads the record, applies `change` and writes it back durably, one writer at a time.
    ///
    /// # Errors
    /// [`Unreadable`]: it could not be read or written; nothing was changed on disk.
    pub fn update<T>(&self, change: impl FnOnce(&mut Record) -> T) -> Result<T, Unreadable> {
        let _writer = lock(&self.write);
        let mut record = self.read()?;
        let out = change(&mut record);
        let bytes = serde_json::to_vec_pretty(&record).map_err(|_| Unreadable)?;
        write_durably(&self.path, &bytes).map_err(|_| Unreadable)?;
        Ok(out)
    }

    fn manifest_path(&self, digest: &ImageDigest) -> PathBuf {
        let name = digest.as_str().replace(':', "-");
        self.dir().join("manifests").join(format!("{name}.json"))
    }

    /// Keeps `raw`, the verified manifest of `digest`.
    ///
    /// # Errors
    /// [`Unreadable`]: it does not match the digest, or could not be written.
    pub fn keep_manifest(&self, digest: &ImageDigest, raw: &[u8]) -> Result<(), Unreadable> {
        if !matches(raw, digest) {
            return Err(Unreadable);
        }
        write_durably(&self.manifest_path(digest), raw).map_err(|_| Unreadable)
    }

    /// The kept manifest of `digest`, only when its bytes still hash to it.
    #[must_use]
    pub fn manifest(&self, digest: &ImageDigest) -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        File::open(self.manifest_path(digest))
            .ok()?
            .take(u64::try_from(MANIFEST_LIMIT).unwrap_or(u64::MAX) + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        matches(&bytes, digest).then_some(bytes)
    }
}

/// Writes `bytes` to a temporary `0600` file beside `path`, syncs it, renames it over `path`
/// and syncs the directory (created `0700` when missing).
fn write_durably(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::from(ErrorKind::InvalidInput))?;
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| std::io::Error::from(ErrorKind::InvalidInput))?;
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
        .and_then(|()| fs::rename(&tmp, path))
        .and_then(|()| File::open(dir)?.sync_all());
    if written.is_err() {
        let _removed = fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use mayfly_domain::Sample;

    use super::*;

    fn entry() -> BaseEntry {
        BaseEntry {
            base_id: "00".repeat(16),
            deployment_id: "deployment".to_owned(),
            source_ref: format!("registry.example/a@{}", ImageDigest::sample()),
            source_digest: ImageDigest::sample(),
            tart_version: "2.41.1".to_owned(),
            runner_version: "2.330.0".to_owned(),
            runner_sha256: "5a".repeat(32),
            build_recipe_version: RECIPE_VERSION,
            state: BaseState::Ready,
            quarantine: None,
            vm_mac: Some("0a:00:00:00:00:01".to_owned()),
            vm_file_identity: None,
            final_fingerprint: None,
            signature: Signature {
                policy: SignaturePolicy::DigestOnly,
                result: SignatureResult::NotChecked,
            },
            clones: BTreeMap::from([("mf-a".to_owned(), None)]),
            clone_disks: BTreeMap::new(),
        }
    }

    #[test]
    fn should_read_back_the_record_when_it_was_written() {
        // given
        let dir = tempfile::tempdir().expect("tempdir");
        let records = BaseRecords::open(dir.path().join("state/bases.json"));
        let empty = records.read();

        // when
        let written = records.update(|r| r.bases.insert("mfb-a".to_owned(), entry()));
        let reopened = BaseRecords::open(records.path().to_path_buf()).read();

        // then
        assert_eq!(empty, Ok(Record::default()));
        assert_eq!(written, Ok(None));
        let expected = Record {
            v: VERSION,
            bases: BTreeMap::from([("mfb-a".to_owned(), entry())]),
            images: BTreeMap::new(),
        };
        assert_eq!(reopened, Ok(expected));
    }

    #[test]
    fn should_refuse_a_record_when_it_does_not_parse_or_has_another_version() {
        // given
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bases.json");
        let records = BaseRecords::open(path.clone());
        let bodies: [&[u8]; 3] = [b"{not json", br#"{"v":2,"bases":{}}"#, b""];

        for body in bodies {
            fs::write(&path, body).expect("write");

            // when
            let read = records.read();
            let updated = records.update(|r| r.bases.clear());

            // then
            assert_eq!(read, Err(Unreadable));
            assert_eq!(updated, Err(Unreadable));
            assert_eq!(fs::read(&path).expect("unchanged"), body);
        }
    }

    #[test]
    fn should_keep_a_manifest_only_when_it_matches_its_digest_when_kept_and_read() {
        // given
        let dir = tempfile::tempdir().expect("tempdir");
        let records = BaseRecords::open(dir.path().join("bases.json"));
        let raw = b"{}";
        let digest =
            ImageDigest::new(&format!("sha256:{}", crate::verify::sha256_hex(raw))).expect("d");

        // when
        let other = records.keep_manifest(&ImageDigest::sample(), raw);
        let kept = records.keep_manifest(&digest, raw);
        fs::write(records.manifest_path(&digest), b"{ }").expect("tamper");
        let tampered = records.manifest(&digest);

        // then
        assert_eq!(other, Err(Unreadable));
        assert_eq!(kept, Ok(()));
        assert_eq!(tampered, None);
    }

    #[test]
    fn should_draw_distinct_ids_when_asked_twice() {
        // given
        let first = random_id().expect("an id");

        // when
        let second = random_id().expect("an id");

        // then
        assert_eq!(first.len(), 32);
        assert_ne!(first, second);
    }
}
