# Operating Perdure as an external agent

The contract for coding agents (Claude Code, Codex, Cursor, anything MCP-capable)
working on a repo that Perdure governs. Perdure does not replace you: you bring the
reasoning and edit files with your own tools; Perdure makes the work scoped, verified,
durable, replayable, and auditable.

## The loop

```console
$ perdure guard begin FixFailingTests   # open a session: snapshot the tree, freeze the ignore set
$ perdure guard context --json          # the full operating contract (see below)
$ perdure guard next --json             # ONE next required action, machine-shaped
# … edit files with your own tools, inside the allowed scope …
$ perdure guard verify                  # run the goal's required commands, receipt the results
$ perdure guard finalize                # commit the session into Perdure's ledger (never git)
$ perdure guard audit                   # prove the ledger: hash chain + receipts + verified bit
```

`guard status`, `guard diff` (changes classified in/out of scope), and `guard abort`
round it out. Every command takes `--json`.

## The packets

- **`guard context --for-agent generic --json`** (`perdure.agent-context.v1`) — the whole
  contract in one read: `allowed_files` (the `fs.write` globs), `allowed_commands` (the
  exact-match shell allowlist), `forbidden`, the current failure, latest command
  receipts with artifact paths, `verification_condition`, and `done_condition`.
- **`guard next --json`** — a single required action:
  `edit_then_verify | fix_scope_violation | run_verify | finalize | done`, with
  machine-actionable hints (offending paths, the scope that rejected them, repair
  strategies). If you only read one thing per turn, read this.
- **`check --json`** — every compiler diagnostic carries `kind`, `code`,
  `repair_strategies`, and a `preferred_patch` (byte-span replacement). You can apply
  the patch verbatim; `perdure fix` does exactly that.
- **Rejections are repairable.** An out-of-scope `verify` returns the violating paths
  and the allowed globs — move the change or ask the operator to widen the goal; never
  edit `.perdureignore` to hide a file (the ignore set is frozen at `begin`, and the
  audit would surface it anyway).

## Rules that are enforced, not advisory

1. Only commands on the allowlist run, verbatim — there is no shell, no `&&`, no glob.
2. Edits outside `fs.write` fail verify/finalize. The gate sees through ignore-file
   edits and attributes tool-generated files (lockfiles) separately.
3. `finalize` writes Perdure's ledger only. Git is yours; Perdure never commits, pushes,
   or touches git state.
4. Receipts are keyed by (command, tree digest): re-running verify on an unchanged tree
   reuses the receipt instead of re-executing — don't loop on verify hoping for a
   different answer; change the tree.
5. The event log is hash-chained and auditable. Assume everything you do through the
   guard is permanent evidence.

## MCP

`perdure serve-mcp` exposes the same surface as MCP tools over stdio
(`perdure_guard_begin`, `perdure_guard_context`, `perdure_guard_next`,
`perdure_guard_verify`, `perdure_guard_finalize`, `perdure_guard_audit`, plus goal
operations). Server-only by design: no raw shell tool, no arbitrary file-write tool —
you edit with your own capabilities; Perdure stays the gate and the ledger. Run the
server outside the agent's sandbox to upgrade ledger tamper-*evidence* into
tamper-*prevention* (see [THREAT_MODEL.md](THREAT_MODEL.md)).

## Plans, for agents that orchestrate

The same guarantees back the plan language: write a `goal` with a `plan { … }` block,
grant exactly the commands/URLs/tools it needs, and `perdure goal run` executes it
durably — receipts, approvals, budgets, crash-safe resume, replay-from-receipts. An
agent can author a plan, hand it to the operator for `goal check` + run, and pick up
results from `goal receipts --json`. Adopt a repo with `perdure init --existing`; it
writes `Perdurefile`, `PERDURE_AGENT.md`, `AGENTS.md`, and `.perdureignore`, and detects
the test command.
