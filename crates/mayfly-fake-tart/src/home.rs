//! The state directory (`TART_HOME`): its guard, lock, atomic writes and file layout.
//!
//! ```text
//! $TART_HOME/
//!   fake-tart.marker     written on first use; proves the directory is the fake's
//!   fake-tart.lock       flock target; held only for short read-modify-write sections
//!   state.json           counters: invocation ids, MAC and address allocation, rule counters
//!   faults.toml          the fault file (written by the lab, see `faults`)
//!   invocations.jsonl    append-only record of every invocation (see `log`)
//!   vms/<name>.json      one VM record each (name percent-encoded)
//!   vms/<name>/config.json  the VM's configuration (JSON with its `macAddress`, as Tart's) and
//!                        its run lock: a foreground `run` holds a POSIX record lock on it for its
//!                        whole life, as Tart's `PIDLock` does, so `fcntl(F_GETLK)` names the
//!                        process that runs the VM
//!   vms/<name>/nvram.bin, disk.img   the VM's other files, copied by `clone` (new inodes, as an
//!                        APFS clone has), so a caller can fingerprint and hash them
//!   guest-logs/<name>.log   the guest's runner log, written by the lab; the launcher's
//!                        `diagnostics` prints it (see `guest`)
//!   registry/<reference> the registry the lab seeds: one manifest per reference
//!                        (`<repo>@sha256:<hex>` or `<repo>:<tag>`, name encoded), served by `pull`
//!   registry/blobs/<digest>  the blobs its layers name (encoded), served as they are
//!   cache/OCIs/<repo>/sha256:<hash of the manifest served>/
//!                        what `pull` lays out, as Tart 2.41.1 does (`VMStorageOCI.pull`, `link`,
//!                        `list`, `gc`): the directory is named by the hash of the manifest it was
//!                        served, never compared with the digest asked for; another requested name
//!                        (a digest or a tag) is an absolute symlink to it (`createSymbolicLink(at:
//!                        withDestinationURL:)` with an absolute URL), listed `<repo>:<name>`
//!                        (`list` joins a symlink's parts with `:`, a directory's with `@`);
//!                        `manifest.json` holds the bytes served (2.28.0 re-serialized them);
//!                        `config.json`, `nvram.bin` and `disk.img` are the layers' blobs (disk
//!                        chunks concatenated in manifest order; the fake stores chunks
//!                        uncompressed); deleting a name removes its link or directory, then any
//!                        directory no link names that was not pulled by its own digest
//! ```
//!
//! Every write is a temp file in the same directory, `sync_all`, then `rename`, so a reader (the
//! fault lab parses these files while the fake runs) never sees a half-written file.
//!
//! Pinned by: `home::tests`, `tests/cli/commands.rs::should_refuse_a_home_that_looks_real_when_it_has_no_marker`.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fail::Fail;
use crate::faults::{Counters, FaultFile};
use crate::log::Record;
use crate::vm::VmRecord;

const MARKER: &str = "fake-tart.marker";
const LOCK: &str = "fake-tart.lock";
const STATE: &str = "state.json";
const FAULTS: &str = "faults.toml";
const LOG: &str = "invocations.jsonl";
const VMS: &str = "vms";
const REGISTRY: &str = "registry";
/// A VM's configuration file, its run lock too.
pub(crate) const CONFIG: &str = "config.json";
/// A VM's NVRAM file.
pub(crate) const NVRAM: &str = "nvram.bin";
/// A VM's disk file.
pub(crate) const DISK: &str = "disk.img";
const OCI_CACHE: &str = "cache/OCIs";

/// Directories only a real Tart home has before the fake ever ran there.
const REAL_HOME_SIGNS: &[&str] = &["vms", "cache", "tmp"];

/// The counters kept in `state.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct State {
    pub next_inv: u64,
    pub next_vm: u64,
    pub rules: BTreeMap<String, Counters>,
}

/// An opened, guarded state directory.
pub(crate) struct Home {
    pub dir: PathBuf,
}

/// The exclusive lock on the state directory; released on drop.
pub(crate) struct Lock(File);

impl Drop for Lock {
    fn drop(&mut self) {
        drop(self.0.unlock());
    }
}

fn io(context: &str, error: &std::io::Error) -> Fail {
    Fail::fake(&format!("{context}: {error}"))
}

impl Home {
    /// Opens `TART_HOME`, refusing when it is unset, relative, or a directory that looks like a
    /// real Tart home (it has `vms`, `cache` or `tmp` and no fake marker).
    pub fn open(var: Option<&str>) -> Result<Self, Fail> {
        let Some(raw) = var.filter(|v| !v.is_empty()) else {
            return Err(Fail::fake(
                "TART_HOME is not set; refusing to run so that it can never touch a real Tart home",
            ));
        };
        let dir = PathBuf::from(raw);
        if !dir.is_absolute() {
            return Err(Fail::fake("TART_HOME must be an absolute path"));
        }
        let home = Self { dir };
        if !home.dir.join(MARKER).exists() {
            if let Some(sign) = REAL_HOME_SIGNS.iter().find(|s| home.dir.join(s).exists()) {
                return Err(Fail::fake(&format!(
                    "TART_HOME has '{sign}' but no fake-tart marker; refusing to touch what may be a real Tart home"
                )));
            }
            fs::create_dir_all(&home.dir).map_err(|e| io("cannot create TART_HOME", &e))?;
            write_atomic(&home.dir.join(MARKER), b"mayfly-fake-tart\n")?;
        }
        fs::create_dir_all(home.dir.join(VMS)).map_err(|e| io("cannot create vms", &e))?;
        Ok(home)
    }

    /// Takes the directory lock, blocking until it is free.
    pub fn lock(&self) -> Result<Lock, Fail> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(LOCK))
            .map_err(|e| io("cannot open the lock file", &e))?;
        file.lock().map_err(|e| io("cannot take the lock", &e))?;
        Ok(Lock(file))
    }

    /// Reads `state.json` (defaults when absent).
    pub fn read_state(&self) -> Result<State, Fail> {
        read_json(&self.dir.join(STATE)).map(Option::unwrap_or_default)
    }

    /// Replaces `state.json`.
    pub fn write_state(&self, state: &State) -> Result<(), Fail> {
        write_json(&self.dir.join(STATE), state)
    }

    /// Reads and validates `faults.toml` (no rules when absent).
    pub fn read_faults(&self) -> Result<FaultFile, Fail> {
        match fs::read_to_string(self.dir.join(FAULTS)) {
            Ok(text) => FaultFile::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FaultFile::default()),
            Err(e) => Err(io("cannot read faults.toml", &e)),
        }
    }

    fn vm_path(&self, name: &str) -> PathBuf {
        self.dir
            .join(VMS)
            .join(format!("{}.json", encode_name(name)))
    }

    /// Reads one VM record.
    pub fn read_vm(&self, name: &str) -> Result<Option<VmRecord>, Fail> {
        read_json(&self.vm_path(name))
    }

    /// Replaces one VM record.
    pub fn write_vm(&self, vm: &VmRecord) -> Result<(), Fail> {
        write_json(&self.vm_path(&vm.name), vm)
    }

    /// Removes one VM record and its run-lock directory.
    pub fn remove_vm(&self, name: &str) -> Result<(), Fail> {
        fs::remove_file(self.vm_path(name)).map_err(|e| io("cannot remove a VM record", &e))?;
        match fs::remove_dir_all(self.dir.join(VMS).join(encode_name(name))) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(io("cannot remove a VM's directory", &e))
            }
            _ => Ok(()),
        }
    }

    /// Takes the VM's run lock (Tart's `PIDLock` on `vms/<name>/config.json`): a POSIX record
    /// lock, held by this process until the returned file is closed or the process ends.
    pub fn take_run_lock(&self, name: &str) -> Result<File, Fail> {
        let dir = self.dir.join(VMS).join(encode_name(name));
        fs::create_dir_all(&dir).map_err(|e| io("cannot create a VM's directory", &e))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(CONFIG))
            .map_err(|e| io("cannot open a VM's config", &e))?;
        rustix::fs::fcntl_lock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| Fail::generic(&format!("the VM \"{name}\" is already running")))?;
        Ok(file)
    }

    /// The guest's runner log the lab wrote, or nothing.
    pub fn guest_log(&self, name: &str) -> Result<String, Fail> {
        let path = self
            .dir
            .join("guest-logs")
            .join(format!("{}.log", encode_name(name)));
        match fs::read_to_string(path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(io("cannot read a guest log", &e)),
        }
    }

    /// Whether the lab seeded a registry: then `pull` serves only what it holds.
    pub fn registry_seeded(&self) -> bool {
        self.dir.join(REGISTRY).is_dir()
    }

    /// The manifest the seeded registry serves for `reference`, if any.
    pub fn registry_manifest(&self, reference: &str) -> Result<Option<Vec<u8>>, Fail> {
        match fs::read(self.dir.join(REGISTRY).join(encode_name(reference))) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io("cannot read the registry", &e)),
        }
    }

    /// The OCI cache directory of `reference`: `cache/OCIs/<repo>/<digest or tag>`; `None` for a
    /// reference without that shape.
    pub fn oci_dir(&self, reference: &str) -> Option<PathBuf> {
        let (repository, version) = split_reference(reference)?;
        let mut dir = self.dir.join(OCI_CACHE);
        for part in repository.split('/') {
            dir.push(part);
        }
        dir.push(version);
        Some(dir)
    }

    /// A blob the seeded registry serves, by its descriptor's digest.
    pub fn registry_blob(&self, digest: &str) -> Result<Option<Vec<u8>>, Fail> {
        match fs::read(
            self.dir
                .join(REGISTRY)
                .join("blobs")
                .join(encode_name(digest)),
        ) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io("cannot read a registry blob", &e)),
        }
    }

    /// Lays out a pulled image the way Tart 2.41.1 does (see the module docs): the directory is
    /// named by `served`'s hash, `reference`'s own name links to it when it differs.
    pub fn write_oci_entry(
        &self,
        reference: &str,
        served_hash: &str,
        manifest: &[u8],
        files: &[(&str, Vec<u8>)],
    ) -> Result<(), Fail> {
        let (repository, version) = split_reference(reference)
            .ok_or_else(|| Fail::generic(&format!("{reference} is not a remote name")))?;
        let target = self
            .oci_dir(&format!("{repository}@{served_hash}"))
            .ok_or_else(|| Fail::generic(&format!("{reference} is not a remote name")))?;
        fs::create_dir_all(&target).map_err(|e| io("cannot create the OCI cache", &e))?;
        write_atomic(&target.join("manifest.json"), manifest)?;
        for (name, bytes) in files {
            write_atomic(&target.join(name), bytes)?;
        }
        if version != served_hash {
            let link = target.with_file_name(version);
            let _gone = fs::remove_file(&link);
            std::os::unix::fs::symlink(&target, &link)
                .map_err(|e| io("cannot link the OCI cache entry", &e))?;
        }
        Ok(())
    }

    /// A VM's directory, `vms/<name>/`.
    pub fn vm_dir(&self, name: &str) -> PathBuf {
        self.dir.join(VMS).join(encode_name(name))
    }

    /// Writes a VM's files, each atomically (a new inode, as Tart's writes are).
    pub fn write_vm_files(&self, name: &str, files: &[(&str, &[u8])]) -> Result<(), Fail> {
        let dir = self.vm_dir(name);
        fs::create_dir_all(&dir).map_err(|e| io("cannot create a VM's directory", &e))?;
        for (file, bytes) in files {
            write_atomic(&dir.join(file), bytes)?;
        }
        Ok(())
    }

    /// Copies a VM's files from `from` (a VM's or an OCI entry's directory) to `name`'s.
    pub fn copy_vm_files(&self, from: &Path, name: &str) -> Result<(), Fail> {
        let dir = self.vm_dir(name);
        fs::create_dir_all(&dir).map_err(|e| io("cannot create a VM's directory", &e))?;
        for file in [CONFIG, NVRAM, DISK] {
            match fs::read(from.join(file)) {
                Ok(bytes) => write_atomic(&dir.join(file), &bytes)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io("cannot copy a VM's file", &e)),
            }
        }
        Ok(())
    }

    /// Sets `macAddress` in a VM's `config.json`, as `tart set` and a cloned MAC rewrite it.
    pub fn set_config_mac(&self, name: &str, mac: &str) -> Result<(), Fail> {
        self.set_config_field(name, "macAddress", mac)
    }

    /// Sets one field of a VM's `config.json` (`tart set` saves the whole file anew).
    pub fn set_config_field(&self, name: &str, key: &str, value: &str) -> Result<(), Fail> {
        let path = self.vm_dir(name).join(CONFIG);
        let mut config: serde_json::Map<String, serde_json::Value> = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        config.insert(key.to_owned(), serde_json::Value::from(value));
        let bytes = serde_json::to_vec(&config)
            .map_err(|e| Fail::fake(&format!("cannot encode a config: {e}")))?;
        write_atomic(&path, &bytes)
    }

    /// Whether `reference`'s OCI cache entry is a link to another entry.
    pub fn oci_is_link(&self, reference: &str) -> bool {
        self.oci_dir(reference)
            .and_then(|dir| fs::symlink_metadata(dir).ok())
            .is_some_and(|meta| meta.file_type().is_symlink())
    }

    /// The directory `reference`'s entry is, or links to; `None` for a broken or missing one.
    pub fn oci_target(&self, reference: &str) -> Option<PathBuf> {
        fs::canonicalize(self.oci_dir(reference)?).ok()
    }

    /// The name `tart list` prints for an OCI entry: `<repo>:<name>` for a link, the reference
    /// as it is for a directory (`VMStorageOCI.list`).
    pub fn oci_listed_name(&self, reference: &str) -> String {
        match split_reference(reference) {
            Some((repository, version)) if self.oci_is_link(reference) => {
                format!("{repository}:{version}")
            }
            _ => reference.to_owned(),
        }
    }

    /// Removes `reference`'s OCI cache directory, if it has one.
    pub fn remove_oci(&self, reference: &str) -> Result<(), Fail> {
        let Some(dir) = self.oci_dir(reference) else {
            return Ok(());
        };
        match fs::remove_dir_all(dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(io("cannot remove an OCI cache entry", &e))
            }
            _ => Ok(()),
        }
    }

    /// Every VM record, in no particular order.
    pub fn list_vms(&self) -> Result<Vec<VmRecord>, Fail> {
        let entries = fs::read_dir(self.dir.join(VMS)).map_err(|e| io("cannot list vms", &e))?;
        let mut vms = Vec::new();
        for entry in entries {
            let path = entry.map_err(|e| io("cannot list vms", &e))?.path();
            let visible = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| !n.starts_with('.'));
            if visible && path.is_file() && path.extension().is_some_and(|e| e == "json") {
                vms.extend(read_json::<VmRecord>(&path)?);
            }
        }
        Ok(vms)
    }

    /// Appends one record to `invocations.jsonl` as a single write.
    pub fn append_log(&self, record: &Record) -> Result<(), Fail> {
        let mut line = serde_json::to_vec(record)
            .map_err(|e| Fail::fake(&format!("cannot encode a log record: {e}")))?;
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(LOG))
            .map_err(|e| io("cannot open the invocation log", &e))?;
        file.write_all(&line)
            .map_err(|e| io("cannot append to the invocation log", &e))
    }
}

/// A remote name's repository and its digest (`<repo>@sha256:<hex>`) or tag (`<repo>:<tag>`);
/// `None` when it has neither or a repository part is empty, `.` or `..`.
fn split_reference(reference: &str) -> Option<(&str, &str)> {
    let (repository, version) = reference.split_once('@').or_else(|| {
        let (repository, tag) = reference.rsplit_once(':')?;
        (!tag.contains('/')).then_some((repository, tag))
    })?;
    let sound = |part: &str| !part.is_empty() && part != "." && part != ".." && !part.contains('/');
    (repository.split('/').all(sound) && sound(version)).then_some((repository, version))
}

/// A file name for a VM name: bytes outside `[A-Za-z0-9_-]` (and a leading dot) become `%XX`.
fn encode_name(name: &str) -> String {
    let mut out = String::new();
    for (index, byte) in name.bytes().enumerate() {
        let plain = byte.is_ascii_alphanumeric()
            || byte == b'_'
            || byte == b'-'
            || (byte == b'.' && index > 0);
        if plain {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, Fail> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Fail::fake(&format!("{} is corrupt: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io("cannot read a state file", &e)),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), Fail> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| Fail::fake(&format!("cannot encode state: {e}")))?;
    write_atomic(path, &bytes)
}

/// Writes `bytes` to a temp file beside `path`, syncs it, and renames it over `path`.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Fail> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = File::create(&tmp)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()))
        .and_then(|()| fs::rename(&tmp, path));
    result.map_err(|e| {
        drop(fs::remove_file(&tmp));
        io("cannot write a state file", &e)
    })
}

#[cfg(test)]
mod tests {
    use super::{Home, State, encode_name};
    use crate::vm::{Source, VmRecord};

    fn open(dir: &tempfile::TempDir) -> Home {
        Home::open(dir.path().to_str()).unwrap()
    }

    #[test]
    fn should_refuse_when_tart_home_is_unset_empty_or_relative() {
        // given
        let inputs = [None, Some(""), Some("relative/dir")];

        for input in inputs {
            // when
            let failure = Home::open(input).err().expect("must refuse");

            // then
            assert_eq!(failure.code, 99, "{input:?}");
        }
    }

    #[test]
    fn should_refuse_a_directory_that_looks_like_a_real_tart_home_when_it_has_no_marker() {
        // given
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("cache")).unwrap();

        // when
        let failure = Home::open(dir.path().to_str()).err().expect("must refuse");

        // then
        assert_eq!(failure.code, 99);
        assert!(failure.message.contains("real Tart home"));
    }

    #[test]
    fn should_accept_a_directory_holding_only_a_fault_file_when_the_lab_wrote_it_first() {
        // given
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("faults.toml"), "").unwrap();

        // when
        let home = Home::open(dir.path().to_str());

        // then
        assert!(home.is_ok());
        assert!(dir.path().join("fake-tart.marker").exists());
    }

    #[test]
    fn should_reopen_a_home_it_made_when_it_now_has_vms() {
        // given
        let dir = tempfile::tempdir().unwrap();
        drop(open(&dir));

        // when
        let again = Home::open(dir.path().to_str());

        // then
        assert!(again.is_ok());
    }

    #[test]
    fn should_round_trip_state_and_vm_records_when_written_and_read_back() {
        // given
        let dir = tempfile::tempdir().unwrap();
        let home = open(&dir);
        let state = State {
            next_inv: 5,
            next_vm: 2,
            ..State::default()
        };
        let vm = VmRecord::new("mf/odd name", Source::Oci, "02:00:00:00:00:01".into(), 3, 9);

        // when
        let _lock = home.lock().unwrap();
        home.write_state(&state).unwrap();
        home.write_vm(&vm).unwrap();

        // then
        assert_eq!(home.read_state().unwrap(), state);
        assert_eq!(home.read_vm("mf/odd name").unwrap(), Some(vm.clone()));
        assert_eq!(home.list_vms().unwrap(), vec![vm]);
        assert_eq!(home.read_vm("absent").unwrap(), None);
    }

    #[test]
    fn should_remove_a_vm_record_when_asked() {
        // given
        let dir = tempfile::tempdir().unwrap();
        let home = open(&dir);
        home.write_vm(&VmRecord::new("a", Source::Local, "m".into(), 2, 1))
            .unwrap();

        // when
        home.remove_vm("a").unwrap();

        // then
        assert!(home.list_vms().unwrap().is_empty());
        assert!(home.remove_vm("a").is_err());
    }

    #[test]
    fn should_report_corruption_when_a_state_file_is_not_json() {
        // given
        let dir = tempfile::tempdir().unwrap();
        let home = open(&dir);
        std::fs::write(dir.path().join("state.json"), "{").unwrap();

        // when
        let failure = home.read_state().expect_err("corrupt");

        // then
        assert!(failure.message.contains("corrupt"));
    }

    #[test]
    fn should_encode_names_when_they_hold_path_characters() {
        // given
        let names = ["plain-name_1", "ghcr.io/x/y:1", ".hidden", "a b"];

        // when
        let encoded: Vec<String> = names.iter().map(|n| encode_name(n)).collect();

        // then
        assert_eq!(
            encoded,
            ["plain-name_1", "ghcr.io%2Fx%2Fy%3A1", "%2Ehidden", "a%20b"]
        );
    }
}
