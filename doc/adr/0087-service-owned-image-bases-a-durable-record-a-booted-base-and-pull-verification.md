# 0087. Service-owned image bases: a durable record, a booted base and pull verification

Date: 2026-11-11

## Status

Accepted (owner, 2026-11-11), after the owner's review of the first draft and two
independent reviews of its implementation

## Context

ADR 0039 has `CloneVm` clone only from a "local, service-owned" base (`mfb-…`), materialised by
pulling an image by digest, cloning a base named by digest and runner version, booting it once
and installing the pinned runner through `tart exec`. DP-04's `TartProvider::clone_vm` checked
only that the base was a local VM. DP-07 builds the `ImageStore` and must decide what makes a base
ours, how its content is known to be the image's, how a crash at any step is recovered, and what
it trusts. The properties are distinct, and each needs its own evidence:

| Property | Evidence |
|:--|:--|
| Source identity | the OCI digest |
| Publisher authenticity | a signature or attestation (policy below) |
| Local creation | a durable record of the operation, written ahead of each step |
| VM identity | file identity and fingerprint read from the filesystem; the MAC only as a secondary check |
| Integrity | content verified against the manifest, and protected from modification |
| Build completion | a durable `Ready` written only after verification |
| Safe deletion | ownership, stopped, no remaining dependents |

What Tart gives (2.28.0, the pinned version): a name-based CLI and nothing else to drive it by.
A MAC can be copied or set, so it proves neither ownership nor integrity. `pull` names the cache
entry by the hash of whatever manifest it was served and never compares it with the requested
digest; it caches the manifest re-serialized (`manifest.toJSON()`), so hashing the cached copy
proves nothing; it checks no blob, and DiskV2 checks uncompressed chunk digests only on a resumed
pull. Disk v1 carries no per-chunk uncompressed digests. Tart keeps no clone lineage and no
labels that survive `clone`. Against anyone who can write `TART_HOME`, a check followed by a
name-based `tart clone` or `tart delete` cannot be made atomic.

Deployment (owner, 2026-11-11): the primary mode is **terminal mode**: the owner runs `mayflyd`
from a terminal as their own user and stops it with Ctrl-C; nothing is auto-started. Linux guests
(arm64 runners) matter as much as macOS ones.

## Decision

1. **Trust model.** Root and the daemon's own user are trusted. Anyone else able to write mayfly's
   `TART_HOME` is outside the supported model: Tart's name-based CLI cannot close the
   check-then-act gap against them. mayfly owns its own `TART_HOME`, apart from the user's
   `~/.tart`: a `0700` directory under its state directory (RT-02 places it). At start-up the image
   store checks that `TART_HOME` and the registry's directory are directories (not symlinks) owned
   by the daemon's user with no access for group or others, that `vms/` and `cache/` are writable
   by no one else, that no component of the path as configured is a symlink (or `.`/`..`), and
   that every ancestor is owned by root or the daemon's user and is not writable by group or
   others unless sticky; it refuses to start otherwise (`verify::check_private`). The path and
   its ancestors are checked before the registry's directory is created, and every file the
   store hashes is opened with `O_NOFOLLOW`. **Service mode** (a launchd daemon under a dedicated macOS user, the
   same checks) is a documented option for a future server; whether Tart runs VMs under a launchd
   daemon without a GUI session is for HW-01.
2. **The registry of bases** (`mayfly-adapter-tart::bases`) is the source of ownership truth: one
   JSON file in a private directory of the state directory, written to a temporary file, synced,
   renamed, then the directory synced. Each entry holds: `deployment_id`; `base_id` (random,
   distinct from the name and the MAC); `source_ref` and the full `source_digest` (the twelve
   digits in the name are only a name: another digest under the same name is refused); the
   `tart_version`; `runner_version` and `runner_sha256`; `build_recipe_version`; the `state`; the
   MAC (secondary); the file identity (the VM directory's and disk's device, inode and birth
   time); the final fingerprint (each file's identity, size and change time: config, NVRAM, disk,
   saved state); the signature policy and result; and the clones made from it, as references or as
   unresolved intents. The verified raw manifest of each image is kept beside it and re-checked
   against its digest on every read. **A corrupt registry fails closed** (every clone,
   materialisation, eviction and listing refused); **a missing one owns nothing** (nothing is
   cloned from or deleted), and nothing is ever rediscovered by name.

   **Why on the data plane, not in the control plane's journal.** A delete's proof must be a
   local host fact (the file identity, the fingerprint, the MAC, what Tart lists now), decided by
   the data plane without the control-plane link, which may be down when a recovery or an
   eviction runs; it must be written synchronously, and durably, before each host effect it
   permits, which a round trip to the journal cannot promise; and it changes with every effect
   (states, clone intents, fingerprints), which does not belong in immutable events. The journal
   keeps the decisions (`ImagePlacement`, KER-11); this record keeps the host's evidence. If it
   grows (many bases, a history of fingerprints), a host-local SQLite store with the same write
   rules is the upgrade path.
3. **One writer.** Record writes are serialised; clone admission and eviction take one admission
   lock, so an eviction never reads the dependents while a clone is being added; one
   materialisation runs per base.
4. **The state machine**, each state written before the step it permits: Reserved (the name is
   absent from a complete listing) → Cloning (`tart clone <repo>@<digest> <base>`) → Customizing
   (its own MAC by `tart set --random-mac` and its file identity recorded; the clone's files
   verified against the manifest before the first boot; one boot under the per-VM shim; the runner
   installed and checked through `tart exec`) → Verifying (stopped) → Ready (the final fingerprint
   recorded) → Retiring → deleted; plus Quarantined. **Ready is written only after the stop, the
   runner check and the final fingerprint.** Crash recovery is by observation, from what persists:

   | Crash point | Recovery |
   |:--|:--|
   | Before the intent | nothing recorded: no authority |
   | Reserved | retried only when a complete listing shows the name absent; present: quarantined |
   | After the clone, before its MAC (Cloning, present) | unproven: preserved and quarantined, never deleted |
   | Cloning, absent | back to Reserved, retried |
   | Customizing or Verifying (before or during the install, after it, before Ready) | never cloned from; proven: stopped with `tart stop` if a crash left it booted, then retired, deleted and rebuilt; identity changed: quarantined |
   | Ready | revalidated before every use |
   | After a job clone's intent, before its instance id | the intent stays and blocks eviction while a VM of that name is listed |
   | Retiring, or after the delete before its record | an observed complete absence settles it; proven and present: deleted again |

5. **Clone only from Ready, after revalidation** (`proof::ready_for_clone`, in
   `TartProvider::clone_vm` itself, under the admission lock): the registry names the base Ready
   by the current recipe, Tart lists it stopped with the recorded MAC, and its file identity and
   fingerprint are the recorded ones. A base that fails is quarantined. The clone's name is
   recorded as an intent before `tart clone` and resolved to its instance id after. An `mfb-*` VM
   the registry does not hold is never cloned from. Absence is always read from a complete
   `tart list` (never a per-name `tart get`), under the base's hold and the admission lock.
6. **Eviction protects mayfly's own clones only**: a base is deleted only when it is Ready (a
   Quarantined base is never deleted by mayfly, nor its entry dropped, whatever its identity or
   presence: only an operator resolves it), its recorded file identity and MAC hold, it is
   stopped, and no recorded clone or unresolved intent is still listed by Tart. Eviction writes
   Retiring, then deletes; the delete (`retire`, shared with recovery) checks the same
   dependents again, although a base that is not Ready has none by construction. That is sound under the dedicated `TART_HOME`; its limit is stated: Tart keeps
   no clone lineage, so a clone made outside mayfly is invisible (it keeps its own copy-on-write
   data). The control plane's lease count (P17) stays the first check.
7. **Full verification of every image**, never from the cached manifest:
   1. the raw manifest is fetched independently by digest over HTTPS (the distribution API with
      the anonymous token flow, reqwest over rustls) and its exact bytes are checked against the
      digest, before `tart pull`; the body is read chunk by chunk and abandoned past 4 MiB (a
      larger stated length is refused before any byte is read); no redirect is followed and one
      deadline bounds the whole fetch. The registry's token challenge is never trusted as a
      destination: its realm must be HTTPS on the registry's host or a host the operator listed,
      never an IP literal, its scope exactly `repository:<path>:pull`, and the token request
      resolves only to public addresses (loopback, link-local, private, shared and unique-local
      refused after resolution); the token is a `Secret`, sent only as the bearer credential.
      Stated limit: a name may resolve differently between that check and the connection the
      resolver hands over (the resolver's answer is the one connected to, so only a resolver
      outside the daemon could change it);
   2. its layout must be Tart's disk v2: exactly one config and one NVRAM layer, at least one
      disk-v2 chunk each with its uncompressed size and digest; disk v1, ASIF overlays and unknown
      media types are refused, before `tart pull`;
   3. after `tart pull`, the cache entry must be a directory (never Tart's symlink to another
      entry), `config.json` and `nvram.bin` must match their descriptors, and every chunk of the
      reconstructed `disk.img` must match its uncompressed size and digest;
   4. the same files are checked again on the fresh `mfb-` clone before its first boot (its
      config compared field by field, the MAC aside).

   On failure the cache entry is deleted (with the entry Tart linked it to, when it named the
   requested digest a link to another manifest's entry: the link's target, absolute or relative,
   is read by its last component and must stay in the same cache directory) and pulled once
   more; a second failure
   is refused. Every hash runs on Tokio's blocking pool, never on a runtime worker.
   Re-checking the compressed blobs is a follow-up: it needs a second download, and the content
   they decompress to is already verified.
8. **Signature policy per pool**, recorded with the result next to the source digest:
   `digest_only` (the default; works for unsigned images), `optional`, `required`. The digest is
   always required. DP-07 records the policy and refuses `required` at configuration
   (`ConfigError::SignaturesUnsupported`) until a verifier exists; `optional` records
   `not_checked`. Verifying signatures (a key, or a certificate identity and issuer, as the trust
   anchor) is SIG-01: it is not small (a sigstore client or a `cosign` child) and nothing the owner
   pulls today is signed.
9. **The shim may run an image base** for its one boot while it is materialised: `ShimTarget` is
   a managed `mf-*` name or an `mfb-*` base name, never anything else; the boot ends by `tart stop`.
10. **The runner is installed by the launcher**: `tart exec <base> <launcher> install-runner
    <version> <sha256>` (ADR 0082's launcher gains the verb), which checks the package against the
    pinned checksum and answers the version it installed; the adapter compares it.
11. **The `ImageStore` port's `list`** reports the verified, unchanged images and only the Ready
    bases that pass the clone proof, with their guest (macOS or Linux); any other `mfb-*` VM is
    counted by the ledger from the inventory, in full (DP-06F), so each base is counted once.

## Consequences

- Model first: `formal/image_store.qnt` models one base, its states, crash points and the
  uncontrolled writers, with `cloneAllowedOnlyWhenOwnedReadyIntact` (CloneAllowed ⇒ Owned ∧ Ready
  ∧ IntegrityValid) and `deleteAllowedOnlyWhenOwnedStoppedFree` (DeleteAllowed ⇒ Owned ∧ Stopped ∧
  NoUnresolvedDependents ∧ not Quarantined), negative controls (an eviction ignoring dependents,
  the state guard or the identity proof; a clone skipping the fingerprint), and its conformance
  seam named in
  `doc/formal-models.md` (the driver is VER-08). Eviction is two steps in the model (Retiring,
  then the delete), so a crash between them is checked.
- `mayfly-adapter-tart` depends on reqwest, rustls and rustls-platform-verifier (already the GitHub
  adapter's: aws-lc-rs, the platform verifier) for `RegistryManifests`; `TartImageStore` takes a
  `ManifestSource`. A distribution client crate (oci-client) was tried and dropped: its raw
  manifest pull buffers the whole body before any size check. Registry credentials are a broker secret class
  (domain-model §9.3), wired when a private registry needs them.
- `TartProvider::new` takes the shared registry; composition builds one `BaseRecords`, gives it to
  the provider and the store, and runs `TartImageStore::recover` at start-up (RT-02).
- Pinned by the `ImageStore` contract (`mayfly-testkit-data::image_contract`, IC-01..IC-14, IC-14 a
  Linux base) on `MemoryImages` and over `mayfly-fake-tart`; `TagPullStore` fails IC-01 and IC-02,
  `BaseEvictingStore` IC-10, `NameEvictingStore` IC-11 and IC-12; VP-17 (clone needs an owned
  base); and the fault cases of
  `crates/mayfly-adapter-tart/tests/tart/images.rs`: a wrong manifest from the registry, a
  re-serialized cached manifest, a corrupt config, NVRAM or chunk, an unchecked compressed digest
  (the stated boundary), disk v1, a crash at each state and after the clone before its MAC, a
  base modified after it was verified, a look-alike with the same name and MAC, concurrent clone
  and evict, an unresolved intent, an incomplete listing, unrecorded clone lineage (the stated
  limit), a lost or corrupt registry, a quarantined base never evicted, a crash while the base
  was booted, a digest Tart linked to another manifest's entry, and an unsafe directory layout
  (open modes, a writable ancestor, a symlink anywhere in the path, another owner: the start-up
  check that is the boundary for a directory swapped between check and effect).
- `mayfly-fake-tart` lays out pulls as Tart 2.28.0 does (an entry named by the served manifest's
  hash, a symlink for another requested name, a re-serialized manifest, no blob checks), keeps
  each VM's `config.json`, `nvram.bin` and `disk.img`, and its launcher has `install-runner`.
- The data plane's durable state is now the epoch floor (ADR 0026), the shims' files (ADR 0037)
  and this registry of image bases, a host fact and never journal state: plan §3.1, ADR 0007's
  note, domain-model §4, foundation §6 and `mayfly-data`'s crate docs say so (`CLAUDE.md` is the
  orchestrator's to update).
- HW-01 confirms on real Tart: pull by digest and its OCI listing's name; the form of the link
  2.28.0 writes from a requested digest to another manifest's entry (absolute or relative) and
  whether `tart list` and `tart delete` know that entry by its digest; the cache layout
  (`cache/OCIs/<repo>/<digest>/` with `config.json`, `nvram.bin`, `disk.img`), that `tart clone`
  from the cached reference does not reach the registry, and that it leaves the base's files'
  change times alone; `tart exec` in a booted base and the launcher's `install-runner`, on a macOS
  and on a Linux image (for example a Cirrus Ubuntu image with the guest agent); whether Tart runs
  VMs under a launchd daemon without a GUI session (service mode).
- The Tart pin stays 2.28.0 here; TART-01 moves it to 2.41.1 and re-checks what this record
  assumes.

## Upstream Tart proposals (listed, not filed)

- Immutable instance UUIDs in `tart get` and `tart list`.
- Atomic compare-and-act: `tart clone --expect-source-mac`, `tart delete --expect-mac` (or by
  UUID), under Tart's own lock.
- Clone lineage recorded on the clone.
- Ownership metadata that `clone` does not inherit (labels cleared or preserved explicitly).
- Pull verification: the manifest against a by-digest reference, every blob against its
  digest and size, every decompressed chunk against its uncompressed digest, and never a link from
  `@sha256:D` to another digest's entry.
- A way to seal and identify a stopped VM (`tart verify <vm>` against its stored manifest).

## Alternatives considered

- Ownership from the name alone, or the MAC alone: either can be made by anyone.
- A marker inside the VM's Tart directory: Tart drops unknown config keys, and any marker there
  is writable by whoever can write the directory.
- Hashing the cached `manifest.json`: Tart 2.28.0 re-serializes it.
- Pulling the whole image with a distribution client instead of Tart: re-implements Tart's cache
  format.
- Hashing the whole base before every clone: tens of seconds per 100 GB per job, and still a
  check-then-act gap; the fingerprint gives the same evidence against the trusted writers' mistakes.
- Requiring signatures by default: no stock Tart image is signed.
- Booting the base with a bounded `tart run` killed at its deadline: signals `tart run`, which
  ADR 0037 forbids.
