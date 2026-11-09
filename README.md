# mayfly

A headless Rust service that runs single-use GitHub Actions runners in disposable virtual machines on Apple Silicon hosts. Each job gets a freshly cloned VM, and the VM is destroyed after one job. Demand comes from GitHub Runner Scale Sets (JIT runners); the VM layer is an abstraction whose first provider is [Tart](https://tart.run/).

Why the name: a mayfly lives for a day. So does every runner VM here: one job, then gone.

> **Status: pre-alpha, under construction.** The workspace and crate stubs exist; nothing runs jobs yet. There is no release.

## Design

- [doc/plan.md](doc/plan.md): what mayfly is, how it is structured, the workspace and dependency law (§4).
- [doc/tasks.md](doc/tasks.md): the task order, decisions and acceptance criteria.

## Development

The toolchain is pinned in `rust-toolchain.toml`. Until `cargo xtask` lands, the gate is:

```text
cargo fmt --all --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked
```

## Licence

Dual licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
