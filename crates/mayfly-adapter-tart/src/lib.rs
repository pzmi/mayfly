//! `mayfly-adapter-tart` (data plane): the `VmProvider`, `ImageStore` and `GuestTransport` ports
//! over the Tart CLI (plan §8.2, §8.3, D12).
//!
//! - [`provider`]: `TartProvider`, the port's implementation, bounded by per-call deadlines; it
//!   clones only from a base the record of materialised bases names (ADR 0039, ADR 0087).
//! - [`images`]: `TartImageStore`: pull by digest with full content verification, single
//!   flight per digest, a base's state machine through one boot and `tart exec`, eviction by
//!   identity and dependents, crash recovery by observation.
//! - [`bases`]: the durable registry of the bases this deployment made, and their states.
//! - [`proof`]: what a base proves before it is cloned from or deleted.
//! - [`verify`]: an image's layout, its files and chunks, fingerprints, private directories.
//! - [`oci`]: the raw manifest fetched by digest independently of Tart, over HTTPS with a size
//!   cap.
//! - [`guest`]: `TartGuestTransport`, guests through `tart exec` only (ADR 0038, ADR 0082): the
//!   nonce challenge, the launch after the run-lock and instance-id check, the launcher's
//!   status, bounded and redacted diagnostics.
//! - [`tart`]: the `tart` commands it runs; [`parse`]: their output, against version-pinned
//!   fixtures.
//! - [`process`]: the process runner (argv only, cleared environment plus an allowlist,
//!   deadline, process-group kill, bounded and redacted output), the shim's launch, and the
//!   shim itself, the body of `mayfly vm-shim` (ADR 0037).
//! - [`shim`]: a shim's directory and the re-adoption inputs it gives a restarted data plane.
//! - [`config`]: what composition passes in.
//! - [`floor`]: the external epoch floor in `TART_HOME` (ADR 0026), the data plane's `FloorStore`.
//!
//! Never a real `tart` in tests: the contract runs against `mayfly-fake-tart`, and against real
//! Tart only in the hardware lane (HW-01).

pub mod bases;
pub mod config;
pub mod floor;
pub mod guest;
pub mod images;
pub mod oci;
pub mod parse;
pub mod process;
pub mod proof;
pub mod provider;
pub mod shim;
pub mod tart;
pub mod verify;

pub use bases::BaseRecords;
pub use config::{Deadlines, ShimProgram, TartConfig};
pub use floor::{FloorFile, FloorUnreadable};
pub use guest::{GuestConfig, GuestDeadlines, TartGuestTransport};
pub use images::{ConfigError, ImageConfig, ImageDeadlines, RunnerPlatform, TartImageStore};
pub use oci::{FetchError, ManifestSource, RegistryManifests};
pub use provider::{ProbeError, TartProvider};
pub use shim::ShimReading;
