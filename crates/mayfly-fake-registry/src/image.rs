//! Synthetic Tart images, what the registry publishes, and the same image seeded for
//! `mayfly-fake-tart`.
//!
//! A Tart image is an OCI image manifest whose layers are, in this order, one VM config, the disk
//! as disk-v2 chunks (each annotated with its uncompressed size and digest) and one NVRAM, with
//! the disk's uncompressed size as a manifest annotation (Tart's `OCIManifest`, the layout the
//! data plane accepts: ADR 0087 decision 7.2). The chunks are kept uncompressed under the digest
//! that stands for their compressed form, as `mayfly-fake-tart` models them (it concatenates the
//! blobs into `disk.img` as they are): the data plane checks every chunk against its
//! uncompressed digest, and re-checking compressed blobs is ADR 0087's stated follow-up.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde_json::json;
use sha2::{Digest as _, Sha256};

/// An OCI image manifest's media type.
const MANIFEST_V1: &str = "application/vnd.oci.image.manifest.v1+json";
/// An OCI image config's media type.
const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
/// Tart's VM config layer.
const CONFIG_LAYER: &str = "application/vnd.cirruslabs.tart.config.v1";
/// Tart's NVRAM layer.
const NVRAM_LAYER: &str = "application/vnd.cirruslabs.tart.nvram.v1";
/// One chunk of a disk-v2 disk.
const DISK_V2_LAYER: &str = "application/vnd.cirruslabs.tart.disk.v2";
/// A chunk's uncompressed size.
const UNCOMPRESSED_SIZE: &str = "org.cirruslabs.tart.uncompressed-size";
/// A chunk's uncompressed digest.
const UNCOMPRESSED_DIGEST: &str = "org.cirruslabs.tart.uncompressed-content-digest";
/// The disk's uncompressed size, on the manifest.
const DISK_SIZE: &str = "org.cirruslabs.tart.uncompressed-disk-size";

/// The guest an image boots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestOs {
    /// macOS: Tart's config names `darwin`.
    Macos,
    /// Linux.
    Linux,
}

impl GuestOs {
    const fn tart_name(self) -> &'static str {
        match self {
            Self::Macos => "darwin",
            Self::Linux => "linux",
        }
    }
}

/// `sha256:<lowercase hex>` of `bytes`.
fn digest_of(bytes: &[u8]) -> String {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

/// One synthetic Tart image: its manifest's exact bytes, the manifest's digest and every blob
/// the manifest names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TartImage {
    manifest: Vec<u8>,
    digest: String,
    blobs: BTreeMap<String, Vec<u8>>,
    disk_bytes: u64,
}

impl TartImage {
    /// An image of `guest` derived from `content` (distinct contents give distinct digests): a
    /// VM config, two disk chunks and an NVRAM, laid out as Tart lays out an image. Pinned by
    /// `tests::should_describe_every_chunk_when_an_image_is_made`.
    #[must_use]
    pub fn synthetic(content: &[u8], guest: GuestOs) -> Self {
        let os = guest.tart_name();
        let vm_config = serde_json::to_vec(&json!({
            "version": 1, "os": os, "arch": "arm64", "cpuCount": 2,
            "memorySize": 4_u64 << 30, "macAddress": "0e:aa:00:00:00:01",
        }))
        .unwrap_or_default();
        let nvram = [b"nvram:".as_slice(), content].concat();
        let chunks = [
            [b"chunk-1:".as_slice(), content].concat(),
            [b"chunk-2:".as_slice(), content].concat(),
        ];
        let oci_config =
            serde_json::to_vec(&json!({"architecture": "arm64", "os": os})).unwrap_or_default();
        let mut blobs = BTreeMap::new();
        let mut whole = |media: &str, bytes: Vec<u8>| {
            let digest = digest_of(&bytes);
            let descriptor = json!({"mediaType": media, "digest": digest, "size": bytes.len()});
            blobs.insert(digest, bytes);
            descriptor
        };
        let config = whole(OCI_CONFIG, oci_config);
        let mut layers = vec![whole(CONFIG_LAYER, vm_config)];
        let nvram_layer = whole(NVRAM_LAYER, nvram);
        let mut disk_bytes = 0_u64;
        for chunk in chunks {
            // The layer's digest stands for the compressed blob; the blob is kept uncompressed
            // (see the module docs).
            let compressed = digest_of(&[b"lz4:".as_slice(), &chunk].concat());
            let size = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
            disk_bytes = disk_bytes.saturating_add(size);
            layers.push(json!({
                "mediaType": DISK_V2_LAYER, "digest": compressed, "size": size.saturating_add(4),
                "annotations": {
                    UNCOMPRESSED_SIZE: size.to_string(),
                    UNCOMPRESSED_DIGEST: digest_of(&chunk),
                },
            }));
            blobs.insert(compressed, chunk);
        }
        layers.push(nvram_layer);
        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2, "mediaType": MANIFEST_V1, "config": config, "layers": layers,
            "annotations": {DISK_SIZE: disk_bytes.to_string()},
        }))
        .unwrap_or_default();
        Self {
            digest: digest_of(&manifest),
            manifest,
            blobs,
            disk_bytes,
        }
    }

    /// The manifest's exact bytes.
    #[must_use]
    pub fn manifest(&self) -> &[u8] {
        &self.manifest
    }

    /// The manifest's digest, `sha256:<hex>`: the reference a pull by digest names.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Every blob the manifest names, by its descriptor's digest. A disk-v2 chunk's blob is the
    /// uncompressed chunk, kept under the digest that stands for its compressed form, so its
    /// bytes do not hash to that digest (see the module docs); every other blob's do.
    #[must_use]
    pub fn blobs(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.blobs
    }

    /// The disk's uncompressed size: the chunks' uncompressed sizes summed, what the data plane
    /// measures a base's disk by.
    #[must_use]
    pub const fn disk_bytes(&self) -> u64 {
        self.disk_bytes
    }
}

/// A published manifest: its bytes, digest and media type.
#[derive(Debug, Clone)]
pub(crate) struct Published {
    pub(crate) bytes: Vec<u8>,
    pub(crate) digest: String,
    pub(crate) media_type: String,
}

/// What a registry serves: manifests by repository name and reference, and blobs by repository
/// name and digest.
#[derive(Debug, Clone, Default)]
pub struct Images {
    manifests: BTreeMap<(String, String), Published>,
    blobs: BTreeMap<(String, String), Vec<u8>>,
}

impl Images {
    /// Nothing published.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes `image` in repository `name` (the path after the registry's host, like
    /// `mayfly/runner`): its manifest at its digest and its blobs, each disk-v2 chunk under a
    /// digest its bytes do not hash to ([`TartImage::blobs`]).
    #[must_use]
    pub fn publish(mut self, name: &str, image: &TartImage) -> Self {
        let media_type = serde_json::from_slice::<serde_json::Value>(&image.manifest)
            .ok()
            .and_then(|m| m["mediaType"].as_str().map(str::to_owned))
            .unwrap_or_else(|| MANIFEST_V1.to_owned());
        self.manifests.insert(
            (name.to_owned(), image.digest.clone()),
            Published {
                bytes: image.manifest.clone(),
                digest: image.digest.clone(),
                media_type,
            },
        );
        for (digest, blob) in &image.blobs {
            self.blobs
                .insert((name.to_owned(), digest.clone()), blob.clone());
        }
        self
    }

    /// Serves `bytes` as the manifest at `reference` in repository `name`, claiming `reference`
    /// as its digest whatever the bytes hash to: a registry that lies, serving a tampered or
    /// substituted manifest at a pinned digest. Pinned by
    /// `tests::should_serve_other_bytes_at_a_reference_when_told_to_lie` and, against the
    /// data plane, by `mayfly-composition`'s
    /// `tests/daemon.rs::placement::should_pull_nothing_when_the_registry_serves_other_bytes_at_the_pinned_digest`.
    #[must_use]
    pub fn serve_at(mut self, name: &str, reference: &str, bytes: &[u8]) -> Self {
        let media_type = serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|m| m["mediaType"].as_str().map(str::to_owned))
            .unwrap_or_else(|| MANIFEST_V1.to_owned());
        self.manifests.insert(
            (name.to_owned(), reference.to_owned()),
            Published {
                bytes: bytes.to_vec(),
                digest: reference.to_owned(),
                media_type,
            },
        );
        self
    }

    pub(crate) fn manifest(&self, name: &str, reference: &str) -> Option<&Published> {
        self.manifests.get(&(name.to_owned(), reference.to_owned()))
    }

    pub(crate) fn blob(&self, name: &str, digest: &str) -> Option<&Vec<u8>> {
        self.blobs.get(&(name.to_owned(), digest.to_owned()))
    }

    pub(crate) fn knows(&self, name: &str) -> bool {
        self.manifests.keys().any(|(n, _)| n == name)
    }
}

/// The file name `mayfly-fake-tart` keeps a registry entry or blob under (its documented
/// `TART_HOME` layout, `home::encode_name`): bytes outside `[A-Za-z0-9_-]`, and a leading dot,
/// become `%XX`. Kept here rather than imported: the fakes reach no workspace crate (D11, ADR
/// 0012). Pinned by
/// `tests::should_encode_a_reference_as_the_fake_tart_names_its_files_when_seeding`, which
/// carries fake-tart's own vectors (`home::tests::should_encode_names_when_they_hold_path_characters`);
/// the cross-check of the two copies is `mayfly-composition`'s Linux daemon test
/// (`tests/daemon.rs`, `mod placement`), where a mismatch is a pull the fake refuses.
#[must_use]
pub fn encode_name(name: &str) -> String {
    name.bytes()
        .enumerate()
        .map(|(index, byte)| {
            if byte.is_ascii_alphanumeric()
                || byte == b'_'
                || byte == b'-'
                || (byte == b'.' && index > 0)
            {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

/// Seeds `mayfly-fake-tart`'s registry under `tart_home` with `image` as published at
/// `repository` (the full reference's repository, host included, like
/// `127.0.0.1:4430/mayfly/runner`): the manifest at `<repository>@<digest>` and every blob,
/// so the fake's `tart pull <repository>@<digest>` lays out the bytes this registry serves.
/// Pinned by `tests::should_write_the_manifest_and_blobs_where_the_fake_reads_them_when_seeding`.
///
/// # Errors
/// When a directory or file cannot be written.
pub fn seed_tart_home(tart_home: &Path, repository: &str, image: &TartImage) -> io::Result<()> {
    let registry = tart_home.join("registry");
    private_dir(&registry)?;
    private_dir(&registry.join("blobs"))?;
    std::fs::write(
        registry.join(encode_name(&format!("{repository}@{}", image.digest))),
        &image.manifest,
    )?;
    for (digest, blob) in &image.blobs {
        std::fs::write(registry.join("blobs").join(encode_name(digest)), blob)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(image: &TartImage) -> serde_json::Value {
        serde_json::from_slice(image.manifest()).expect("a JSON manifest")
    }

    #[test]
    fn should_name_the_manifest_by_its_digest_when_an_image_is_made() {
        // given
        let content = b"runner-image";

        // when
        let image = TartImage::synthetic(content, GuestOs::Linux);
        let other = TartImage::synthetic(b"another", GuestOs::Linux);

        // then
        assert_eq!(image.digest(), digest_of(image.manifest()));
        assert_ne!(image.digest(), other.digest());
        assert_eq!(image, TartImage::synthetic(content, GuestOs::Linux));
    }

    #[test]
    fn should_describe_every_chunk_when_an_image_is_made() {
        // given
        let image = TartImage::synthetic(b"runner-image", GuestOs::Linux);

        // when
        let manifest = manifest(&image);
        let layers = manifest["layers"].as_array().expect("layers");

        // then: config first, NVRAM last, disk-v2 chunks between them, each annotated with
        // the size and digest of the blob served under its digest
        let media: Vec<&str> = layers
            .iter()
            .map(|l| l["mediaType"].as_str().expect("a media type"))
            .collect();
        assert_eq!(
            media,
            [CONFIG_LAYER, DISK_V2_LAYER, DISK_V2_LAYER, NVRAM_LAYER]
        );
        let mut total = 0;
        for chunk in &layers[1..3] {
            let blob = &image.blobs()[chunk["digest"].as_str().expect("a digest")];
            let size = chunk["annotations"][UNCOMPRESSED_SIZE]
                .as_str()
                .expect("a size");
            assert_eq!(size, blob.len().to_string());
            assert_eq!(chunk["annotations"][UNCOMPRESSED_DIGEST], digest_of(blob));
            total += blob.len();
        }
        assert_eq!(manifest["annotations"][DISK_SIZE], total.to_string());
        assert_eq!(image.disk_bytes(), u64::try_from(total).expect("a size"));
        for whole in [&layers[0], &layers[3], &manifest["config"]] {
            let blob = &image.blobs()[whole["digest"].as_str().expect("a digest")];
            assert_eq!(whole["digest"], digest_of(blob));
            assert_eq!(whole["size"], blob.len());
        }
        assert_eq!(manifest["mediaType"], MANIFEST_V1);
        assert_eq!(manifest["schemaVersion"], 2);
    }

    #[test]
    fn should_name_the_guest_in_the_vm_config_when_an_image_is_made() {
        // given
        let guests = [(GuestOs::Linux, "linux"), (GuestOs::Macos, "darwin")];

        for (guest, os) in guests {
            // when
            let image = TartImage::synthetic(b"x", guest);

            // then
            let layer = &manifest(&image)["layers"][0];
            let config: serde_json::Value =
                serde_json::from_slice(&image.blobs()[layer["digest"].as_str().expect("a digest")])
                    .expect("a VM config");
            assert_eq!(config["os"], os);
        }
    }

    #[test]
    fn should_encode_a_reference_as_the_fake_tart_names_its_files_when_seeding() {
        // given
        // fake-tart's own vectors (`home::tests::should_encode_names_when_they_hold_path_characters`),
        // then a registry reference as the seeding writes it
        let names = [
            ("plain-name_1", "plain-name_1"),
            ("ghcr.io/x/y:1", "ghcr.io%2Fx%2Fy%3A1"),
            ("a b", "a%20b"),
            (
                "127.0.0.1:4430/a/b@sha256:ab",
                "127.0.0.1%3A4430%2Fa%2Fb%40sha256%3Aab",
            ),
            (".hidden", "%2Ehidden"),
            ("plain_name-1", "plain_name-1"),
        ];

        for (name, want) in names {
            // when
            let got = encode_name(name);

            // then
            assert_eq!(got, want);
        }
    }

    #[test]
    fn should_write_the_manifest_and_blobs_where_the_fake_reads_them_when_seeding() {
        // given
        let home = tempfile::tempdir().expect("a tempdir");
        let image = TartImage::synthetic(b"runner-image", GuestOs::Linux);
        let repository = "127.0.0.1:4430/mayfly/runner";

        // when
        seed_tart_home(home.path(), repository, &image).expect("seeded");
        seed_tart_home(home.path(), repository, &image).expect("seeded again");

        // then
        let registry = home.path().join("registry");
        let served =
            std::fs::read(registry.join(encode_name(&format!("{repository}@{}", image.digest()))))
                .expect("the manifest");
        assert_eq!(served, image.manifest());
        for (digest, blob) in image.blobs() {
            let stored =
                std::fs::read(registry.join("blobs").join(encode_name(digest))).expect("a blob");
            assert_eq!(&stored, blob);
        }
        let mode = |p: &Path| {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777
        };
        assert_eq!(mode(&registry), 0o700);
        assert_eq!(mode(&registry.join("blobs")), 0o700);
    }

    #[test]
    fn should_serve_other_bytes_at_a_reference_when_told_to_lie() {
        // given
        let pinned = TartImage::synthetic(b"pinned", GuestOs::Linux);
        let other = TartImage::synthetic(b"other", GuestOs::Linux);

        // when
        let images = Images::new().serve_at("mayfly/runner", pinned.digest(), other.manifest());

        // then
        let served = images
            .manifest("mayfly/runner", pinned.digest())
            .expect("served");
        assert_eq!(served.bytes, other.manifest());
        assert_eq!(served.digest, pinned.digest());
        assert_ne!(digest_of(&served.bytes), pinned.digest());
        assert!(images.knows("mayfly/runner"));
    }

    #[test]
    fn should_serve_what_was_published_under_its_name_when_images_are_looked_up() {
        // given
        let image = TartImage::synthetic(b"runner-image", GuestOs::Linux);
        let (blob_digest, blob) = image.blobs().iter().next().expect("a blob");

        // when
        let images = Images::new().publish("mayfly/runner", &image);

        // then
        let published = images
            .manifest("mayfly/runner", image.digest())
            .expect("published");
        assert_eq!(published.bytes, image.manifest());
        assert_eq!(published.digest, image.digest());
        assert_eq!(published.media_type, MANIFEST_V1);
        assert_eq!(images.blob("mayfly/runner", blob_digest), Some(blob));
        assert!(images.knows("mayfly/runner"));
        assert!(!images.knows("other/runner"));
        assert!(images.manifest("other/runner", image.digest()).is_none());
        assert!(images.blob("other/runner", blob_digest).is_none());
    }
}
