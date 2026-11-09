#!/usr/bin/env bash
# PostToolUse(Edit|Write|MultiEdit): rustfmt the edited Rust file so the tree
# stays formatted between gate runs. Deliberately fast and silent: no clippy,
# no build (the Stop hook runs the gate). A non-Rust file, a missing file, a
# missing rustfmt or a file that does not parse yet is left alone; the gate
# reports it. Always exits 0.
set -uo pipefail

payload="$(cat)"
if command -v jq >/dev/null 2>&1; then
  file_path="$(printf '%s' "$payload" | jq -r '.tool_input.file_path // empty' 2>/dev/null)"
elif command -v python3 >/dev/null 2>&1; then
  file_path="$(printf '%s' "$payload" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("tool_input", {}).get("file_path", ""))' 2>/dev/null)"
else
  exit 0
fi

case "$file_path" in
  *.rs) ;;
  *) exit 0 ;;
esac
[ -f "$file_path" ] || exit 0
command -v rustfmt >/dev/null 2>&1 || exit 0
command -v timeout >/dev/null 2>&1 || exit 0

# Run from the file's directory so rustup picks the toolchain pinned there and
# rustfmt finds the nearest rustfmt.toml. The timeout keeps a missing pinned
# toolchain (rustup would try to install it) from stalling the edit.
cd "$(dirname "$file_path")" || exit 0
timeout 20 rustfmt --edition 2024 "$file_path" >/dev/null 2>&1 || true
exit 0
