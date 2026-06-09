<div align="center">

# Tach

**A typed goal runtime for long-horizon agents.**

*The fastest path from a failing goal to a verified result.*

</div>

---

Tach gives an agent's goals a **typed, deterministic, auditable control plane**. A goal
runs with a budget it can't exceed, an authority it can't escape, checkpoints it can
resume from, an event history that records everything it did, and a verification pipeline
that refuses any change that breaks the build, regresses a test, or reaches outside the
scope it was granted. Use Tach when you want an agent to pursue a goal **without losing
state, exceeding authority, repeating side effects, or making untraceable changes.**

The foundation is a language whose **compiler is also an agent harness**. Most languages
were designed for humans to write and a machine to run; Tach is designed for the loop
agents live inside:

```
goal → budget → authority → diagnostic → typed patch → verify → checkpoint → resume → trace
```

The bet that makes it work: if the compiler emits *repairs* instead of just complaints,
the agent driving it can be *trivial*. Every Tach diagnostic carries a machine-applicable
`preferred_patch` — a byte-span replacement that fixes the problem — and `tach fix` can
drive a broken project from red to green **with no model in the loop**. The goal runtime
wraps that loop in durability: budgets, checkpoints, resume, replay, and a per-step event
log.

This repo is a **working compiler, toolchain, and goal runtime**: a real lexer, a Pratt
parser, a type + effect checker that emits patch-carrying diagnostics, user-defined sum
types with `match`, a deterministic interpreter, a hermetic test runner, the typed-patch
verification pipeline, the agent repair loop (with a pluggable coder seam), the `goal`
language construct, a durable goal store with append-only event history and
crash/resume/replay, a canonical formatter, versioned JSON schemas, and a suite-level
benchmark — all in Rust, in a single static binary with **zero language-model dependency**
for everything that matters.

## The 60-second demo

```console
$ tach new demo && cd demo

$ tach check
error[E0421]: function `load_session` performs undeclared effects `db.read`, `log.write`, `time.read`
  --> src/auth.tach:15:4
15 | fn load_session(token: String) -> Result<Session, AuthError> {
   |    ^^^^^^^^^^^^
     = agent  kind=effect_undeclared  strategies=[add_effect]
       patch insert `effects [db.read, log.write, time.read] ` — declare the effects this function performs
  ● 3 error(s), 0 warning(s)

$ tach fix
Tach fix · strategy minimal

  ✓ lap 1  applied fix:effect_undeclared  src/auth.tach:15
          ↳ effects [db.read, log.write, time.read]    scope src/auth.tach
          proof 3 impacted tests pass · 2 errors left
  ✓ lap 2  applied fix:unknown_module  src/auth.tach:18
          ↳ ⏎import log   scope src/auth.tach
          proof 3 impacted tests pass · 1 errors left
  ✓ lap 3  applied fix:type_mismatch  src/auth.tach:24
          ↳ Int   scope src/auth.tach
          proof 3 impacted tests pass · 0 errors left

  ● green in 3 laps · 3 patches · 9 tests run · 0 regressions · 610µs
```

**Tach fixed the bug in three laps** — with no model in the loop. The intelligence is in
the compiler's output, not the agent.

## The goal runtime

`tach fix` is the loop. A **goal** is that loop made durable: budgeted, authority-scoped,
checkpointed after every step, and resumable. A fresh project ships with one:

```tach
goal FixFailingTests -> Success {
  budget {
    steps: 30
    retries: 3
  }
  allow {
    effect db.read
    effect db.write
    effect time.read
    effect log.write
    fs.write ["src/**", "tests/**"]
  }
  require {
    tests.pass
    no_new_effects
  }
}
```

The killer demo: **run it, crash it mid-flight, and resume to green without repeating a
single patch.**

```console
$ tach goal run FixFailingTests --crash-after step:2
● FixFailingTests  run_69c04fc4d55f672e
  status               running
  steps                2
  patches-applied      2
  ✗ crashed after step 2 (simulated). State is durable.
  resume with  tach goal resume run_69c04fc4d55f672e

$ tach check        # the working tree is untouched — a crash never half-edits your code
  ● 1 error(s) ...

$ tach goal resume run_69c04fc4d55f672e
● FixFailingTests  run_69c04fc4d55f672e
  status               completed
  steps                3
  patches-applied      3       ← the third patch only; the first two were not repeated
  ✓ updated src/auth.tach

$ tach goal replay run_69c04fc4d55f672e
  ● replay reproduced the run exactly — 3 step(s), completed
```

Everything the run did is an immutable line in an append-only log
(`.tach/goals/<id>/events.jsonl`, schema `tach.event.v1`):

```console
$ tach goal inspect run_69c04fc4d55f672e
event history
   3  diagnostic.emitted     E0421 effect_undeclared
   4  patch.proposed         fix:effect_undeclared
   6  patch.applied          fix:effect_undeclared
   8  checkpoint.written     step 1
  ...
  15  run.resumed            from step 2
  21  checkpoint.written     step 3
  22  run.completed
```

The goal's `allow` block is **authority, not documentation**: a patch that would write
outside `src/**`/`tests/**`, or perform an effect the goal was never granted, is rejected
by the same verification pipeline that runs the tests — *before* it touches disk. The
runtime is deterministic and offline: a run's id is a *fingerprint* of the goal and its
source — no clock, no randomness — so the first run is addressable as a clean `run_<hash>`.
A fingerprint is not an identity, though: a fresh run of the same goal over the same source
atomically claims a new, unique id (`run_<hash>-2`, `-3`, …) and can never overwrite a
prior run's history.

## The action layer: a goal that *does* something

Repair goals fix code. **Action goals run a business workflow** — they propose effectful
actions, pause for human approval, call tools, and prove each effect with a durable
**receipt**. Two ship built-in (no setup): `ResolveDuplicateCharge` and `ShipHotfixPR`.
Everything is offline and deterministic — the "tools" are fakes, no key required.

The killer demo: **a goal that refunds a duplicate charge behind an approval gate, survives a
crash mid-refund, and refunds exactly once.**

```console
$ tach goal run ResolveDuplicateCharge
● ResolveDuplicateCharge  run_3f8b062b8280c330
  status               awaiting_approval
  actions-executed     1
  cursor               1
  receipts             0
  awaiting-approval    apr_efa727c214fb
  ⏸ awaiting approval — review with tach goal approvals run_3f8b062b8280c330

$ tach goal approve run_3f8b062b8280c330 apr_efa727c214fb
  ✓ approved `apr_efa727c214fb` — resume with tach goal resume run_3f8b062b8280c330

$ tach goal resume run_3f8b062b8280c330 --crash-after step:4
  ✗ crashed after step 4 (simulated). State is durable.   # refund already done & receipted

$ tach goal resume run_3f8b062b8280c330
● ResolveDuplicateCharge  run_3f8b062b8280c330
  status               completed
  receipts             3
  ✓ goal completed — 3 receipt(s). See tach goal receipts run_3f8b062b8280c330

$ tach goal receipts run_3f8b062b8280c330
  ● rcpt_235edb8551e6  fake.stripe.refund    refund     ← exactly one refund, ever
  ● rcpt_2eb1d78d6950  fake.zendesk.comment  close
  ● rcpt_edf8e1dd64b4  fake.email.send       notify

$ tach goal replay run_3f8b062b8280c330
  ● replay reproduced the run exactly — 4 step(s), completed
```

Why the crash can't double-refund: **advancing the plan cursor (and saving state) is the only
commit that moves past an action.** The receipt is written *before* that commit, so a crash in
between leaves the receipt durable but the cursor unmoved — and on resume the runtime finds the
receipt by its idempotency key and emits `receipt.reused` instead of calling Stripe again. The
approval file (`approvals/<id>.json`) is the single source of truth for a gate; the human's
`approve`/`deny` is recorded once as a real event. Authority is enforced too: a plan can only
call the tools its `allow` block grants. The whole lifecycle is in the log — `action.proposed`,
`approval.requested`/`granted`, `tool.called`/`completed`, `receipt.created`/`reused`.

## The plan language: loops and long-horizon workflows

The action layer runs a fixed list of steps. The plan language is the general version of that:
a `plan { ... }` block in goal source with real control flow — `let` bindings, tool `call`s,
`approve` gates, `if`/`else`, and `for`/`while` loops. A goal can pull a list from a tool, loop
over it, branch on each item, pause for approval on each one, and survive a crash anywhere
without repeating a side effect.

```text
goal ReconcileChargebacks -> Success {
  budget { steps: 60 }
  allow { fake.stripe.list_disputes; fake.stripe.refund; fake.email.send; fake.zendesk.comment }
  plan {
    let disputes = call fake.stripe.list_disputes { customer: "cus_42" }
    for charge in disputes.charges {
      if charge.is_duplicate {
        approve "refund the duplicate charge" {
          call fake.stripe.refund { charge_id: charge.charge_id, amount_cents: charge.amount_cents }
          call fake.email.send   { to: "billing@acme.test", charge_id: charge.charge_id }
        }
      } else {
        call fake.zendesk.comment { ticket_id: "zd_dispute", body: "Reviewed: not a duplicate." }
      }
    }
  }
}
```

Loops stay safe because of **durable re-execution**. A run and a resume both walk the plan from
the top, and every tool call is memoized by its receipt: reach a call whose receipt already
exists and it hands back the recorded output instead of calling the tool again. So a goal with
two real duplicates pauses twice, once per refund gate. Crash right after the first refund, and
the resume re-walks the loop, replays the finished work for free, and picks up where it stopped.
The first refund never runs twice.

```console
$ tach goal run ReconcileChargebacks          # pauses at the FIRST duplicate's refund gate
$ tach goal approve <id> <apr>                 # grant it
$ tach goal resume <id> --crash-after step:1   # crash right after the refund is receipted
  ✗ crashed after step 1 (simulated). State is durable.
$ tach goal resume <id>                         # refund #1 is memoized, NOT repeated → pauses at the 2nd gate
$ tach goal approve <id> <apr2> && tach goal resume <id>
  ✓ goal completed — 6 receipt(s).
$ tach goal receipts <id>                       # exactly two fake.stripe.refund receipts — one per duplicate
$ tach goal replay <id>
  ● replay reproduced the run exactly — 6 step(s), completed
```

`RetryFlakyDeploy` is a `while` loop: it retries a flaky deploy until it works (it fails on
attempts 1–2 and succeeds on the 3rd, deterministically), then announces it behind an approval
gate. Resume a crashed retry loop and it re-derives the attempt count without re-running any
deploy that already has a receipt. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the
interpreter and its exactly-once invariant.

### Write your own

Plan goals aren't limited to the built-ins — write one in your own project and run it the same
way. `tach new demo --goal chargebacks` scaffolds a `goal.tach` with a `ReconcileLocalDemo` plan
goal you own; `tach goal check ReconcileLocalDemo` validates the plan before you run it (ungranted
tools, unknown tools, unbound variables, unsupported expressions, and loops that can't make
progress all fail here, not at runtime).

When a run starts, Tach snapshots the goal source into the run record. Resume and replay re-parse
that **frozen snapshot**, never the live file — so editing `goal.tach` after a run begins cannot
change a run already in flight. Each receipt is self-describing for audit: it records the run and
step it committed at, the effect, a hash of the input, the approval that authorized it, and the
history event that recorded it (`tach goal receipt <id> <rcpt>`).

## The coding harness: govern an external agent on a real repo

Everything above operates on Tach's own language. The **coding harness** turns the same
runtime — authority scopes, real command execution, receipts, replay — outward, onto an
existing Rust / JS / Go / Python repo edited by an external agent (Claude Code, Codex,
Cursor). Tach does not replace those agents. It makes their work scoped, verified,
replayable, and auditable. *The agent brings the reasoning; Tach is the guardrail and the
ledger.*

```bash
cd some-existing-repo
tach init --existing            # writes Tachfile, TACH_AGENT.md, .tachignore (detects `cargo test`)
tach guard begin FixFailingTests
#   … the agent reads `tach guard context --json` and edits files …
tach guard verify               # runs the real test command; captures a receipt; sets verified
tach guard finalize             # finalizes only if verified — and only in Tach's ledger, never git
```

The `Tachfile` is the contract: a goal in the same grammar, scoped to a real tree.

```tach
goal FixFailingTests -> Success {
  budget {
    steps: 40
  }
  allow {
    fs.write ["src/**", "tests/**"]
    shell.run ["cargo test"]
  }
  require {
    command("cargo test").passes
    no_out_of_scope_writes
  }
}
```

What Tach guarantees across a session:

- **Scope gate.** `begin` snapshots the working tree; `verify`/`commit` diff against it and
  reject any change outside the `fs.write` globs. Without a sandbox Tach can't *prevent* the
  write — it is an honest detect-and-reject gate: the violation lands in `events.jsonl` and
  the commit is blocked.
- **Real commands, real receipts.** Each required command runs as a real process with a fixed
  cwd, a scrubbed environment (secrets in the parent env never reach the child), a timeout that
  kills the whole process group on overrun (so a runaway and its children can't outlive it),
  and stdout/stderr captured to artifacts. The exit code becomes a durable
  [receipt](#3-effects-are-first-class). `tach guard verify` reports `verified: true` only
  when every required command passed and nothing out-of-scope changed — the one bit the agent
  is told never to claim "done" without.
- **Crash-safe and replayable.** A receipt is keyed by the command *and a digest of the tree
  it ran against*, so a crashed-then-re-run `verify` over an unchanged tree reuses the proof
  instead of re-running, while an edited tree correctly re-runs. `tach goal replay <id>`
  re-derives the verdict from the recorded receipts without re-executing; `--rerun` actually
  re-runs and compares.

Nondeterministic evidence (stdout bytes, durations, exit timing) lives in receipts and
artifacts; the control-flow event log stays deterministic — the same separation the action
layer already relies on.

## Why this is different

Rust made memory safety central. Bun made JS tooling feel instant. Tach makes the
**agentic coding loop** feel instant, and measures itself on a new axis:

| Old benchmark        | Tach benchmark         |
| -------------------- | ---------------------- |
| `12 ns` per call     | **time-to-green**      |
| binary size          | **patches-to-green**   |
| compile time         | **tests-run-per-fix**  |
| —                    | **regressions-per-run**|

```console
$ tach bench
Tach bench · agent loop
  status               green
  time-to-green        481µs
  laps-to-green        3
  patches-applied      3
  tests-run            9
  regressions          0
  diff-size            54 chars
```

## Three ideas that make it work

### 1. Diagnostics are repairs

Every error is *agent-shaped*. The same diagnostic renders as a friendly caret block
for humans and as stable JSON for machines:

```console
$ tach check --json
```
```json
{
  "code": "E0421",
  "kind": "effect_undeclared",
  "span": { "start": 240, "end": 252 },
  "repair_strategies": ["add_effect"],
  "preferred_patch": {
    "file": "src/auth.tach",
    "span": { "start": 300, "end": 300 },
    "replacement": "effects [db.read, log.write, time.read] ",
    "rationale": "declare the effects this function performs"
  }
}
```

Nine diagnostics ship today, and every one that admits a mechanical fix carries a
`preferred_patch`:

| Code | Kind | The fix it carries |
| --- | --- | --- |
| `E0322` | `unknown_module` | insert the missing `import` |
| `E0421` | `effect_undeclared` | add / extend the `effects [...]` clause |
| `E0450` | `effect_unused` *(warning)* | trim effects the function doesn't perform |
| `E0309` | `type_mismatch` | fix the annotation, or convert the value |
| `E0330` | `unknown_field` | rename to the nearest field (did-you-mean) |
| `E0340` | `non_exhaustive_match` | insert an arm for each missing variant |
| `E0341` | `unknown_variant` | rename to the nearest variant (did-you-mean) |
| `E0460` | `unused_import` *(warning)* | remove the unused `import` line |
| `E0461` | `unused_variable` *(warning)* | prefix the binding with `_` |

The did-you-mean repairs only fire when a real name is a small edit away — they never guess
wildly, because an agent would dutifully apply a bad rename.

### 2. Agents submit typed patches, not raw diffs

The repair loop never edits files directly. It submits a **scoped, typed patch** that the
pipeline verifies *before* anything is written. A patch is rejected if it:

- **reaches outside its declared scope** — `touched file outside allowed scope: src/billing.tach`
- **introduces a new effect** — `introduced new effect: net.write`
- **breaks compilation** — `patch introduced 1 syntax error`
- **regresses a test** — `regressed test: auth.refresh.expired_token`

Only the **impacted** tests (found by call-graph analysis) are run, so verification is
proportional to the change, not the repo.

### 3. Effects are first-class

A function declares what it can do. The checker infers what it *actually* does and
reconciles the two. That makes a function's blast radius obvious to a reviewer — or an
agent deciding whether it's safe to run in a test:

```console
$ tach audit
  load_session (String) -> Result<Session, AuthError> effects [db.read, log.write, time.read]
      ⚠ db.read   reads from the database
      log.write   writes to the log
      time.read   reads the wall clock
  session_summary (Session) -> Int
      pure — no declared effects
```

## Racing repair strategies

Agentic coding is parallel. `tach race` runs several repair strategies speculatively,
each in an **isolated copy** of the workspace, then merges the first verified winner —
green, with the smallest diff:

```console
$ tach race
Tach race · 3 branches, isolated
  ● minimal   green · 3 laps · diff  54 · 795µs   ← winner
  ● convert   green · 3 laps · diff  71 · 880µs
  ● strict    green · 3 laps · diff  54 · 755µs
  ● winner: minimal — green with the smallest verified diff
```

Because every run is deterministic, `tach trace` and `tach replay` re-open and reproduce
any loop exactly.

## Install / build

Requires a Rust toolchain (1.75+).

```console
$ git clone <this-repo> tach && cd tach
$ cargo build --release
$ ./target/release/tach --help
```

The result is a single static binary. Put `target/release/tach` on your `PATH`.

## Commands

| Command | What it does |
| --- | --- |
| `tach new <name>` | Scaffold a project (`--clean` for an empty one, `--goal chargebacks` for a plan-goal demo) |
| `tach init --existing` | Adopt an existing repo: write `Tachfile`, `TACH_AGENT.md`, `.tachignore` (`--force` to overwrite) |
| `tach goal init <template>` | Write a `Tachfile` coding goal (e.g. `coding.fix-tests`) |
| `tach guard begin <Goal>` | Open a coding-agent session over the working tree |
| `tach guard status` / `context` | Status line, or the agent's operating contract (`--json`) |
| `tach guard diff` | Changed files since the baseline, classified by `fs.write` scope (`--json`) |
| `tach guard verify` | Run the goal's required commands for real; set the `verified` bit (`--rerun`) |
| `tach guard finalize` / `abort` | Finalize verified changes into Tach's ledger (ledger-only, never git; `commit` is an alias), or cancel the session |
| `tach check [file]` | Type- and effect-check; `--json` for the machine view |
| `tach run [file]` | Run the project's `main` |
| `tach test [filter]` | Run tests (blocked while the project has errors) |
| `tach fix` | Run the agentic repair loop to green (`--strategy`, `--dry-run`, `--coder fixture`) |
| `tach fmt [file]` | Format to the one canonical style (`--check` for CI) |
| `tach race` | Race repair strategies in isolation; `--apply` the winner |
| `tach trace` | Show the last fix/race run (`--json`) |
| `tach replay` | Re-run the last loop and prove it reproduces |
| `tach bench` | Report agent-loop metrics (time-to-green, laps, …); `--suite <dir>` over a corpus |
| `tach audit [file]` | Show every function's effect surface |
| `tach goal run <name>` | Start a durable run (`--crash-after step:N`, `--strategy`, `--dry-run`) |
| `tach goal check <name>` | Statically validate a goal's plan before running it (`--json`) |
| `tach goal list` | List runs in the store |
| `tach goal inspect <id>` | Show a run's state and event history (`--json`) |
| `tach goal resume <id>` | Resume a crashed/incomplete run from its last checkpoint |
| `tach goal replay <id>` | Re-run from base and prove it reproduces |
| `tach goal cancel <id>` | Cancel a run |
| `tach goal approvals <id>` | List a run's approval gates |
| `tach goal approve <id> <apr>` | Grant a pending approval (`--note`) |
| `tach goal deny <id> <apr>` | Deny a pending approval (`--reason`) |
| `tach goal receipts <id>` | List a run's effect receipts |
| `tach goal receipt <id> <rcpt>` | Show one receipt in full (`--json`) |
| `tach doctor` | Hermetic health check of the toolchain + workspace |
| `tach explain <code>` | Long-form explanation of a diagnostic code |
| `tach schema [name]` | Print a versioned JSON schema for any machine output — `diagnostic`, `patch`, `event`, `goal`, `run`, `approval`, `receipt`, `bench`, `test`, and the guard packets `guard-context`, `guard-status`, `guard-diff`, `guard-verify`, `guard-commit` |

## A taste of the language

Tach is deliberately boring for humans and easy for models: one formatter's worth of
style, explicit effects, explicit `?`, no macro magic.

```tach
import db
import time

type Session = {
  token: String
  user_id: Int
  expires_at: Int
}

fn load_session(token: String) -> Result<Session, AuthError>
  effects [db.read, time.read]
{
  let row = db.query("select * from sessions where token = ?", token)?
  ensure row.expires_at > time.now()
  return Ok(Session { token: row.token, user_id: row.user_id, expires_at: row.expires_at })
}

test "expired session is rejected" {
  db.seed("old", { token: "old", user_id: 7, expires_at: 1 })
  ensure load_session("old").is_err()
}
```

It also has user-defined **sum types** and `match` — and a `match` on an enum must cover
every variant, or the compiler hands you a patch that inserts the missing arm:

```tach
type Parity = Even | Odd

fn describe(p: Parity) -> String {
  return match p {
    Even => "even"
    Odd => "odd"
  }
}
```

There's **one formatter**: `tach fmt` renders any file to a single canonical, idempotent
style, and `tach fmt --check` gates it in CI.

See [`docs/LANGUAGE.md`](docs/LANGUAGE.md) for the full tour.

## How it's built

Single Rust crate, cleanly separated modules:

```
src/
  lexer · parser · ast       front-end (spans double as edit coordinates)
  check · types · builtins   type + effect analysis → structured diagnostics
  interp · runner            deterministic tree-walking runtime + test runner
  patch                      Workspace, typed patches, the verify pipeline, impact analysis
  agent                      the fix loop, the coder seam, race, suite bench, metrics
  goal · event · store       the goal contract, append-only event history, durable store
  runtime                    the durable executor: budgets, checkpoint, resume, replay
  fmt · schema               the one canonical formatter, versioned JSON schemas
  trace · render · cli       persistence, pretty output, the `tach` binary
```

The design notes are in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## What's real, and what's next

**Real today:** the language runs — records, `Result`, effects, and user-defined sum
types with `match`; the checker finds effect, type, import, field, and exhaustiveness
bugs and emits machine-applicable patches (including did-you-mean renames and an
insert-the-missing-arm fix for non-exhaustive matches); the typed-patch pipeline enforces
scope, effects, and regressions; the deterministic loop drives the demo to green;
`tach bench --suite corpus` benchmarks the loop over a suite of broken projects, one per
diagnostic family. **The goal runtime is real:** `goal` is a first-class language
construct with `budget`/`allow`/`require` blocks; `tach goal run` drives the loop under
those constraints, checkpointing after every step into a durable store with an append-only
`tach.event.v1` event history; `tach goal resume` recovers a crashed run from its last
checkpoint **without repeating work**; `tach goal replay` proves a run reproduces; and the
`allow` block is enforced as real authority by the verification pipeline. **The action layer
is real too:** built-in business goals (`tach goal run ResolveDuplicateCharge`) run a fixed
plan that proposes effectful actions, pauses for human approval (`tach goal approvals` /
`approve` / `deny`), calls offline **fake tools**, and records a durable **receipt** for every
effect (`tach goal receipts` / `receipt`). Idempotency keys make it survive crash/resume with
*no duplicate side effect*, and `tach goal replay` reproduces the effects from the recorded
approvals. **The plan language is real:** a `plan { ... }` block (`let`/`call`/`approve`/`if`/
`for`/`while`) drives built-in goals like `ReconcileChargebacks` (a `for` loop with a per-duplicate
approval gate) and `RetryFlakyDeploy` (a `while` retry loop). Its **durable re-execution interpreter**
re-walks the plan on every run and resume and memoizes completed calls by receipt, so loops and
crashes still produce each effect exactly once, and `tach goal replay` reproduces the run. Durable
writes are atomic (temp file, then rename), so a crash mid-write can't strand a half-written receipt
that a resume would read as "not yet done." There's a pluggable coder seam
(`tach fix --coder fixture`) whose proposals still go through the exact same pipeline; `tach fmt`
gives one canonical, idempotent style; `tach schema` publishes versioned JSON schemas for every
machine output (including `approval` and `receipt`); and `tach doctor` / `tach explain` round out
the toolchain. **125 passing tests** plus end-to-end checks (red→green, crash→resume→replay, the
approval/refund/receipt demo, the loop/approval/crash plan demo, a user-authored plan goal
that resumes off its source snapshot, and the coding harness adopting a real repo and rejecting
an out-of-scope edit) and a schema-validation step in CI.

**Near-term follow-ups (the roadmap the runtime is built for):** real tool integrations behind
the fake-tool seam, typed memory lanes with a context-drift detector, MCP client/server, and a
portable goal ABI. The event log, durable store, authority model, and the approval/receipt
substrate are exactly what those phases hang off. (User-authored plan goals — a `plan` block in
your own workspace, `run`/`check`/`resume`/`replay`d off a source snapshot — already work, and so
does the coding harness: `tach init --existing` adopts a real Cargo/npm/Bun/Go/pytest repo and
`tach guard` scopes, verifies, and replays an external agent's edits.) Also: multi-file user
imports and comment-preserving formatting.

**Deliberately scoped out:** native/LLVM codegen (today it interprets), a borrow checker,
a package manager, an LSP server, and a *model-backed* coder. The loop already has the
seam — a `Coder` trait, exercised offline by a deterministic fixture coder — so a real
model slots in behind a flag later, with every model output flowing through the same
patch/effect/test/authority pipeline. The core demo and the whole test suite stay
model-free, so everything is fully reproducible offline.

## Testing

```console
$ cargo test                   # unit + integration tests (125)
$ bash scripts/e2e.sh          # new → check → fix → test demo, asserts green
$ bash scripts/goal_e2e.sh     # goal run → crash → resume → replay, asserts no repeated work
$ bash scripts/action_e2e.sh   # approve → crash → resume → replay, asserts exactly one refund
$ bash scripts/plan_e2e.sh     # plan loop → per-duplicate approval → mid-loop crash → exactly-once
$ bash scripts/user_plan_e2e.sh # scaffold → check → crash → snapshot-beats-live-edit → replay
$ bash scripts/guard_e2e.sh    # coding harness: adopt → verify → crash/resume → finalize → out-of-scope reject → replay
```

CI (`.github/workflows/ci.yml`) runs all six on every push, plus `tach fmt --check` and
JSON-schema validation. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for notes aimed at
automated/cloud agents.

## License

MIT — see [`LICENSE`](LICENSE).
