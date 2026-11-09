# 0046. Config schema v1 reads raw TOML, secret references live in the kernel, exit codes

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-10).

## Context

RT-01 builds the configuration of plan §11 and the state-dir lock of plan §13. Three choices
the task text leaves open shape what SEC-02, RT-02, RT-03 and OPS-01 build on: how the file is
read (the operator must be told every problem, by field, with the fix), where the parsed
secret reference lives (the `SecretProvider` port is in `mayfly-control`, which cannot depend on
`mayfly-composition`), and which numbers the distinct exit codes of ADR 0031 take.

## Decision

1. **The file is read as a raw TOML table and walked key by key**, not through `serde`
   derive. Every problem is collected and reported with its field path (`pools[1].tier`), what
   is wrong and the fix; unknown keys are refused at every level with the list of valid ones;
   `allow_untrusted_prs` (ADR 0027) and `mode` (ADR 0028) are refused with their reason. A
   refused secret reference and a TOML syntax error never repeat the text they refused (it may
   be a key pasted by mistake). Keys are never renamed; a breaking shape raises
   `schema_version`.
2. **`SecretRef` (ADR 0033's four URI schemes) is a kernel type in `mayfly-domain`**, parsed
   by `FromStr`, so the config reader, the `SecretProvider` port (SEC-02) and its adapters share
   one parser. It is the reference only; the resolved value and `Secret<T>` stay SEC-02's.
3. **Exit codes** (`mayfly_composition::exit_code`), sysexits values, never renumbered: invalid
   or unreadable config 78, lock refused 69, state dir unusable 73. OPS-05 adds the watchdog's.
4. **The lock is `flock(2)` on `<state_dir>/mayflyd.lock`** through `rustix` (no `unsafe`, no
   `libc`), non-blocking and exclusive; the state dir must be owned by the daemon's user and
   `0700`, the lock file `0600`, a wider mode is refused with the `chmod` that fixes it, never
   repaired silently.
5. **Defaults** a pool takes when a key is absent: `min_prepared` 0, `max_prepared_age` 1h,
   `breaker_failures` 3 in `breaker_window` 30m; `[host]` reserves 4096 MiB memory and 50 GiB
   disk. Conservative placeholders until the first real host gives numbers; changing a default
   changes behaviour and is a new ADR.

## Consequences

- RT-01 implements it; `tests/config.rs`, `tests/state_dir.rs` and the golden fixtures under
  `crates/mayfly-composition/tests/fixtures/config/` pin it.
- SEC-02 uses `mayfly_domain::SecretRef`; RT-02 maps `ConfigErrors`, `LoadError` and
  `StateDirError` to exit codes through their `exit_code()`; OPS-01's unit file treats 69 as
  "do not restart".
- A symlinked state dir is followed (the target is checked); a symlinked lock file is refused.

## Alternatives considered

- `serde` derive with `deny_unknown_fields`: one error at a time, no field path, the source
  line echoed.
- `SecretRef` in `mayfly-composition`: SEC-02's port in `mayfly-control` could not name it.
- A pid file: stale after a crash.
