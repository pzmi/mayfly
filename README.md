# mayfly

A headless Rust service that runs single-use GitHub Actions runners in disposable virtual machines on Apple Silicon hosts. Each job gets a freshly cloned VM, and the VM is destroyed after one job. Demand comes from GitHub Runner Scale Sets (JIT runners); the VM layer is an abstraction whose first provider is [Tart](https://tart.run/).

Why the name: a mayfly lives for a day. So does every runner VM here: one job, then gone.

> **Status: pre-alpha, under construction.** The workspace and crate stubs exist; nothing runs jobs yet. There is no release.

## Design

- [doc/plan.md](doc/plan.md): what mayfly is, how it is structured, the workspace and dependency law (§4).
- [doc/philosophy.md](doc/philosophy.md): why it is built this way and the lines it does not cross (a draft).
- [doc/foundation.md](doc/foundation.md): the engineering rules; [doc/verification.md](doc/verification.md): how it is tested.
- [doc/domain-model.md](doc/domain-model.md): the language and boundaries; [doc/adr/](doc/adr/README.md): the decisions.
- [doc/tasks.md](doc/tasks.md): the task order, decisions and acceptance criteria.

## Development

The toolchain is pinned in `rust-toolchain.toml`. The gate is `cargo xtask check`; `cargo xtask quick`
is the faster loop while working, and `cargo xtask doctor` checks the pinned tools.

## Licence

Dual licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
