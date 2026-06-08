# Architecture

Tach's toolchain is one Rust crate (lib + `tach` binary) with sharply separated modules.
The throughline of the design: **a span is both "where the error is" and "where to edit."**
Keeping a single byte-offset coordinate space across diagnostics and patches is what makes
machine repair clean.

## The pipeline

```
source ──▶ lexer ──▶ parser ──▶ AST ──┬──▶ checker ──▶ Diagnostics (+ preferred_patch)
                                       └──▶ interpreter ──▶ values / test results
                                                  ▲
                          Workspace ──▶ Patch ──▶ verify ──▶ commit
                                                  │
                                            agent loop (fix / race)
                                                  │
              goal (budget · authority · require) ──▶ runtime ──▶ events + checkpoints
                                                  │                       │
                                            durable store  ◀── resume / replay
```

The bottom two rows are the **goal runtime**: the same repair loop, made durable. A `goal`
declares a budget it can't exceed and an authority it can't escape; the runtime drives the
loop under those constraints, emitting an append-only event per step and a checkpoint after
each one, so a crashed run resumes from exactly where it stopped and a finished run replays
byte-for-byte.

## Modules

| Module | Responsibility |
| --- | --- |
| `span` | byte-range spans; insertion points are zero-width spans |
| `source` | offset → line/column, source slicing |
| `lexer` | tokens with spans; newlines significant except inside `()` / `[]` |
| `ast` | the tree, with **patch-precise** span fields (`brace_offset`, effects-clause spans, return-type span, field/variant name spans); records, `Result`, sum types, `match`, and the goal `plan` block (`PlanStmt`: `let`/`call`/`approve`/`if`/`for`/`while`) |
| `parser` | recursive descent + Pratt expressions; a `no_record_lit` flag disambiguates `if cond {` / `match x {` / `for x in expr {` from `Name {`; the plan block reuses the expression grammar with `call`/`approve`/`for`/`while`/`in` as contextual keywords |
| `diagnostics` | the `Diagnostic` type: human message **and** machine fields (`kind`, `repair_strategies`, `preferred_patch`) |
| `types` | the type lattice, lenient structural compatibility (`Unknown` ~ anything), and the registry of record fields + enum variants |
| `builtins` | builtin module members, their effects, and effect metadata |
| `check` | effect inference vs. declared effects, type checking, import checking, field-access and `match` exhaustiveness checks, and the unused-import / unused-variable lints — every mechanical one emits a `preferred_patch` |
| `value` / `interp` | deterministic tree-walking interpreter; `?` / `ensure` / `return` modeled as non-local control flow; sum-type variants and `match` |
| `runner` | the test runner + impact-scoped runs |
| `patch` | `Workspace`, `Edit`, `Patch`, the verify pipeline, call-graph impact analysis, glob scoping |
| `agent` | the `fix` loop, the optional `Coder` seam (default off), speculative `race`, the suite benchmark, the agent-era `Metrics`, and the shared repair leaves (`collect_problems`, `pick_candidate`, `build_patch`) the goal runtime reuses |
| `goal` | the resolved `GoalSpec` (budget, authority, success conditions), decoupled from spans so it serializes into the store |
| `event` | the `tach.event.v1` envelope and the append-only JSONL `EventLog` |
| `store` | the durable goal store: `goal.json`, `state.json`, `events.jsonl`, `checkpoints/`, `approvals/`, `receipts/`, the deterministic source `fingerprint`, canonical idempotency/approval ids, **atomic writes** (temp-file + rename), and unique run-id allocation |
| `runtime` | the durable executor — `step_once`/`drive` (the repair loop), the action layer's `drive_actions` (a fixed plan with approval gates and receipts), and the plan language's `drive_plan` (durable re-execution of a `plan` block); `resume_run`/`replay_run` dispatch on `GoalRecord.kind` |
| `action` | the linear action layer: the `ActionPlan` model, the offline deterministic **fake tools** (`invoke_fake_tool`), and the built-in goal catalog (`ResolveDuplicateCharge`, `ShipHotfixPR`) |
| `plan` | the **plan language**: the pure expression evaluator (AST `Expr` → JSON value) and the built-in plan-goal catalog (`ReconcileChargebacks`, `RetryFlakyDeploy`); the durable interpreter lives in `runtime` |
| `fmt` | the one canonical formatter — a precedence-aware, idempotent AST pretty-printer (goals included) |
| `schema` | versioned JSON Schemas for every machine output, embedded and served by `tach schema` |
| `trace` | persist/load `fix`/`race` runs to `.tach/trace.json` (the per-goal history lives in the store) |
| `render` / `term` | pretty, colored human output (JSON is the machine path) |
| `project` / `cli` | file discovery, scaffolding, suite loading, the command dispatcher |

## The verify pipeline (`patch::verify_patch`)

A patch is checked against a base `Workspace` without mutating it:

1. **Scope** — every edit's file must match the patch's `touches` globs.
2. **Apply** to a clone (edits per file applied in descending offset order so they don't
   invalidate each other).
3. **Compile** — the patched workspace must still parse and check.
4. **Effect delta** — the set of effects the program *performs* (inferred from bodies) must
   not gain a new member, unless explicitly allowed. Declaring an effect doesn't count as
   introducing one; adding a `net.post(...)` call does.
5. **API changes** — changed public signatures are reported (blocking only if requested).
6. **Impacted tests** — the call graph determines which tests can be affected; only those
   run, and a test that passed before but fails after is a rejection.

The verdict carries the post-patch workspace, so an accepted patch is committed by simply
swapping it in.

## The agent loop (`agent::fix`)

```
loop:
  diagnostics = parse_errors ++ check(workspace)
  if no errors and tests green: status = green; stop
  pick the earliest diagnostic that carries a preferred_patch
  if none:
    if a Coder is wired in: patch = coder.propose(...)   # the seam, default off
    else: status = stuck; stop
  patch = build a typed patch from it (scoped to its file)
  verdict = verify_patch(workspace, patch)
  if accepted: workspace = verdict.workspace   # advance one lap
  else: status = stuck; stop
```

Re-checking every lap means spans are always fresh, so applying one patch at a time never
trips over offsets shifted by an earlier edit. The loop is deterministic, so `race` can run
strategies on threads and `replay` can reproduce a run from its recorded base files.

### The coder seam

When structured repair is exhausted — a syntax error, or a logic bug with no
patch-carrying diagnostic — an optional `Coder` may propose a typed `Patch`. It gets **no**
special privileges: its proposal flows through the exact same `verify_patch` pipeline and is
committed only if it passes scope, effect, API, and regression checks. The default loop
uses no coder at all, so it stays model-free and fully reproducible; `FixtureCoder` is a
deterministic implementation that replays canned patches for offline tests, and a
model-backed coder is a future implementation of the same trait behind a flag.

### Benchmarking over a corpus

`agent::run_suite` runs the loop over every project in a suite (e.g. `corpus/`, one broken
project per diagnostic family) and aggregates the metrics — time-to-green, patches-to-green,
tests-run, regressions — so the thesis is measured over more than the single demo.

## The goal runtime (`runtime`)

A `goal` is the repair loop made durable. The same leaves run — `collect_problems`,
`pick_candidate`, `build_patch`, `verify_patch` — but each step is wrapped in a policy gate,
an event, and a checkpoint.

```
drive(goal, workspace):
  loop:
    if step >= budget.steps: status = budget_exhausted; stop
    outcome = step_once(workspace, goal)          # pure decision, shared with replay
    match outcome:
      Done:      status = completed; emit run.completed; stop
      NoAction:  status = failed;    emit run.failed;    stop
      Patch:
        emit diagnostic.emitted, patch.proposed
        verify under VerifyOpts { allowed_effects, allowed_writes }   # the goal's authority
        if accepted: workspace = new; emit patch.applied
        else:        emit patch.rejected; if consecutive > budget.retries: fail
        step += 1
        write checkpoint(step); emit checkpoint.written; save state
        if crash_after == step: return (durable, not finalized)
```

Three properties carry the design:

- **Authority is enforced, not advertised.** The `allow` block becomes `VerifyOpts`:
  `allowed_effects` rejects a patch that would perform an ungranted effect; `allowed_writes`
  rejects an edit outside the granted `fs.write` globs. Both are caught by the *same*
  pipeline that runs the tests, before anything touches disk.
- **Resume never duplicates work.** A checkpoint stores the post-step workspace. Resuming
  loads the latest checkpoint and continues, so the patches already applied are simply
  *not* re-derived — `step_once` sees a workspace where those diagnostics are gone. The
  event log is appended (a `run.resumed` boundary), never rewritten.
- **Determinism makes it addressable; allocation keeps it safe.** `store::fingerprint` is
  an FNV-1a hash of the goal name and its base source — no clock, no randomness — so the
  first run of a goal is addressable as a clean `run_<hash>`. But a fingerprint is *not* an
  identity: `store::allocate_run` turns it into a unique run id by atomically claiming a
  fresh directory (`fs::create_dir`, which fails if it already exists), handing later runs
  `run_<hash>-2`, `-3`, …. A fresh run therefore can never collide with — or overwrite — an
  existing run, and `EventLog::create` uses `create_new` as a last line of defense, refusing
  to clobber any existing history. `replay_run` re-derives a run with the pure `simulate`
  (the same `step_once`, no I/O) and proves it reproduces the recorded final state.

The working tree is never half-edited: the runtime operates on an in-memory `Workspace` and
only writes verified files back when a run reaches `completed`. A crash leaves the source
exactly as it was; the durable state lives entirely under `.tach/goals/<run_id>/`.

### The store layout

```
.tach/goals/<run_id>/
  goal.json               the resolved GoalSpec + the base source snapshot
  state.json              the mutable RunState head (status, step, metrics)
  events.jsonl            append-only history (one tach.event.v1 per line)
  checkpoints/<step>.json a workspace snapshot taken after each step (repair runs)
  approvals/<id>.json     a human approval gate on an effect (action + plan runs)
  receipts/<id>.json      durable proof an effect ran exactly once (action + plan runs)
```

For action and plan runs the durable truth is the `approvals/` and `receipts/` dirs (plus the
event log); `state.json` is a cosmetic head. All durable writes are **atomic** — staged to a
sibling `.tmp` file and `rename`d into place — so a crash mid-write can never leave a half-written
receipt that `list_receipts` would silently skip and a resume would mistake for "not yet done"
(which would re-run the effect). The rename is the commit; a leftover `.tmp` is harmless.

## The action layer (`action` + `runtime::drive_actions`)

A *business* goal is the same durable spine — run ids, the append-only log, `state.json` —
but its unit of work is not "verify a typed patch", it is "invoke a tool and write a receipt".
It runs a fixed `ActionPlan` (Rust data, from a built-in catalog; the goal's authority/budget
live in a real `goal` declaration), where steps may pause for human approval and effectful
steps must produce a receipt. `tach goal run ResolveDuplicateCharge` is the killer demo: look
up a duplicate charge, refund it **behind an approval gate**, notify the customer, close the
ticket — with *exactly one* refund, even across a crash.

```
drive_actions(goal, plan):
  loop:
    if cursor >= budget.steps: budget_exhausted; stop      # budget bills the cursor
    if cursor >= plan.len:     completed; stop
    action = plan[cursor]
    if action.tool not in allow.tools: failed; stop        # authority, every step
    if action.requires_approval:
      approval = read approvals/<id>                        # the file IS the truth
      None     -> write pending; emit action.proposed, approval.requested; pause
      pending  -> pause
      denied   -> emit action.skipped; status = denied; stop
      granted  -> fall through and execute
    if action.effectful:
      if receipt exists for idempotency_key:
        emit receipt.reused, action.skipped                 # never call the tool twice
      else:
        emit tool.called; out = invoke_fake_tool(...)
        save receipt(out)                                   # <- the commit point
        emit tool.completed, receipt.created
    cursor += 1; save state                                 # <- the SOLE commit past a step
```

The one invariant that makes crash/resume safe: **advancing the cursor and saving state is the
only commit that moves past an action or clears an approval gate.** Until that save, the action
is "not done" durably. So every crash window resolves correctly:

- **Crash after the receipt, before the cursor advance** → resume re-enters the same action,
  the approval is still `granted`, the receipt scan hits, and it emits `receipt.reused` instead
  of calling the tool again. No duplicate effect.
- **Crash after the cursor advance** → resume enters at the next action; the finished one is
  never touched.
- **Crash during the pause** → resume sees the approval still `pending` and waits. The human
  decision is recorded once, by the `approve`/`deny` command, as a real event in the log.

Idempotency keys and approval/receipt ids are FNV-1a digests over a *canonical* (sorted-key)
byte serialization — no clock, no randomness — the same discipline as `store::fingerprint`.
`replay_action_run` re-derives the run purely: it reads the recorded approvals as the run's
human *inputs*, re-invokes the deterministic fake tools, and proves the terminal status and the
receipt set reproduce. (It deliberately does **not** compare event sequences: a crashed-then-
resumed run has a longer log than a straight one, yet produces identical effects.)

No ambient authority, and the working tree is never touched: an action goal proves its work
entirely through receipts under `.tach/goals/<run_id>/`.

## The plan language (`plan` + `runtime::drive_plan`)

The linear `ActionPlan` is a straight list of steps. The **plan language** generalizes it to a
real workflow: a `plan { ... }` block in goal source with `let` bindings, tool `call`s, `approve`
gates, `if`/`else`, and **`for`/`while` loops**. Expressions reuse the ordinary Tach grammar
(field access, arithmetic, `&&`/`||`/`!`, comparisons); the interpreter evaluates them in
JSON-value space, since that is what tools consume and receipts store. Two goals ship built-in:
`ReconcileChargebacks` (a `for` loop over a tool's output with a per-duplicate refund gate) and
`RetryFlakyDeploy` (a `while` retry loop that converges).

The execution model is **durable re-execution**. `run` and `resume` are the *same* operation:
walk the plan from the top. The trick that makes loops and long horizons safe is **memoization
by receipt** —

```
exec_call(c):
  input = eval(c.input)                                  # JSON object
  call_index += 1                                        # walk-order ordinal
  if call_index > budget.steps: budget_exhausted
  if c.tool not in allow.tools: failed                   # authority, before any invoke
  key = idempotency_key(run_id, "c{call_index}", c.tool, input)
  if receipt exists for key: return recorded output      # MEMOIZED — no invoke, no events
  out = invoke_fake_tool(c.tool, input)
  save receipt(out)                                      # <- atomic commit point
  emit tool.called, tool.completed, receipt.created       # only after the receipt is durable
  crash-check; return out

exec_approve(summary, body):
  gate = approval_id(run_id, "gate{gate_index}"); gate_index += 1
  read approvals/<gate>:
    None | pending -> write/keep pending; emit action.proposed, approval.requested; PAUSE
    denied         -> emit action.skipped; status = denied; stop
    granted        -> execute body                        # the gated sub-plan runs now
```

Because `call_index`/`gate_index` are assigned in **walk order** (not at parse time), a loop's
repeated `call` gets a distinct idempotency key per iteration, and — crucially — the assignment
is **stable across resume**: every call before the frontier is either a memoized receipt or a
pure deterministic fake tool, so the branches taken and loop counts are identical on every walk,
hence the same ordinals and the same keys. That is what lets re-execution stand in for a saved
program counter. `state.json` is never read back for control flow; it is recomputed from the
receipts and approvals dirs each time.

Every crash window is safe by the same argument as the action layer, but without a cursor: the
receipt write is the atomic commit, and a crash before it means the call simply has no receipt,
so the resume re-invokes the *deterministic* tool and gets the same output. A crash *after* it
means the resume finds the receipt and returns it without invoking — so a refund inside a loop,
crashed right after it commits, is replayed for free and never issued twice. `replay_plan_run`
is a separate pure re-walk (`ReplayCtx`) that reads the recorded approvals as inputs, re-invokes
the fake tools, and proves the terminal status and receipt set reproduce — sharing the same
expression semantics, key formulas, and tools as the durable walk so the two cannot disagree.

Budget bills tool calls reached per walk (a `for`/`while` that calls tools is bounded by it); a
generous `PLAN_LOOP_LIMIT` separately bounds a pathological call-free loop. Approval decisions are
final once made (the `approve`/`deny` command refuses to change a decided gate), and the plan is
re-derived from the catalog by goal name on every resume — it is part of the binary's identity,
the same philosophy as the repair leaves a replay re-runs.

## Why an interpreter (for now)

v0 prioritizes the *loop*, not raw runtime speed. A deterministic tree-walker is the fastest
path to a language that genuinely cooperates with agents, and determinism is a feature here
(replayable runs, trustworthy metrics). Native/LLVM codegen is a later concern; the
front-end, checker, and patch pipeline are the parts that carry the thesis and they're all
backend-agnostic.
