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

The Rust toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it on first use. The other
tools the gate needs are pinned in `tools/versions.toml`:

```text
cargo xtask doctor          # checks every pinned tool; run it first when local and CI disagree
cargo xtask install-tools   # installs the ones xtask can install
cargo xtask quick           # fmt check, architecture, clippy, fast tests: while working
cargo xtask check           # the full blocking gate: before every commit
```

`quick` is not `check`; a pull request says which stages it ran. `cargo xtask check --only
<stages>` runs named stages through the same code CI runs.

## Architecture and decisions

Read [doc/plan.md](doc/plan.md) (the workspace and dependency law is §4). Crate boundaries and the
purity of the domain crates are enforced by `cargo xtask architecture`; fix the structure instead
of loosening a lint or a test. A change that adds or reverses an architectural decision adds an
ADR (`cargo xtask adr new "<title>"`) or amends or supersedes one, in the same change, together
with the tests that pin it. [doc/philosophy.md](doc/philosophy.md) (a draft) says which lines
mayfly does not cross; a new feature, default, metric or dependency passes its decision test (§8).

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
