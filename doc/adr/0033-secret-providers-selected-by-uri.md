# 0033. Secret providers selected by URI

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

The GitHub App key and registry credentials must come from somewhere. A laptop has the Keychain;
a headless server usually has a file, an environment or a secret manager's CLI. Plan §7.1 offered
a `0600` file or the Keychain as two fixed stores (`doc/domain-model.md` OD20). Envoy separates
where a secret is referenced from where it comes from (its secret discovery service); the owner
wants the same.

## Decision

A `SecretProvider` port (control plane; adapters in `mayfly-adapter-secrets`) resolves a
**reference URI** to a `Secret<T>` in memory. Config holds only references, chosen per secret:

| Scheme | Source | Refused when |
|:--|:--|:--|
| `keychain:<service>/<account>` | macOS Keychain item | not macOS; item locked or absent |
| `file:<absolute path>` | a file owned by the service user, `0600`, in a `0700` directory | other owner, wider mode, symlink, relative path |
| `env:<NAME>` | the daemon's environment, read once at start | variable absent; never passed to a child |
| `exec:<absolute path>` | stdout of an operator command, argv only, no shell, deadline, bounded output | non-zero exit, timeout, empty output |

New schemes are new adapters with an ADR. Values are never persisted, logged, cached on disk or
passed to a child (ADR 0014); a provider is asked again on refresh, so rotation needs no
restart. The remaining OD20 items (socket peer-uid allowlist, actor through `sudo -u`, audit in
event envelopes) stay planner defaults in the domain model.

## Consequences

Amends ADR 0014 (where secrets come from). SEC-02 builds the port, the four adapters and the
`SecretProvider` contract suite (ver §5); RT-01 parses references; `doctor` resolves each
reference without printing it.

## Alternatives considered

- Two fixed stores (file and Keychain).
- A built-in client for one secret manager: a moving part `exec:` already covers.
