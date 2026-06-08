#!/usr/bin/env bash
# End-to-end smoke test: scaffold a broken project, prove `tach check` is red,
# run the repair loop, and prove the project ends green with passing tests.
# Exit code 0 == everything works. Safe for headless / CI / cloud-agent use.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cargo build --release --manifest-path "$ROOT/Cargo.toml"
BIN="$ROOT/target/release/tach"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

echo "## tach new demo"
"$BIN" new demo >/dev/null
cd demo

echo "## tach check  (expect failure: 3 planted bugs)"
if "$BIN" check >/dev/null 2>&1; then
  echo "FAIL: expected check to report errors on the fresh demo"
  exit 1
fi
echo "   ok — check is red as expected"

echo "## tach fix"
"$BIN" fix

echo "## tach check  (expect success)"
"$BIN" check

echo "## tach test   (expect all green)"
"$BIN" test

echo "## tach replay (expect exact reproduction)"
"$BIN" replay >/dev/null

echo
echo "ALL GOOD — red → green, reproduced."
