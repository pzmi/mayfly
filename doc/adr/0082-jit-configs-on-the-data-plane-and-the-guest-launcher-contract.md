# 0082. JIT configs on the data plane and the guest launcher contract

Date: 2026-11-11

## Status

Accepted (owner, 2026-11-11)

## Context

ADR 0038 delivers a JIT config with `tart exec -i <exact VM> <fixed in-image launcher>` on stdin
only, after checking that the process holding the VM's run lock is its shim's child and that its
instance id matches. Three things it leaves open are DP-05's to settle:

- **Where the data plane gets the config.** Plan §3.2, ADR 0010 and R14 say the link adapter
  resolves the `SecretHandle` from the control plane's broker at send time into a wire frame
  that is not `Serialize`; the `GuestTransport` port takes only the handle, and nothing on the
  data plane holds the config yet.
- **What "the instance id matches" is compared with.** `LaunchRunner{vm_name, launch}` carries
  no instance id, and the data plane keeps nothing durable (plan §3.4): it cannot know the id
  `VmCloned` reported without the control plane telling it. The control plane already treats a
  `proof_mismatch` refusal of a launch as a quarantine (`provider_identity_mismatch`, KER-04F).
- **What the in-image launcher answers**, and where Tart's run lock is. A check made only on the
  host (shim, lock, `tart get`) still hands the config to whichever guest answers the exec: one
  Tart routed elsewhere, or a VM re-cloned under the same name between the check and the exec.

## Decision

1. **A data-plane store by handle.** `mayfly-data` names a port `LaunchSecrets` and its store
   `LaunchStore`: the JIT configs held for delivery, in memory, each bound when it is put to the
   exact VM, the recorded instance id and a deadline. A take for another VM or instance id is
   refused and discards the config; a second put for a handle held or already taken is refused;
   the store's owner drops what passed its deadline; the guest transport discards the handle
   however its launch ends, so nothing lingers once the directive has ended. The link's
   data-plane end puts each config it receives in the non-`Serialize` wire frame there (RT-02 or
   the link's task wires it). Nothing journals, logs or persists it; a restart empties it
   (ADR 0014).
2. **The launch carries the recorded instance id.** Protocol 1.3 (`GUEST_IDENTITY`) adds an
   optional `instance_id` to `LaunchRunner`, which the control plane fills from the attempt's
   `VmCloned`; a session below 1.3 is written without it. A data plane refuses a launch without
   it `proof_mismatch` (no delivery unchecked, ADR 0038 decision 2), and a failed check right
   before or during the delivery is `proof_mismatch` too: the executor's effect-stage stimulus
   `IdentityRefused`, which the control plane quarantines `provider_identity_mismatch` (a failed
   challenge stays `guest_identity_refused`).
3. **The run lock** is Tart's: the POSIX record lock `tart run` holds on
   `$TART_HOME/vms/<vm>/config.json`, read with `fcntl(F_GETLK)`. The host check passes only
   when the VM's shim is alive (ADR 0037), the lock's holder is the `child_pid` of the shim's
   `shim.json`, and `tart get` lists the VM running with the recorded MAC. It runs first, before
   the launcher's status is read.
4. **The launcher contract**, at `/usr/local/libexec/mayfly/launcher` in every image:
   - `exec -i <vm> <launcher>`: a handshake on the launch exec itself. The transport writes
     `<nonce> <expected mac>` and reads one answer line; only `ok <nonce> <expected mac>` lets
     it write the JIT config, which the launcher reads to end of file and hands the runner in
     `ACTIONS_RUNNER_INPUT_JITCONFIG` (exit 0). A guest with another MAC answers `mismatch`
     (exit 5) and gets nothing: `IdentityRefused`. `launched <nonce> <mac>` (exit 3) means a
     runner was already started: `Delivered`, with no second delivery. Once any byte of the
     config was written, every failure (another exit, a broken pipe, a deadline) is ambiguous,
     `RunnerLaunchUnknown` (I22), never a definite refusal or a delivery.
   - `exec -i <vm> <launcher> verify`: reads a nonce on stdin and prints one JSON line
     `{"v":1,"nonce","os","uptime_ms","mac"}`; verified only when it echoes the nonce and names
     the MAC Tart lists for the VM. The claims are parsed into typed, bounded values.
   - `exec <vm> <launcher> status`: its status file (`none`, `running`, or `exited` with
     `code`), which outlives the daemon.
   - `exec <vm> <launcher> diagnostics`: the runner's log, exported bounded and redacted
     (control characters dropped first, then a line naming the JIT variable, a configured
     secret, a GitHub token, a `Bearer` credential or a long base64 run masked whole; one
     function, `mayfly_data::redact_log`, for the real and the fake transport), written `0600`
     in a directory the daemon owns, made `0700`.
5. **The transport lives in `mayfly-adapter-tart`**, beside the process runner and the shim
   records it reuses. The empty `mayfly-adapter-guest` crate (whose SSH description ADR 0038
   superseded) is removed, with its policy entries.

## Consequences

The `GuestTransport` contract (GT-01..GT-14, `mayfly-testkit-data`) runs against the in-memory
guests and against the Tart transport over `mayfly-fake-tart`, whose foreground `run` now holds
the run lock and whose `exec` emulates the launcher and its handshake; `UncheckedLockTransport`
fails GT-05, GT-06 and GT-12. A launch with no config held for it (no frame, another VM,
expired) is a definite failure, which retires the attempt. A 1.2 control plane cannot launch
through a 1.3 data plane (every launch is quarantined): both planes ship in one binary in v0.x,
so the pair is never mixed in practice; a split deployment upgrades the control plane first.
HW-01 confirms on real Tart that `tart run` holds that lock and `F_GETLK` reads its pid, the
exit code and message of an unreachable agent, and the launcher's behaviour in a real image
(OPS-02 builds it). The link's data-plane end filling `LaunchStore` is still to be built.

## Alternatives considered

- The data plane compares the MAC with the one it last listed: a VM replaced under the same name
  passes. The id must come from the journal.
- A host-only check before the exec: the config still goes to whichever guest answers.
- The data plane fetches the config from the control plane by handle: a second channel and a
  request path for secrets; the link already carries the frame.
- Pass the config to the runner as `--jitconfig`: visible in the guest's process list.
