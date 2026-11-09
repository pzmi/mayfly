# 0038. JIT delivery through tart exec with a checked guest identity

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

A JIT config must reach exactly the guest of this attempt. Per-image SSH host keys identify the
image, not the clone, and an IP address can be reused by another VM after a delete, so an
address-based channel can hand a secret to the wrong guest. `tart exec` talks to the guest agent
through the VM's own control socket and virtio-vsock, addressed by VM name, with no network
identity (`doc/domain-model.md` OD5). ADR 0013 allowed SSH with a pinned per-image key.

## Decision

1. The JIT config is delivered with `tart exec -i <exact VM name> <fixed in-image launcher>` and
   travels **only on stdin**; argv names the VM and the launcher path, nothing else.
2. Immediately before the exec, the data plane checks that the PID holding the VM's run lock is
   the child of that VM's shim (ADR 0037) and that the instance id matches (ADR 0026); any
   mismatch refuses delivery and quarantines the attempt.
3. Every clone gets a random MAC (ADR 0026); `TART_HOME` is `0700` and owned by a dedicated
   service user, so the control sockets are reachable by mayfly alone.
4. After delivery the control plane confirms through the GitHub API that the runner id issued
   with the JIT config came online; otherwise the registration is presumed leaked (P13).
5. **No SSH** in v0.x. Images without the Tart guest agent are refused at verification.

## Consequences

Partly supersedes ADR 0013 (its SSH path); Tart by argv stands. DP-05 (transport, launcher
contract, lock check), CP-05 and KER-09 (online confirmation), OPS-02 (manifest requires the
guest agent) implement it; F39 and mutant 20 pin it. `tart exec` needs a macOS 14+ host.

## Alternatives considered

- SSH with a per-clone host key learned at first boot: a second channel and key management.
- SSH with a per-image pinned key: a confused deputy (any clone of the image answers).
