#!/usr/bin/env bash
# Stop: refuse to finish a turn that leaves the fast gate red. Runs only when
# Rust code, manifests, the lockfile, the architecture policy, a pinned-tool
# file or a Quint model changed; a clean tree or a docs-only change stops at
# once, and so does a repo with no Cargo.toml yet (before FND-01) or a machine
# without cargo or git. `stop_hook_active` is true when Claude is already
# continuing because of this hook, so a gate that cannot go green never loops.
set -uo pipefail

payload="$(cat)"
if command -v jq >/dev/null 2>&1; then
  active="$(printf '%s' "$payload" | jq -r '.stop_hook_active // false' 2>/dev/null)"
elif command -v python3 >/dev/null 2>&1; then
  active="$(printf '%s' "$payload" | python3 -c 'import json,sys; print(str(json.load(sys.stdin).get("stop_hook_active", False)).lower())' 2>/dev/null)"
else
  exit 0
fi
[ "$active" = "true" ] && exit 0

cd "${CLAUDE_PROJECT_DIR:-$(dirname "$0")/../..}" || exit 0
[ -f Cargo.toml ] || exit 0
command -v cargo >/dev/null 2>&1 || exit 0
command -v git >/dev/null 2>&1 || exit 0
git rev-parse --is-inside-work-tree >/dev/null 2>&1 || exit 0

changed="$(git status --porcelain --untracked-files=all 2>/dev/null | awk '{print $NF}' \
  | grep -E '\.rs$|(^|/)Cargo\.toml$|^Cargo\.lock$|^architecture\.toml$|^tools/versions\.toml$|\.qnt$|^rust-toolchain(\.toml)?$|^\.cargo/config\.toml$' || true)"
[ -z "$changed" ] && exit 0

# `cargo xtask quick` once the alias and the subcommand exist (FND-02); the
# plain fallback before that (CLAUDE.md, "The gate").
if grep -qsE '^[[:space:]]*xtask[[:space:]]*=' .cargo/config.toml \
  && [ -d tools/xtask/src ] && grep -rqs '"quick"' tools/xtask/src; then
  gate="cargo xtask quick"
  run() { cargo xtask quick; }
else
  gate="cargo fmt --all --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked"
  run() {
    cargo fmt --all --check \
      && cargo clippy --workspace --all-targets --locked -- -D warnings \
      && cargo test --workspace --locked
  }
fi

if ! output="$(run 2>&1)"; then
  echo "\`$gate\` is red with uncommitted changes; fix it before stopping:" >&2
  printf '%s\n' "$output" | tail -n 60 >&2
  exit 2
fi
exit 0
