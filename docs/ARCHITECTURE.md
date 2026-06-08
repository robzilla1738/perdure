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
| `ast` | the tree, with **patch-precise** span fields (`brace_offset`, effects-clause spans, return-type span, field/variant name spans); records, `Result`, sum types, and `match` |
| `parser` | recursive descent + Pratt expressions; a `no_record_lit` flag disambiguates `if cond {` / `match x {` from `Name {` |
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
| `store` | the durable goal store: `goal.json`, `state.json`, `events.jsonl`, `checkpoints/`, the deterministic source `fingerprint`, and unique run-id allocation |
| `runtime` | the durable executor — `step_once` (the pure decision), `drive` (persisting loop with budgets, checkpoints, crash), `resume_run`, `replay_run`, `cancel_run` |
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
  checkpoints/<step>.json a workspace snapshot taken after each step
```

## Why an interpreter (for now)

v0 prioritizes the *loop*, not raw runtime speed. A deterministic tree-walker is the fastest
path to a language that genuinely cooperates with agents, and determinism is a feature here
(replayable runs, trustworthy metrics). Native/LLVM codegen is a later concern; the
front-end, checker, and patch pipeline are the parts that carry the thesis and they're all
backend-agnostic.
