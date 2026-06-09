#!/usr/bin/env bash
# End-to-end test of AGENT INTERFACE v1 — the structured front door an external
# coding agent (Claude Code, Codex, Cursor-style, local) uses to operate Perdure from
# the outside. `scripts/guard_e2e.sh` covers the guard session itself; this proves
# the agent-facing surface added on top, driven entirely through the `perdure` binary.
#
# It proves, against the binary:
#   - `perdure init --existing` GENERATES a vendor-neutral `AGENTS.md` (with the Perdure
#     sentinel and the `context --for-agent generic` instruction) when none exists.
#   - `perdure guard context --for-agent generic --json` emits EXACTLY the field set its
#     published schema (`perdure schema agent-context`) promises — output and contract
#     cannot drift apart.
#   - `perdure guard next --json` returns a single next action whose value tracks state:
#     edit_then_verify → fix_scope_violation → finalize → done.
#   - an out-of-scope edit makes `verify --json` return machine-actionable repair
#     hints: a `scope_violation` rejection naming the path, the allowed scope, named
#     `repair_strategies`, and a `preferred_next_action` of revert_file.
#   - `perdure guard finalize` and the `perdure guard commit` alias are equivalent,
#     ledger-only operations: both reach `completed` and neither creates a git repo.
#   - `perdure serve-mcp` speaks JSON-RPC 2.0 over stdio (initialize → tools/list →
#     tools/call), exposes only safe guard/goal tools, and never a raw-shell tool.
#
# Hermetic + toolchain-free: the allowlisted command is a trivial committed `check.sh`
# (greps for a fix marker), so CI needs no project toolchain.
set -euo pipefail
export NO_COLOR=1

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cargo build --release --manifest-path "$ROOT/Cargo.toml"
BIN="$ROOT/target/release/perdure"

WORK="$(mktemp -d)"
SCRATCH="$(mktemp -d)"   # for tool output we must NOT write inside the guarded repo
trap 'rm -rf "$WORK" "$SCRATCH"' EXIT
cd "$WORK"

jget() { python3 -c 'import sys,json; print(json.load(sys.stdin)'"$1"')'; }

# ---------------------------------------------------------------------------
echo "## build an existing repo fixture (no AGENTS.md — init must generate one)"
cat > Cargo.toml <<'EOF'
[package]
name = "fixture"
version = "0.1.0"
edition = "2021"
EOF
mkdir -p src
echo '// broken: no fix marker yet' > src/lib.rs
echo '# Fixture' > README.md
echo 'grep -q FIXMARKER src/lib.rs && exit 0 || exit 1' > check.sh

# ---------------------------------------------------------------------------
echo "## perdure init --existing  (must GENERATE AGENTS.md, vendor-neutral, with the sentinel)"
"$BIN" init --existing >/dev/null
[ -f AGENTS.md ] || { echo "FAIL: init did not generate AGENTS.md"; exit 1; }
head -1 AGENTS.md | grep -q 'perdure:agents-contract' || { echo "FAIL: AGENTS.md missing the Perdure sentinel"; exit 1; }
grep -q 'perdure guard context --for-agent generic --json' AGENTS.md || { echo "FAIL: AGENTS.md does not instruct the agent to read the generic context"; exit 1; }
grep -qi 'ledger-only' AGENTS.md || { echo "FAIL: AGENTS.md does not say finalize is ledger-only"; exit 1; }
echo "   ok — AGENTS.md generated with the operating contract"

echo "## point the goal at the trivial check.sh command (keeps CI toolchain-free)"
cat > Perdurefile <<'EOF'
goal FixFailingTests -> Success {
  budget {
    steps: 40
  }
  allow {
    fs.write ["src/**", "tests/**"]
    shell.run ["sh check.sh"]
  }
  require {
    command("sh check.sh").passes
    no_out_of_scope_writes
  }
}
EOF

# ---------------------------------------------------------------------------
echo "## perdure guard begin"
RUN_ID="$("$BIN" guard begin FixFailingTests --json | jget '["run_id"]')"
[ -n "$RUN_ID" ] || { echo "FAIL: no run id"; exit 1; }

echo "## guard context --for-agent generic --json  (keys MUST match the published schema)"
"$BIN" guard context --for-agent generic --json > "$SCRATCH/ctx.json"
"$BIN" schema agent-context > "$SCRATCH/ctx.schema.json"
python3 - "$SCRATCH/ctx.json" "$SCRATCH/ctx.schema.json" <<'PY'
import json, sys
ctx = json.load(open(sys.argv[1]))
sch = json.load(open(sys.argv[2]))
props = set(sch["properties"].keys())
emitted = set(ctx.keys())
assert emitted == props, f"agent-context keys {sorted(emitted)} != schema properties {sorted(props)}"
assert set(sch["required"]) <= emitted, f"missing required: {set(sch['required']) - emitted}"
assert ctx["agent"] == "generic" and ctx["schema"] == "perdure.agent-context.v1", ctx["schema"]
assert ctx["required_commands"] == ["sh check.sh"], ctx["required_commands"]
assert any("src" in g for g in ctx["allowed_files"]), ctx["allowed_files"]
print("   ok — agent-context output matches its schema exactly, agent=generic")
PY

echo "## guard next --json  (fresh session → edit_then_verify)"
N="$("$BIN" guard next --json)"
[ "$(echo "$N" | jget '["status"]')" = editing ] || { echo "FAIL: fresh status not editing"; exit 1; }
[ "$(echo "$N" | jget '["next_action"]')" = edit_then_verify ] || { echo "FAIL: fresh next_action not edit_then_verify"; exit 1; }
# `next` keys must also match its published schema.
"$BIN" guard next --json > "$SCRATCH/next.json"
"$BIN" schema guard-next > "$SCRATCH/next.schema.json"
python3 - "$SCRATCH/next.json" "$SCRATCH/next.schema.json" <<'PY'
import json, sys
n = json.load(open(sys.argv[1])); sch = json.load(open(sys.argv[2]))
assert set(n.keys()) == set(sch["properties"].keys()), (sorted(n), sorted(sch["properties"]))
print("   ok — guard next output matches its schema exactly")
PY

# ---------------------------------------------------------------------------
echo "## OUT-OF-SCOPE edit → next says fix_scope_violation, verify --json carries repair hints"
mkdir -p .github/workflows
echo 'on: push' > .github/workflows/ci.yml
echo '# out of scope' >> README.md
N="$("$BIN" guard next --json)"
[ "$(echo "$N" | jget '["status"]')" = blocked ] || { echo "FAIL: status not blocked on out-of-scope"; exit 1; }
[ "$(echo "$N" | jget '["next_action"]')" = fix_scope_violation ] || { echo "FAIL: next_action not fix_scope_violation"; exit 1; }
echo "$N" | jget '["scope_violations"]' | grep -q '.github/workflows/ci.yml' || { echo "FAIL: next missing .github violation"; exit 1; }
echo "$N" | jget '["scope_violations"]' | grep -q 'README.md' || { echo "FAIL: next missing README violation"; exit 1; }

"$BIN" guard verify --json > "$SCRATCH/verify.json" || true   # non-zero exit expected; capture JSON
python3 - "$SCRATCH/verify.json" <<'PY'
import json, sys
v = json.load(open(sys.argv[1]))
assert v["verified"] is False, v["verified"]
assert v["next_action"] == "fix_scope_violation", v["next_action"]
r = v["rejection"]
assert r is not None and r["kind"] == "scope_violation", r
paths = {x["path"] for x in r["violations"]}
assert ".github/workflows/ci.yml" in paths and "README.md" in paths, paths
assert all(x["allowed"] for x in r["violations"]), "each violation must name the allowed scope"
assert "revert_out_of_scope_file" in r["repair_strategies"], r["repair_strategies"]
assert r["preferred_next_action"]["kind"] == "revert_file", r["preferred_next_action"]
assert r["preferred_next_action"]["path"] in paths, r["preferred_next_action"]
print("   ok — scope rejection is machine-actionable (paths, allowed scope, repair strategies, preferred move)")
PY

# verify keys must match the (richer) guard-verify schema.
"$BIN" schema guard-verify > "$SCRATCH/verify.schema.json"
python3 - "$SCRATCH/verify.json" "$SCRATCH/verify.schema.json" <<'PY'
import json, sys
v = json.load(open(sys.argv[1])); sch = json.load(open(sys.argv[2]))
assert set(v.keys()) == set(sch["properties"].keys()), (sorted(v), sorted(sch["properties"]))
print("   ok — guard verify output matches its schema exactly")
PY

# ---------------------------------------------------------------------------
echo "## revert + fix in scope → next says finalize"
rm -rf .github
echo '# Fixture' > README.md            # revert the README edit
echo '// FIXMARKER applied' > src/lib.rs # the in-scope fix
"$BIN" guard verify >/dev/null
N="$("$BIN" guard next --json)"
[ "$(echo "$N" | jget '["status"]')" = verified ] || { echo "FAIL: status not verified after green verify"; exit 1; }
[ "$(echo "$N" | jget '["next_action"]')" = finalize ] || { echo "FAIL: next_action not finalize"; exit 1; }
[ "$(echo "$N" | jget '["recommended_command"]')" = "perdure guard finalize" ] || { echo "FAIL: recommended_command not finalize"; exit 1; }
# Agent context now shows the passing receipt with its captured artifact.
"$BIN" guard context --for-agent generic --json > "$SCRATCH/ctx2.json"
python3 - "$SCRATCH/ctx2.json" <<'PY'
import json, sys
c = json.load(open(sys.argv[1]))
assert c["verified"] is True and c["next_action"] == "finalize", (c["verified"], c["next_action"])
assert len(c["latest_receipts"]) == 1 and c["latest_receipts"][0]["passed"] is True, c["latest_receipts"]
assert c["latest_receipts"][0]["stdout_artifact"], "receipt must point at a captured stdout artifact"
assert c["scope_violations"] == [] and c["forbidden_files"] == [], (c["scope_violations"], c["forbidden_files"])
print("   ok — agent context reflects verified state + the passing receipt")
PY

echo "## finalize via the preferred spelling (ledger only; git untouched)"
"$BIN" guard finalize >/dev/null
[ "$(python3 -c 'import json;print(json.load(open(".perdure/goals/'"$RUN_ID"'/state.json"))["status"])')" = completed ] || { echo "FAIL: run not completed"; exit 1; }
[ ! -d .git ] || { echo "FAIL: finalize created a .git directory"; exit 1; }
N="$("$BIN" guard next --json "$RUN_ID")"
[ "$(echo "$N" | jget '["next_action"]')" = done ] || { echo "FAIL: finalized run next_action not done"; exit 1; }

# ---------------------------------------------------------------------------
echo '## prove `commit` is an equivalent ledger-only alias: a second run finalized via `commit`'
RUN2="$("$BIN" guard begin FixFailingTests --json | jget '["run_id"]')"  # FIXMARKER already present → verifies green
"$BIN" guard verify >/dev/null
"$BIN" guard commit >/dev/null
S2="$(python3 -c 'import json;print(json.load(open(".perdure/goals/'"$RUN2"'/state.json"))["status"])')"
[ "$S2" = completed ] || { echo "FAIL: commit alias did not reach completed"; exit 1; }
grep -q '"guard.committed"' ".perdure/goals/$RUN2/events.jsonl" || { echo "FAIL: commit alias recorded no guard.committed event"; exit 1; }
[ ! -d .git ] || { echo "FAIL: commit alias created a .git directory"; exit 1; }
echo "   ok — finalize and commit both reach completed, ledger-only, git untouched"

# ---------------------------------------------------------------------------
echo "## perdure serve-mcp (server-only MCP over stdio): initialize → tools/list → goal_receipts"
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"perdure_goal_receipts","arguments":{"run_id":"'"$RUN_ID"'"}}}' \
  | "$BIN" serve-mcp > "$SCRATCH/mcp.out"
python3 - "$SCRATCH/mcp.out" <<'PY'
import json, sys
msgs = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
assert len(msgs) == 3, f"expected 3 responses (the notification gets none), got {len(msgs)}"
init, tools, call = msgs
assert init["result"]["serverInfo"]["name"] == "perdure", init
assert init["result"]["protocolVersion"] == "2025-06-18", "server should echo the client protocol version"
names = [t["name"] for t in tools["result"]["tools"]]
for must in ["perdure_guard_begin", "perdure_guard_verify", "perdure_guard_finalize", "perdure_goal_receipts"]:
    assert must in names, f"missing tool {must}"
assert not any(("shell" in n or "exec" in n or "write" in n) for n in names), "no raw-execution tool may be exposed"
body = json.loads(call["result"]["content"][0]["text"])
assert isinstance(body, list) and len(body) >= 1, "the receipts tool should return the run's receipts"
print("   ok — MCP server spoke JSON-RPC over stdio; tools are safe; receipts returned")
PY

echo
echo "ALL GOOD — AGENTS.md generated, agent-context/next/verify outputs match their"
echo "           published schemas, next_action tracked editing→fix_scope→finalize→done,"
echo "           scope rejection carried repair hints, finalize ≡ commit (ledger-only),"
echo "           and serve-mcp exposed only safe tools over JSON-RPC stdio."
