#!/usr/bin/env bash
# End-to-end test of REAL TOOLS IN PLANS: a durable workflow whose `call
# shell.run { ... }` statements execute actual processes under the goal's
# authority. Proves the centerpiece guarantees against the real world:
#
#   1. authority  — a command not on `allow { shell.run [...] }` is refused
#                   BEFORE it spawns (authority.denied on the ledger, no receipt,
#                   no side effect on disk);
#   2. exactly-once — crash right after a command's receipt commits, resume, and
#                   the command has still run exactly once (marker-file count);
#   3. approvals  — a real command behind `approve` waits for a human grant;
#   4. replay     — `goal replay` re-walks the plan feeding recorded receipts
#                   back in and NEVER spawns (marker counts cannot grow).
#
# Exit 0 == real effects are governed, durable, and exactly-once.
set -euo pipefail
export NO_COLOR=1

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cargo build --release --manifest-path "$ROOT/Cargo.toml"
BIN="$ROOT/target/release/perdure"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

status_of() {
  "$BIN" goal inspect "$1" --json | python3 -c 'import sys,json; print(json.load(sys.stdin)["state"]["status"])'
}
pending_apr() {
  "$BIN" goal approvals "$1" | grep pending | grep -oE 'apr_[a-f0-9]+' | head -1
}
shell_receipts() {
  "$BIN" goal receipts "$1" | grep -c 'shell.run' || true
}
lines_in() {
  if [ -f "$1" ]; then wc -l < "$1" | tr -d ' '; else echo 0; fi
}

echo "## the scaffolded shell template checks clean"
"$BIN" new tmpl --goal shell >/dev/null
(cd tmpl && "$BIN" goal check SmokeCheck >/dev/null && "$BIN" fmt --check >/dev/null)
echo "   ok — perdure new --goal shell scaffolds a canonical, checkable goal"

echo "## a marker-counting goal: two real commands, the second behind approval"
mkdir realgoal && cd realgoal
cat > goal.pdr <<'PDR'
goal CountedRun -> Success {
  budget {
    steps: 10
  }
  allow {
    shell.run ["sh -c \"echo ran >> first.txt\"", "sh -c \"echo ran >> second.txt\""]
  }
  plan {
    let first = call shell.run {
      cmd: "sh -c \"echo ran >> first.txt\""
    }
    if first.ok {
      approve "run the second command" {
        call shell.run {
          cmd: "sh -c \"echo ran >> second.txt\""
        }
      }
    }
  }
}
PDR

"$BIN" goal check CountedRun >/dev/null

echo "## run: first command executes and receipts, then the gate pauses the run"
"$BIN" goal run CountedRun >/dev/null
RUN_ID="$("$BIN" goal list | grep -oE 'run_[a-f0-9]+' | head -1)"
[ "$(status_of "$RUN_ID")" = "awaiting_approval" ] || { echo "FAIL: expected awaiting_approval"; exit 1; }
[ "$(lines_in first.txt)" = "1" ] || { echo "FAIL: first command should have run once"; exit 1; }
[ "$(lines_in second.txt)" = "0" ] || { echo "FAIL: gated command must not run before approval"; exit 1; }
[ "$(shell_receipts "$RUN_ID")" = "1" ] || { echo "FAIL: expected 1 receipt"; exit 1; }
echo "   ok — real command receipted, gated command waiting"

echo "## approve, resume --crash-after step:1: crash lands right after the second receipt"
APR="$(pending_apr "$RUN_ID")"
"$BIN" goal approve "$RUN_ID" "$APR" >/dev/null
set +e
"$BIN" goal resume "$RUN_ID" --crash-after step:1 >/dev/null
CODE=$?
set -e
if [ "$CODE" -ne 99 ]; then
  echo "FAIL: expected crash exit 99, got $CODE"
  exit 1
fi
[ "$(lines_in second.txt)" = "1" ] || { echo "FAIL: second command should have run before the crash"; exit 1; }
echo "   ok — crashed with the second receipt durable"

echo "## resume after the crash: completes WITHOUT re-running either command"
"$BIN" goal resume "$RUN_ID" >/dev/null
[ "$(status_of "$RUN_ID")" = "completed" ] || { echo "FAIL: expected completed"; exit 1; }
[ "$(lines_in first.txt)" = "1" ] || { echo "FAIL: first command reran on resume"; exit 1; }
[ "$(lines_in second.txt)" = "1" ] || { echo "FAIL: second command reran on resume"; exit 1; }
[ "$(shell_receipts "$RUN_ID")" = "2" ] || { echo "FAIL: expected exactly 2 receipts"; exit 1; }
echo "   ok — exactly-once held across the crash"

echo "## replay: re-walks from receipts, spawns nothing"
"$BIN" goal replay "$RUN_ID" >/dev/null
[ "$(lines_in first.txt)" = "1" ] || { echo "FAIL: replay spawned the first command"; exit 1; }
[ "$(lines_in second.txt)" = "1" ] || { echo "FAIL: replay spawned the second command"; exit 1; }
echo "   ok — replay reproduced the run with zero process spawns"

echo "## authority: an unlisted command is refused before it can touch disk"
cd .. && mkdir evilgoal && cd evilgoal
cat > goal.pdr <<'PDR'
goal Sneaky -> Success {
  budget {
    steps: 5
  }
  allow {
    shell.run ["true"]
  }
  plan {
    call shell.run {
      cmd: "sh -c \"echo pwned >> evil.txt\""
    }
  }
}
PDR
# The static checker already rejects this (E0438); prove the RUNTIME gate too by
# running the unchecked goal directly.
set +e
"$BIN" goal run Sneaky >/dev/null 2>&1
set -e
EVIL_RUN="$("$BIN" goal list | grep -oE 'run_[a-f0-9]+' | head -1)"
[ "$(status_of "$EVIL_RUN")" = "failed" ] || { echo "FAIL: expected failed status"; exit 1; }
[ ! -f evil.txt ] || { echo "FAIL: refused command wrote to disk"; exit 1; }
grep -q '"authority.denied"' ".perdure/goals/$EVIL_RUN/events.jsonl" || { echo "FAIL: no authority.denied event"; exit 1; }
[ "$(shell_receipts "$EVIL_RUN")" = "0" ] || { echo "FAIL: a refusal must not receipt"; exit 1; }
echo "   ok — refusal is on the ledger, nothing fired"

echo
echo "PASS: real shell tools are governed, durable, exactly-once, and replay-safe"
