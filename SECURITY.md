# Security Policy

## Supported versions

No version has been released yet. Only the latest commit on `main` is considered supported.

## Reporting a vulnerability

Please report privately using GitHub private vulnerability reporting: open the repository's **Security** tab and choose **Report a vulnerability** (<https://github.com/pzmi/mayfly/security/advisories/new>). Do not open a public issue for security problems.

## Scope

In scope, as the design develops (see [doc/plan.md](doc/plan.md)):

- Job-to-job state leaking between runner VMs
- Deleting a VM or image that mayfly does not own
- Exposure of GitHub App keys, installation tokens or JIT runner configurations
- Escape from the documented control-plane and data-plane trust boundary

## Non-goals

- mayfly does not sandbox the workflow code itself beyond the VM boundary the provider gives.
- Compromise of the host operating system or of the VM provider.
