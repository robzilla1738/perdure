#!/usr/bin/env bash
# End-to-end test of the PLAN LANGUAGE: a durable workflow with a loop, a branch,
# and a per-iteration approval gate. ReconcileChargebacks pulls a customer's
# disputed charges and, for each genuine duplicate, refunds it behind its own
# approval gate. We approve the first refund, CRASH mid-loop right after that
# refund is receipted, resume, approve the second, and finish — then prove each
# duplicate was refunded EXACTLY ONCE across the crash, with a deterministic
# replay. Exit 0 == loops + long-horizon approvals + exactly-once effects work.
# Hermetic: the "tools" are offline fakes — no network, no clock, no services.
set -euo pipefail
export NO_COLOR=1

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cargo build --release --manifest-path "$ROOT/Cargo.toml"
BIN="$ROOT/target/release/tach"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

status_of() {
  "$BIN" goal inspect "$1" --json | python3 -c 'import sys,json; print(json.load(sys.stdin)["state"]["status"])'
}
pending_apr() {
  "$BIN" goal approvals "$1" | grep pending | grep -oE 'apr_[a-f0-9]+' | head -1
}
refund_receipts() {
  "$BIN" goal receipts "$1" | grep -c 'fake.stripe.refund' || true
}

echo "## tach goal run ReconcileChargebacks  (expect a pause at the first duplicate's gate)"
"$BIN" goal run ReconcileChargebacks >/dev/null
RUN_ID="$("$BIN" goal list | awk '/run_/ {print $1; exit}')"
echo "## run id: $RUN_ID"

if [ "$(status_of "$RUN_ID")" != "awaiting_approval" ]; then
  echo "FAIL: expected awaiting_approval, got $(status_of "$RUN_ID")"
  exit 1
fi
echo "   ok — paused at the first refund gate"

APR1="$(pending_apr "$RUN_ID")"
echo "## approve the first refund ($APR1), then resume --crash-after step:1 (crash right after the refund)"
"$BIN" goal approve "$RUN_ID" "$APR1" >/dev/null
set +e
"$BIN" goal resume "$RUN_ID" --crash-after step:1 >/dev/null
CODE=$?
set -e
if [ "$CODE" -ne 99 ]; then
  echo "FAIL: expected crash exit 99, got $CODE"
  exit 1
fi
if [ "$(refund_receipts "$RUN_ID")" -ne 1 ]; then
  echo "FAIL: expected 1 durable refund receipt after the crash, found $(refund_receipts "$RUN_ID")"
  exit 1
fi
echo "   ok — crashed mid-loop with the first refund durable"

echo "## tach goal resume $RUN_ID  (expect a pause at the SECOND duplicate's gate — refund #1 is NOT repeated)"
"$BIN" goal resume "$RUN_ID" >/dev/null
if [ "$(status_of "$RUN_ID")" != "awaiting_approval" ]; then
  echo "FAIL: expected awaiting_approval at the second gate, got $(status_of "$RUN_ID")"
  exit 1
fi
echo "   ok — paused at the second refund gate"

APR2="$(pending_apr "$RUN_ID")"
echo "## approve the second refund ($APR2), resume to completion"
"$BIN" goal approve "$RUN_ID" "$APR2" >/dev/null
"$BIN" goal resume "$RUN_ID" >/dev/null
if [ "$(status_of "$RUN_ID")" != "completed" ]; then
  echo "FAIL: expected completed, got $(status_of "$RUN_ID")"
  exit 1
fi
echo "   ok — completed"

echo "## assert fake.stripe.refund was called EXACTLY ONCE PER DUPLICATE (two calls, no crash double-up)"
CALLS="$("$BIN" goal inspect "$RUN_ID" --json | python3 -c '
import sys, json
evs = json.load(sys.stdin)["events"]
print(sum(1 for e in evs
          if e["kind"] == "tool.called"
          and e["payload"].get("tool") == "fake.stripe.refund"))')"
if [ "$CALLS" -ne 2 ]; then
  echo "FAIL: expected 2 refund tool.called (one per duplicate), found $CALLS"
  exit 1
fi
echo "   ok — two refunds, each invoked exactly once across the mid-loop crash"

echo "## assert exactly two fake.stripe.refund receipts, six receipts total"
if [ "$(refund_receipts "$RUN_ID")" -ne 2 ]; then
  echo "FAIL: expected 2 refund receipts, found $(refund_receipts "$RUN_ID")"
  exit 1
fi
TOTAL="$("$BIN" goal receipts "$RUN_ID" | grep -c 'rcpt_')"
if [ "$TOTAL" -ne 6 ]; then
  echo "FAIL: expected 6 receipts (list + refund×2 + email×2 + comment), found $TOTAL"
  exit 1
fi
echo "   ok — two durable refund receipts (six total)"

echo "## tach goal replay $RUN_ID  (expect exact reproduction)"
"$BIN" goal replay "$RUN_ID"

echo
echo "ALL GOOD — looped over disputes, paused per duplicate, crashed mid-loop, refunded each exactly once, reproduced."
