# xtask

Repository automation for mayfly, run as `cargo xtask <command>`: the gate (`quick`, `check`,
`test`, `security`), the architecture checker (`architecture`, policy in `architecture.toml`,
fixtures in this crate's `tests/architecture-invalid` and `tests/architecture-valid`), the pinned-tool
doctor (`doctor`, `install-tools`, pins in `tools/versions.toml`), ADRs (`adr new`,
`adr index`), `land` and `list-checks`.

Ported from [site-preview](https://github.com/pzmi/site-preview) (same owner, MIT OR
Apache-2.0) and adapted to mayfly's crates (D3).
