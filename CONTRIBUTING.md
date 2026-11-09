# Contributing

Thanks for your interest. The project is pre-alpha. See [doc/tasks.md](doc/tasks.md) for current work.

Planned work is done by the maintainer and agents as one signed-off commit per task. Outside
contributions are welcome as pull requests; for anything larger than a small fix, open an issue
first so the design can be agreed before you write code.

## Sign off your commits (DCO)

Sign off every commit under the [Developer Certificate of Origin](https://developercertificate.org/):

```text
git commit -s
```

That adds a `Signed-off-by:` line with your name and email. Use short imperative subjects, with a
body that explains why when it is not obvious.

## Build and test

The Rust toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it on first use. Until
`cargo xtask` lands, the gate is:

```text
cargo fmt --all --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked
```

## Architecture and decisions

Read [doc/plan.md](doc/plan.md) (the workspace and dependency law is §4). Crate boundaries and the
purity of the domain crates are enforced by checks once they land; fix the structure instead of
loosening a lint or a test.

## Pull requests

- Do not suppress, skip or delete tests or lints to get green; fix the cause.
- Add tests for behaviour changes, named `should_<outcome>_when_<condition>` with `// given`,
  `// when` and `// then` sections.
- Keep a pull request to one change, and do not include secrets, personal paths or private
  content: fixtures use synthetic content.
- New dependencies are added with `cargo add` and need a reason in the description.

## Reporting problems

Security issues must be reported privately; see [SECURITY.md](SECURITY.md).

## Licensing

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as `MIT OR Apache-2.0`, without any additional terms or conditions.

By participating you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).
