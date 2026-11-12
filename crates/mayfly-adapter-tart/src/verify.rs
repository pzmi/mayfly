//! What the image store checks on disk (ADR 0087): a Tart image's layout from its raw manifest,
//! the files Tart materialised against their descriptors, the per-chunk digests of a disk, a VM
//! directory's fingerprint, and the private directories the daemon runs with.
//!
//! Tart (2.41.1, `VMStorageOCI.pull`) names a pulled entry by the hash of whatever manifest it
//! was served and never compares it with the digest it was asked for, caches that manifest as
//! served (2.28.0 re-serialized it), and checks no config, NVRAM or compressed blob on a fresh
//! pull (`VMDirectory+OCI.swift`; `DiskV2` checks a chunk's uncompressed digest only when it
//! resumes). So nothing here reads the cached manifest: the layout comes from the raw manifest
//! fetched independently and checked against the digest ([`crate::oci`]), and every file is
//! checked against it.
//!
//! Pinned by the tests below and `tests/tart/images.rs` (the fault cases).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{Mode, OFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// An OCI image manifest.
pub const MANIFEST_V1: &str = "application/vnd.oci.image.manifest.v1+json";
/// Tart's VM configuration layer, stored uncompressed.
pub const CONFIG_LAYER: &str = "application/vnd.cirruslabs.tart.config.v1";
/// Tart's NVRAM layer, stored uncompressed.
pub const NVRAM_LAYER: &str = "application/vnd.cirruslabs.tart.nvram.v1";
/// Tart's chunked, compressed disk layer with per-chunk uncompressed digests.
pub const DISK_V2_LAYER: &str = "application/vnd.cirruslabs.tart.disk.v2";
/// A chunk's uncompressed size.
pub const UNCOMPRESSED_SIZE: &str = "org.cirruslabs.tart.uncompressed-size";
/// A chunk's uncompressed SHA-256.
pub const UNCOMPRESSED_DIGEST: &str = "org.cirruslabs.tart.uncompressed-content-digest";
/// The whole disk's uncompressed size, when the manifest states it.
pub const DISK_SIZE: &str = "org.cirruslabs.tart.uncompressed-disk-size";

/// The VM files Tart keeps, in `<TART_HOME>/vms/<name>/` and in an OCI cache entry.
pub const CONFIG_FILE: &str = "config.json";
/// The NVRAM file.
pub const NVRAM_FILE: &str = "nvram.bin";
/// The disk file.
pub const DISK_FILE: &str = "disk.img";
/// A suspended VM's saved state, when there is one.
pub const STATE_FILE: &str = "state.vzvmsave";

/// The most bytes of a raw manifest accepted.
pub const MANIFEST_LIMIT: usize = 4 << 20;

/// Why an image or a file was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The manifest does not parse, or is not an OCI image manifest.
    Unparseable,
    /// A layer of a media type this store does not take: disk v1 (no per-chunk digests), ASIF,
    /// or anything unknown.
    UnsupportedLayer(String),
    /// A config, NVRAM or disk layer is missing or repeated.
    Layers,
    /// A disk chunk lacks its uncompressed size or digest, or they do not add up.
    Annotations,
    /// A file is missing, a symlink, or not what its descriptor says.
    Content(&'static str),
}

/// One file's expected size and SHA-256 (lowercase hex).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    /// Size in bytes.
    pub size: u64,
    /// SHA-256, lowercase hex.
    pub sha256: String,
}

/// What a raw manifest says the image is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// `config.json`.
    pub config: Expected,
    /// `nvram.bin`.
    pub nvram: Expected,
    /// `disk.img`, chunk by chunk in manifest order (uncompressed).
    pub chunks: Vec<Expected>,
}

#[derive(Deserialize)]
struct Manifest {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
    layers: Vec<Layer>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Layer {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

fn hex_of(digest: &str) -> Option<String> {
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .then(|| hex.to_owned())
}

/// The SHA-256 of `bytes`, lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Reads the layout of a raw manifest: an OCI image manifest with exactly one config and one
/// NVRAM layer, at least one disk-v2 chunk, each chunk with its uncompressed size and digest,
/// and no other layer, in Tart's order: the config first, the chunks, the NVRAM last (Tart
/// 2.41.1's `OCIManifest.tartDiskRepresentation` refuses any other order before it pulls).
/// Pinned by `tests::should_refuse_a_manifest_when_its_layout_is_not_disk_v2`.
///
/// # Errors
/// [`Refusal`].
pub fn layout(raw: &[u8]) -> Result<Layout, Refusal> {
    let manifest: Manifest = serde_json::from_slice(raw).map_err(|_| Refusal::Unparseable)?;
    if manifest.schema_version != 2 || manifest.media_type.as_deref() != Some(MANIFEST_V1) {
        return Err(Refusal::Unparseable);
    }
    let (mut config, mut nvram, mut chunks) = (Vec::new(), Vec::new(), Vec::new());
    for layer in &manifest.layers {
        let sha256 = hex_of(&layer.digest).ok_or(Refusal::Unparseable)?;
        match layer.media_type.as_str() {
            CONFIG_LAYER => config.push(Expected {
                size: layer.size,
                sha256,
            }),
            NVRAM_LAYER => nvram.push(Expected {
                size: layer.size,
                sha256,
            }),
            DISK_V2_LAYER => {
                let size = layer
                    .annotations
                    .get(UNCOMPRESSED_SIZE)
                    .and_then(|s| s.parse().ok())
                    .ok_or(Refusal::Annotations)?;
                let sha256 = layer
                    .annotations
                    .get(UNCOMPRESSED_DIGEST)
                    .and_then(|d| hex_of(d))
                    .ok_or(Refusal::Annotations)?;
                chunks.push(Expected { size, sha256 });
            }
            other => return Err(Refusal::UnsupportedLayer(other.to_owned())),
        }
    }
    let ordered = manifest
        .layers
        .first()
        .is_some_and(|l| l.media_type == CONFIG_LAYER)
        && manifest
            .layers
            .last()
            .is_some_and(|l| l.media_type == NVRAM_LAYER);
    if config.len() != 1 || nvram.len() != 1 || chunks.is_empty() || !ordered {
        return Err(Refusal::Layers);
    }
    let total = chunks
        .iter()
        .try_fold(0u64, |sum, c| sum.checked_add(c.size))
        .ok_or(Refusal::Annotations)?;
    if let Some(stated) = manifest.annotations.get(DISK_SIZE)
        && stated.parse::<u64>().ok() != Some(total)
    {
        return Err(Refusal::Annotations);
    }
    Ok(Layout {
        config: config.remove(0),
        nvram: nvram.remove(0),
        chunks,
    })
}

/// Opens `path` as a regular file, never through a symlink.
fn regular(path: &Path, what: &'static str) -> Result<File, Refusal> {
    // O_NOFOLLOW on the open itself: no window between a check and the open in which the file
    // could become a symlink. A FIFO or device is refused by the type check on the open file.
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| Refusal::Content(what))?;
    let file = File::from(fd);
    let regular_file = file.metadata().is_ok_and(|meta| meta.file_type().is_file());
    if regular_file {
        Ok(file)
    } else {
        Err(Refusal::Content(what))
    }
}

/// Hashes the next `size` bytes of `file`; `None` when it ends first.
fn hash_next(file: &mut File, size: u64) -> Option<String> {
    let mut hasher = Sha256::new();
    let mut left = size;
    let mut buffer = vec![0u8; 1 << 20];
    while left > 0 {
        let want = usize::try_from(left.min(1 << 20)).unwrap_or(1 << 20);
        let read = file.read(&mut buffer[..want]).ok()?;
        if read == 0 {
            return None;
        }
        hasher.update(&buffer[..read]);
        left -= u64::try_from(read).unwrap_or(left);
    }
    Some(
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// Checks a whole file against `expected`.
///
/// # Errors
/// [`Refusal::Content`] naming `what`.
pub fn check_file(path: &Path, expected: &Expected, what: &'static str) -> Result<(), Refusal> {
    let mut file = regular(path, what)?;
    let size = file.metadata().map_err(|_| Refusal::Content(what))?.len();
    if size != expected.size || hash_next(&mut file, size).as_ref() != Some(&expected.sha256) {
        return Err(Refusal::Content(what));
    }
    Ok(())
}

/// Checks a disk file against its chunks: its size is their sum, and each range hashes to its
/// chunk's uncompressed digest.
///
/// # Errors
/// [`Refusal::Content`].
pub fn check_disk(path: &Path, chunks: &[Expected]) -> Result<(), Refusal> {
    let mut file = regular(path, "disk")?;
    let total: u64 = chunks.iter().map(|c| c.size).sum();
    if file.metadata().map_err(|_| Refusal::Content("disk"))?.len() != total {
        return Err(Refusal::Content("disk"));
    }
    for chunk in chunks {
        if hash_next(&mut file, chunk.size).as_ref() != Some(&chunk.sha256) {
            return Err(Refusal::Content("disk chunk"));
        }
    }
    Ok(())
}

/// Checks an image's files in `dir` against `layout`. With `clone_of`, `dir` is a fresh clone
/// and its `config.json` is compared with that verified cache entry's, every field but the MAC
/// (which `tart clone` and `tart set --random-mac` rewrite).
///
/// # Errors
/// [`Refusal::Content`].
pub fn check_image(dir: &Path, layout: &Layout, clone_of: Option<&Path>) -> Result<(), Refusal> {
    match clone_of {
        None => check_file(&dir.join(CONFIG_FILE), &layout.config, "config")?,
        Some(cache) => {
            check_file(&cache.join(CONFIG_FILE), &layout.config, "config")?;
            if config_without_mac(&dir.join(CONFIG_FILE))?
                != config_without_mac(&cache.join(CONFIG_FILE))?
            {
                return Err(Refusal::Content("config"));
            }
        }
    }
    check_file(&dir.join(NVRAM_FILE), &layout.nvram, "nvram")?;
    check_disk(&dir.join(DISK_FILE), &layout.chunks)
}

fn config_without_mac(path: &Path) -> Result<serde_json::Map<String, serde_json::Value>, Refusal> {
    let mut bytes = Vec::new();
    regular(path, "config")?
        .take(1 << 20)
        .read_to_end(&mut bytes)
        .map_err(|_| Refusal::Content("config"))?;
    let mut config: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&bytes).map_err(|_| Refusal::Content("config"))?;
    config.remove("macAddress");
    Ok(config)
}

/// A file's identity on its filesystem: device, inode and birth time. APFS never reuses an
/// inode number; other filesystems may, so the birth time (when the platform reports one) tells a
/// file recreated under a recycled inode from the original.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileId {
    /// `st_dev`.
    pub dev: u64,
    /// `st_ino`.
    pub ino: u64,
    /// Birth time, seconds and nanoseconds since the epoch, when reported.
    pub born: Option<(u64, u32)>,
}

fn file_id(meta: &fs::Metadata) -> FileId {
    FileId {
        dev: meta.dev(),
        ino: meta.ino(),
        born: meta
            .created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| (d.as_secs(), d.subsec_nanos())),
    }
}

/// A file's identity, size and change time: what any write, truncate, chmod, `tart set` or
/// delete-and-recreate changes (a non-root user cannot set `ctime`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    /// Its identity.
    pub id: FileId,
    /// `st_size`.
    pub size: u64,
    /// `st_ctime`, seconds.
    pub ctime: i64,
    /// `st_ctime`, nanoseconds.
    pub ctime_nsec: i64,
}

/// A VM directory's identity: the directory and its disk (ADR 0087's "file identity").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// The directory.
    pub dir: FileId,
    /// `disk.img`.
    pub disk: FileId,
}

/// A stopped VM's fingerprint: its files' stamps. The directory's own change time is left out:
/// Tart touches the directory on every open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// The directory.
    pub dir: FileId,
    /// `config.json`.
    pub config: FileStamp,
    /// `nvram.bin`.
    pub nvram: FileStamp,
    /// `disk.img`.
    pub disk: FileStamp,
    /// `state.vzvmsave`, when there is one.
    pub state: Option<FileStamp>,
}

fn stamp(path: &Path) -> Option<FileStamp> {
    let meta = fs::symlink_metadata(path).ok()?;
    meta.file_type().is_file().then(|| FileStamp {
        id: file_id(&meta),
        size: meta.size(),
        ctime: meta.ctime(),
        ctime_nsec: meta.ctime_nsec(),
    })
}

fn dir_id(dir: &Path) -> Option<FileId> {
    let meta = fs::symlink_metadata(dir).ok()?;
    meta.file_type().is_dir().then(|| file_id(&meta))
}

/// The fingerprint of the VM directory `dir`; `None` when it or a file is missing or a symlink.
/// Pinned by `tests::should_change_the_fingerprint_when_a_file_is_written_or_replaced`.
#[must_use]
pub fn fingerprint(dir: &Path) -> Option<Fingerprint> {
    let state = match fs::symlink_metadata(dir.join(STATE_FILE)) {
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        _ => Some(stamp(&dir.join(STATE_FILE))?),
    };
    Some(Fingerprint {
        dir: dir_id(dir)?,
        config: stamp(&dir.join(CONFIG_FILE))?,
        nvram: stamp(&dir.join(NVRAM_FILE))?,
        disk: stamp(&dir.join(DISK_FILE))?,
        state,
    })
}

/// The identity of the VM directory `dir`; `None` when it or its disk is missing.
#[must_use]
pub fn identity(dir: &Path) -> Option<Identity> {
    Some(Identity {
        dir: dir_id(dir)?,
        disk: stamp(&dir.join(DISK_FILE))?.id,
    })
}

/// Why a directory is unsafe to run with (ADR 0087's trust model: root and the daemon's own
/// user are trusted, nobody else may write there).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsafeLayout {
    /// It is missing, not a directory, or not an absolute path without `.` or `..`.
    NotADirectory(PathBuf),
    /// It, or a component of the path to it, is a symlink.
    Symlink(PathBuf),
    /// It is not owned by the daemon's user.
    OwnedByOther(PathBuf),
    /// Its group or others have any access to it.
    OpenToOthers(PathBuf),
    /// An ancestor is writable by group or others without the sticky bit, or owned by neither
    /// root nor the daemon's user: whoever can write there can swap the directory.
    UnsafeAncestor(PathBuf),
}

/// The facts [`judge`] decides on, read from one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirFacts {
    /// `st_uid`.
    pub uid: u32,
    /// `st_mode`.
    pub mode: u32,
    /// Whether it is a directory and not a symlink.
    pub directory: bool,
}

const STICKY: u32 = 0o1000;
const GROUP_OTHER_WRITE: u32 = 0o022;
const GROUP_OTHER_ANY: u32 = 0o077;

/// Whether `facts` are safe for the private directory itself (`ancestor` false: a directory
/// owned by `euid` with no access for group or others) or for one of its ancestors (owned by
/// root or `euid`, not writable by group or others unless sticky). Pinned by
/// `tests::should_judge_a_private_directory_and_its_ancestors_when_given_their_facts`.
#[must_use]
pub const fn judge(facts: DirFacts, euid: u32, ancestor: bool) -> bool {
    if !facts.directory {
        return false;
    }
    if ancestor {
        let owner_ok = facts.uid == 0 || facts.uid == euid;
        let writable = facts.mode & GROUP_OTHER_WRITE != 0;
        owner_ok && (!writable || facts.mode & STICKY != 0)
    } else {
        facts.uid == euid && facts.mode & GROUP_OTHER_ANY == 0
    }
}

fn facts(path: &Path) -> Option<DirFacts> {
    let meta = fs::symlink_metadata(path).ok()?;
    Some(DirFacts {
        uid: meta.uid(),
        mode: meta.mode(),
        directory: meta.file_type().is_dir(),
    })
}

/// Checks that `dir` is private to the daemon's user and that no ancestor lets anyone else
/// swap it: the start-up check of ADR 0087. Every component of the path as configured is read
/// without following links, and a symlink anywhere in it is refused (the daemon uses the
/// configured path, so a link's target could change under it); `.` and `..` are refused too.
/// Its `vms` and `cache` subdirectories, when present, must not be writable by group or others
/// either. Pinned by
/// `tests/tart/images.rs::should_refuse_to_start_when_tart_home_or_an_ancestor_lets_others_swap_it`.
///
/// # Errors
/// [`UnsafeLayout`].
pub fn check_private(dir: &Path) -> Result<(), UnsafeLayout> {
    let euid = rustix::process::geteuid().as_raw();
    check_path(dir)?;
    let own = facts(dir);
    let own = own.ok_or_else(|| UnsafeLayout::NotADirectory(dir.to_path_buf()))?;
    if !own.directory {
        return Err(UnsafeLayout::NotADirectory(dir.to_path_buf()));
    }
    if own.uid != euid {
        return Err(UnsafeLayout::OwnedByOther(dir.to_path_buf()));
    }
    if !judge(own, euid, false) {
        return Err(UnsafeLayout::OpenToOthers(dir.to_path_buf()));
    }
    for sub in ["vms", "cache"] {
        let path = dir.join(sub);
        if let Some(sub_facts) = facts(&path)
            && (!sub_facts.directory
                || sub_facts.uid != euid
                || sub_facts.mode & GROUP_OTHER_WRITE != 0)
        {
            return Err(UnsafeLayout::OpenToOthers(path));
        }
    }
    Ok(())
}

/// The part of [`check_private`] that holds before `dir` exists, run before the daemon creates
/// it: an absolute path without `.` or `..`, no component that exists is a symlink, and every
/// ancestor that exists is owned by root or the daemon's user and not writable by group or
/// others unless sticky.
///
/// # Errors
/// [`UnsafeLayout`].
pub fn check_path(dir: &Path) -> Result<(), UnsafeLayout> {
    let euid = rustix::process::geteuid().as_raw();
    if !dir.is_absolute()
        || dir
            .components()
            .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return Err(UnsafeLayout::NotADirectory(dir.to_path_buf()));
    }
    for component in dir.ancestors() {
        let link = fs::symlink_metadata(component).is_ok_and(|m| m.file_type().is_symlink());
        if link {
            return Err(UnsafeLayout::Symlink(component.to_path_buf()));
        }
    }
    for ancestor in dir.ancestors().skip(1) {
        let Some(found) = facts(ancestor) else {
            continue;
        };
        if !judge(found, euid, true) {
            return Err(UnsafeLayout::UnsafeAncestor(ancestor.to_path_buf()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(layers: &[(&str, Option<(u64, &str)>)]) -> Vec<u8> {
        let digest = format!("sha256:{}", "a".repeat(64));
        let layers: Vec<serde_json::Value> = layers
            .iter()
            .map(|(media, chunk)| {
                let mut layer =
                    serde_json::json!({"mediaType": media, "digest": digest, "size": 3});
                if let Some((size, sha)) = chunk {
                    layer["annotations"] = serde_json::json!({
                        UNCOMPRESSED_SIZE: size.to_string(),
                        UNCOMPRESSED_DIGEST: format!("sha256:{sha}"),
                    });
                }
                layer
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2, "mediaType": MANIFEST_V1, "layers": layers
        }))
        .expect("json")
    }

    #[test]
    fn should_refuse_a_manifest_when_its_layout_is_not_disk_v2() {
        // given
        let b = "b".repeat(64);
        let chunk = Some((4, b.as_str()));
        let good = manifest(&[
            (CONFIG_LAYER, None),
            (DISK_V2_LAYER, chunk),
            (NVRAM_LAYER, None),
        ]);
        let v1 = manifest(&[
            (CONFIG_LAYER, None),
            ("application/vnd.cirruslabs.tart.disk.v1", None),
            (NVRAM_LAYER, None),
        ]);
        let asif = manifest(&[
            (CONFIG_LAYER, None),
            (DISK_V2_LAYER, chunk),
            ("application/vnd.cirruslabs.tart.disk.asif.overlay.v1", None),
            (NVRAM_LAYER, None),
        ]);
        let unannotated = manifest(&[
            (CONFIG_LAYER, None),
            (DISK_V2_LAYER, None),
            (NVRAM_LAYER, None),
        ]);
        let two_configs = manifest(&[
            (CONFIG_LAYER, None),
            (CONFIG_LAYER, None),
            (DISK_V2_LAYER, chunk),
            (NVRAM_LAYER, None),
        ]);
        let nvram_before_disk = manifest(&[
            (CONFIG_LAYER, None),
            (NVRAM_LAYER, None),
            (DISK_V2_LAYER, chunk),
        ]);

        // when
        let judged = [
            layout(&good).map(|l| l.chunks.len()),
            layout(&v1).map(|l| l.chunks.len()),
            layout(&asif).map(|l| l.chunks.len()),
            layout(&unannotated).map(|l| l.chunks.len()),
            layout(&two_configs).map(|l| l.chunks.len()),
            layout(&nvram_before_disk).map(|l| l.chunks.len()),
            layout(b"{}").map(|l| l.chunks.len()),
        ];

        // then
        assert_eq!(
            judged,
            [
                Ok(1),
                Err(Refusal::UnsupportedLayer(
                    "application/vnd.cirruslabs.tart.disk.v1".to_owned()
                )),
                Err(Refusal::UnsupportedLayer(
                    "application/vnd.cirruslabs.tart.disk.asif.overlay.v1".to_owned()
                )),
                Err(Refusal::Annotations),
                Err(Refusal::Layers),
                Err(Refusal::Layers),
                Err(Refusal::Unparseable),
            ]
        );
    }

    #[test]
    fn should_refuse_a_disk_when_a_chunk_does_not_hash_to_its_digest() {
        // given
        let dir = tempfile::tempdir().expect("tempdir");
        let disk = dir.path().join(DISK_FILE);
        fs::write(&disk, b"abcdefgh").expect("disk");
        let chunks = |second: &[u8]| {
            vec![
                Expected {
                    size: 4,
                    sha256: sha256_hex(b"abcd"),
                },
                Expected {
                    size: 4,
                    sha256: sha256_hex(second),
                },
            ]
        };

        // when
        let good = check_disk(&disk, &chunks(b"efgh"));
        let bad = check_disk(&disk, &chunks(b"efgX"));

        // then
        assert_eq!(good, Ok(()));
        assert_eq!(bad, Err(Refusal::Content("disk chunk")));
    }

    #[test]
    fn should_change_the_fingerprint_when_a_file_is_written_or_replaced() {
        // given
        let dir = tempfile::tempdir().expect("tempdir");
        for file in [CONFIG_FILE, NVRAM_FILE, DISK_FILE] {
            fs::write(dir.path().join(file), file).expect("file");
        }
        let before = fingerprint(dir.path()).expect("a fingerprint");
        let same = fingerprint(dir.path()).expect("a fingerprint");

        // when
        fs::remove_file(dir.path().join(DISK_FILE)).expect("remove");
        fs::write(dir.path().join(DISK_FILE), DISK_FILE).expect("recreate");
        let after = fingerprint(dir.path()).expect("a fingerprint");

        // then
        assert_eq!(before, same);
        assert_ne!(before.disk, after.disk);
        assert_eq!(before.config, after.config);
    }

    #[test]
    fn should_judge_a_private_directory_and_its_ancestors_when_given_their_facts() {
        // given
        let euid = 501;
        let dir = |uid, mode| DirFacts {
            uid,
            mode,
            directory: true,
        };
        let cases = [
            (dir(501, 0o700), false, true),
            (dir(501, 0o750), false, false),
            (dir(502, 0o700), false, false),
            (dir(0, 0o755), true, true),
            (dir(501, 0o755), true, true),
            (dir(0, 0o1777), true, true),
            (dir(0, 0o777), true, false),
            (dir(502, 0o755), true, false),
            (
                DirFacts {
                    uid: 501,
                    mode: 0o700,
                    directory: false,
                },
                false,
                false,
            ),
        ];

        for (facts, ancestor, safe) in cases {
            // when
            let judged = judge(facts, euid, ancestor);

            // then
            assert_eq!(judged, safe, "{facts:?} ancestor={ancestor}");
        }
    }
}
