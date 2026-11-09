#!/usr/bin/env bash
# SessionStart: make a fresh Claude Code on the web container ready for the
# gate with no manual setup. Local sessions already have their toolchain, so
# the hook does nothing outside a remote session. Every step is a no-op while
# its prerequisite does not exist yet (no rust-toolchain.toml, no Cargo.toml,
# no Cargo.lock, no tools/versions.toml, no formal/package-lock.json), so the
# repo works before FND-01, FND-02 and FND-06 land. It never installs heavy
# optional tools (it prints how instead) and never touches Tart or Docker:
# neither exists in the sandbox.
set -uo pipefail

if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-$(dirname "$0")/../..}" || exit 0

# The pinned toolchain. rustup >= 1.28 installs the toolchain named by
# rust-toolchain.toml (with its components) when given no argument.
if [ -f rust-toolchain.toml ] || [ -f rust-toolchain ]; then
  if command -v rustup >/dev/null 2>&1; then
    if ! log="$(rustup toolchain install 2>&1)"; then
      printf 'rustup could not install the pinned toolchain:\n%s\n' "$log" >&2
      exit 1
    fi
    echo "rust: $(rustc --version 2>/dev/null) (pinned by rust-toolchain.toml)"
  else
    echo "rust: rustup missing; install it from https://rustup.rs, then run \`rustup toolchain install\`"
  fi
else
  echo "rust: $(rustc --version 2>/dev/null || echo missing) (no rust-toolchain.toml yet)"
fi

# Crates, so the first build in a worktree does not wait on the network.
if [ -f Cargo.lock ] && [ -f Cargo.toml ]; then
  if ! log="$(cargo fetch --locked 2>&1)"; then
    printf 'cargo fetch --locked failed:\n%s\n' "$(printf '%s\n' "$log" | tail -n 30)" >&2
    exit 1
  fi
  echo "crates: fetched (Cargo.lock)"
elif [ -f Cargo.toml ]; then
  echo "crates: no Cargo.lock yet; the first build will create it"
else
  echo "crates: no Cargo.toml yet; nothing to fetch"
fi

# Optional tools: report, never fail, never install the heavy ones here.
# `cargo xtask doctor` is the authority once tools/versions.toml exists.
if [ -f tools/versions.toml ]; then
  for tool in cargo-nextest cargo-deny cargo-llvm-cov cargo-mutants; do
    if grep -qs "^\[tools\.${tool}\]" tools/versions.toml; then
      if command -v "$tool" >/dev/null 2>&1; then
        echo "$tool: present"
      else
        echo "$tool: missing; \`cargo xtask doctor\` names the pinned version to install"
      fi
    fi
  done
else
  echo "tools: no tools/versions.toml yet (FND-02); nothing pinned to check"
fi
if [ -x formal/node_modules/.bin/quint ]; then
  echo "quint: present"
elif [ -f formal/package-lock.json ]; then
  echo "quint: missing; run \`npm ci --prefix formal\` (dev-time only; Node $(node --version 2>/dev/null || echo missing))"
else
  echo "quint: not needed yet (no formal/package-lock.json; FND-06)"
fi

if [ -f Cargo.toml ]; then
  if grep -qsE '^[[:space:]]*xtask[[:space:]]*=' .cargo/config.toml \
    && [ -d tools/xtask/src ] && grep -rqs '"check"' tools/xtask/src; then
    echo "mayfly ready: run \`cargo xtask quick\` while working, \`cargo xtask check\` before commit (see doc/tasks.md)"
  else
    echo "mayfly ready: the gate is the fallback in CLAUDE.md until xtask exists (FND-02)"
  fi
else
  echo "mayfly: no code yet; see doc/tasks.md for the first task (FND-01)"
fi
exit 0
