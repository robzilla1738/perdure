# Perdure guard — operating contract for AI coding agents

This repository is operated through **Perdure**: a runtime that scopes, verifies, and
records your work. You bring the reasoning and the edits; Perdure is the guardrail and
the durable ledger. Follow this contract.

## Open a session
    perdure guard begin FixFailingTests
    perdure guard context --json                      # the contract for this run
    perdure guard context --for-agent generic --json  # the full agent packet (changes, receipts, next move)
    perdure guard next --json                          # just the single next required action

`context` reports:
  - allowed_files     — globs you may edit; edits elsewhere are rejected at the gate
  - allowed_commands  — the only commands Perdure will run for you
  - current_failure   — what is failing right now (or null)
  - next_required_action

## While you work
  - Edit only files matching `allowed_files`. An edit outside that scope is an
    out-of-scope write: Perdure records it and it will block the commit. Perdure detects
    and rejects such edits at the gate — it does not silently prevent the write, so
    staying in scope is on you.
  - Run project commands through `perdure guard verify`, not directly, so each run is
    captured as a receipt.

## Before you claim done
  - Run `perdure guard verify` and read the JSON.
  - **Do not tell the user the task is done unless Perdure reports `verified: true`.**
    `verified` is true only when every required command passed AND no out-of-scope
    file changed. The `done_condition` field of `context --json` states this check.
  - Finish with `perdure guard finalize` (alias: `perdure guard commit`). This finalizes the
    run into **Perdure's own ledger only — it does not create a git commit or touch git in
    any way.** Staging, committing, or pushing to git is yours to do (or not), separately.
  - If `finalize` refuses, run `perdure guard diff --json`, fix the violations, and verify
    again.

Goal: FixFailingTests
Verified by: `cargo test`
