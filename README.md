<div align="center">

# Perdure

**A typed goal runtime for long-horizon agents.**

*The fastest path from a failing goal to a verified result.*

</div>

---

Perdure gives an agent's goals a **typed, deterministic, auditable control plane**. A goal
runs with a budget it can't exceed, an authority it can't escape, checkpoints it can
resume from, an event history that records everything it did, and a verification pipeline
that refuses any change that breaks the build, regresses a test, or reaches outside the
scope it was granted. Use Perdure when you want an agent to pursue a goal **without losing
state, exceeding authority, repeating side effects, or making untraceable changes.**

The foundation is a language whose **compiler is also an agent harness**. Most languages
were designed for humans to write and a machine to run; Perdure is designed for the loop
agents live inside:

```
goal → budget → authority → diagnostic → typed patch → verify → checkpoint → resume → trace
```

The bet that makes it work: if the compiler emits *repairs* instead of just complaints,
the agent driving it can be *trivial*. Every Perdure diagnostic carries a machine-applicable
`preferred_patch` — a byte-span replacement that fixes the problem — and `perdure fix` can
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
$ perdure new demo && cd demo

$ perdure check
error[E0421]: function `load_session` performs undeclared effects `db.read`, `log.write`, `time.read`
  --> src/auth.pdr:15:4
15 | fn load_session(token: String) -> Result<Session, AuthError> {
   |    ^^^^^^^^^^^^
     = agent  kind=effect_undeclared  strategies=[add_effect]
       patch insert `effects [db.read, log.write, time.read] ` — declare the effects this function performs
  ● 3 error(s), 0 warning(s)

$ perdure fix
Perdure fix · strategy minimal

  ✓ lap 1  applied fix:effect_undeclared  src/auth.pdr:15
          ↳ effects [db.read, log.write, time.read]    scope src/auth.pdr
          proof 3 impacted tests pass · 2 errors left
  ✓ lap 2  applied fix:unknown_module  src/auth.pdr:18
          ↳ ⏎import log   scope src/auth.pdr
          proof 3 impacted tests pass · 1 errors left
  ✓ lap 3  applied fix:type_mismatch  src/auth.pdr:24
          ↳ Int   scope src/auth.pdr
          proof 3 impacted tests pass · 0 errors left

  ● green in 3 laps · 3 patches · 9 tests run · 0 regressions · 610µs
```

**Perdure fixed the bug in three laps** — with no model in the loop. The intelligence is in
the compiler's output, not the agent.

## The goal runtime

`perdure fix` is the loop. A **goal** is that loop made durable: budgeted, authority-scoped,
checkpointed after every step, and resumable. A fresh project ships with one:

```perdure
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
$ perdure goal run FixFailingTests --crash-after step:2
● FixFailingTests  run_69c04fc4d55f672e
  status               running
  steps                2
  patches-applied      2
  ✗ crashed after step 2 (simulated). State is durable.
  resume with  perdure goal resume run_69c04fc4d55f672e

$ perdure check        # the working tree is untouched — a crash never half-edits your code
  ● 1 error(s) ...

$ perdure goal resume run_69c04fc4d55f672e
● FixFailingTests  run_69c04fc4d55f672e
  status               completed
  steps                3
  patches-applied      3       ← the third patch only; the first two were not repeated
  ✓ updated src/auth.pdr

$ perdure goal replay run_69c04fc4d55f672e
  ● replay reproduced the run exactly — 3 step(s), completed
```

Everything the run did is a line in an append-only, **hash-chained** log
(`.perdure/goals/<id>/events.jsonl`, schema `perdure.event.v1`) — each event commits to the one
before it, so any later edit is detectable (`perdure guard audit`):

```console
$ perdure goal inspect run_69c04fc4d55f672e
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
$ perdure goal run ResolveDuplicateCharge
● ResolveDuplicateCharge  run_3f8b062b8280c330
  status               awaiting_approval
  actions-executed     1
  cursor               1
  receipts             0
  awaiting-approval    apr_efa727c214fb
  ⏸ awaiting approval — review with perdure goal approvals run_3f8b062b8280c330

$ perdure goal approve run_3f8b062b8280c330 apr_efa727c214fb
  ✓ approved `apr_efa727c214fb` — resume with perdure goal resume run_3f8b062b8280c330

$ perdure goal resume run_3f8b062b8280c330 --crash-after step:4
  ✗ crashed after step 4 (simulated). State is durable.   # refund already done & receipted

$ perdure goal resume run_3f8b062b8280c330
● ResolveDuplicateCharge  run_3f8b062b8280c330
  status               completed
  receipts             3
  ✓ goal completed — 3 receipt(s). See perdure goal receipts run_3f8b062b8280c330

$ perdure goal receipts run_3f8b062b8280c330
  ● rcpt_235edb8551e6  fake.stripe.refund    refund     ← exactly one refund, ever
  ● rcpt_2eb1d78d6950  fake.zendesk.comment  close
  ● rcpt_edf8e1dd64b4  fake.email.send       notify

$ perdure goal replay run_3f8b062b8280c330
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
$ perdure goal run ReconcileChargebacks          # pauses at the FIRST duplicate's refund gate
$ perdure goal approve <id> <apr>                 # grant it
$ perdure goal resume <id> --crash-after step:1   # crash right after the refund is receipted
  ✗ crashed after step 1 (simulated). State is durable.
$ perdure goal resume <id>                         # refund #1 is memoized, NOT repeated → pauses at the 2nd gate
$ perdure goal approve <id> <apr2> && perdure goal resume <id>
  ✓ goal completed — 6 receipt(s).
$ perdure goal receipts <id>                       # exactly two fake.stripe.refund receipts — one per duplicate
$ perdure goal replay <id>
  ● replay reproduced the run exactly — 6 step(s), completed
```

`RetryFlakyDeploy` is a `while` loop: it retries a flaky deploy until it works (it fails on
attempts 1–2 and succeeds on the 3rd, deterministically), then announces it behind an approval
gate. Resume a crashed retry loop and it re-derives the attempt count without re-running any
deploy that already has a receipt. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the
interpreter and its exactly-once invariant.

### Write your own

Plan goals aren't limited to the built-ins — write one in your own project and run it the same
way. `perdure new demo --goal chargebacks` scaffolds a `goal.pdr` with a `ReconcileLocalDemo` plan
goal you own; `perdure goal check ReconcileLocalDemo` validates the plan before you run it (ungranted
tools, unknown tools, unbound variables, unsupported expressions, and loops that can't make
progress all fail here, not at runtime).

When a run starts, Perdure snapshots the goal source into the run record. Resume and replay re-parse
that **frozen snapshot**, never the live file — so editing `goal.pdr` after a run begins cannot
change a run already in flight. Each receipt is self-describing for audit: it records the run and
step it committed at, the effect, a hash of the input, the approval that authorized it, and the
history event that recorded it (`perdure goal receipt <id> <rcpt>`).

## The coding harness: govern an external agent on a real repo

Everything above operates on Perdure's own language. The **coding harness** turns the same
runtime — authority scopes, real command execution, receipts, replay — outward, onto an
existing Rust / JS / Go / Python repo edited by an external agent (Claude Code, Codex,
Cursor). Perdure does not replace those agents. It makes their work scoped, verified,
replayable, and auditable. *The agent brings the reasoning; Perdure is the guardrail and the
ledger.*

```bash
cd some-existing-repo
perdure init --existing            # writes Perdurefile, PERDURE_AGENT.md, AGENTS.md, .perdureignore (detects `cargo test`)
perdure guard begin FixFailingTests
#   … the agent reads `perdure guard context --for-agent generic --json` and edits files …
perdure guard verify               # runs the real test command; captures a receipt; sets verified
perdure guard next                 # the single next required action (edit / fix scope / finalize / done)
perdure guard finalize             # finalizes only if verified — and only in Perdure's ledger, never git
perdure guard audit                # (operator/CI) prove the ledger wasn't tampered: chain + receipts + verified bit
```

The `Perdurefile` is the contract: a goal in the same grammar, scoped to a real tree.

```perdure
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

What Perdure guarantees across a session:

- **Scope gate.** `begin` snapshots the working tree and **freezes the ignore set**;
  `verify`/`commit` diff against that baseline (using SHA-256 content hashes, so a change can't be
  crafted to hash equal to the original) and reject any change outside the `fs.write` globs. The
  ignore rules are fixed at `begin` and `.perdureignore`/`.gitignore` are never self-ignored, so an
  agent can't edit them mid-session to shrink the gate's view; `guard diff` surfaces the gate's
  blind spots (conventional build/dependency roots) explicitly. A file the *authorized command
  itself* generates (a refreshed `Cargo.lock`) is attributed as tool-generated and doesn't trip
  the gate — only the agent's own out-of-scope edits do. Without a sandbox Perdure still can't
  *prevent* a write — it is an honest detect-and-reject gate: the violation lands in
  `events.jsonl` and the commit is blocked.
- **Real commands, real receipts.** Each required command runs as a real process with a fixed
  cwd, a scrubbed environment (secrets in the parent env never reach the child), a timeout that
  kills the whole process group on overrun (so a runaway and its children can't outlive it),
  and stdout/stderr captured to artifacts. The exit code becomes a durable
  [receipt](#3-effects-are-first-class). `perdure guard verify` reports `verified: true` only
  when every required command passed and nothing out-of-scope changed — the one bit the agent
  is told never to claim "done" without.
- **Crash-safe, durable, and replayable.** A receipt is keyed by the command *and a digest of the
  tree it ran against*, so a crashed-then-re-run `verify` over an unchanged tree reuses the proof
  instead of re-running, while an edited tree correctly re-runs. Durable writes are `fsync`'d
  (file then directory), so the guarantee survives a power loss or kernel panic, not just a clean
  process exit. `perdure goal replay <id>` re-derives the verdict from the recorded receipts without
  re-executing; `--rerun` actually re-runs and compares.
- **Tamper-evident and auditable.** The event log is a SHA-256 hash chain, so editing, inserting,
  removing, or reordering any event breaks every link after it. `perdure guard audit` re-derives a
  run's integrity from outside the agent — the chain, each receipt's anchoring and untampered
  input hash, and whether the recorded `verified` bit is actually supported by the receipts — and
  exits non-zero if anything was forged. With no sandbox an agent with `.perdure/` write access can
  still *corrupt* its own ledger; what it can't do is forge a self-consistent one. Detection, not
  prevention, is the guarantee here.

Nondeterministic evidence (stdout bytes, durations, exit timing) lives in receipts and
artifacts; the control-flow event log stays deterministic — the same separation the action
layer already relies on.

### The agent's structured front door

An external agent never has to infer state from prose. Every answer it needs is a stable,
versioned JSON packet:

- `perdure guard context --for-agent generic --json` — the full operating contract: allowed
  files and commands, the classified change set, the latest command receipts (with their
  captured stdout/stderr artifacts), the current failure, the `done_condition`, and the next
  action. Pin the shape with its `schema` field (`perdure.agent-context.v1`).
- `perdure guard next --json` — just the one next required action, with the exact command to run:
  `edit_then_verify` → `fix_scope_violation` → `run_verify` → `finalize` → `done`.
- A refused `perdure guard verify --json` carries machine-actionable repair hints: the offending
  paths, the scope that would have allowed them, named `repair_strategies`, and a
  `preferred_next_action` (e.g. `revert_file`) — compiler-diagnostic ergonomics for a guard
  refusal.
- `perdure serve-mcp` exposes these same safe operations as MCP tools over stdio — **server-only,
  with no raw shell and no arbitrary file writes** — so an MCP-speaking agent can drive a
  guarded repo through structured tool calls. Perdure embeds no model; the agent stays outside.

`perdure init --existing` also writes a vendor-neutral `AGENTS.md` (only when absent — a
user-authored one is never touched) spelling out this contract, alongside the Perdure-specific
`PERDURE_AGENT.md`.

## Why this is different

Rust made memory safety central. Bun made JS tooling feel instant. Perdure makes the
**agentic coding loop** feel instant, and measures itself on a new axis:

| Old benchmark        | Perdure benchmark         |
| -------------------- | ---------------------- |
| `12 ns` per call     | **time-to-green**      |
| binary size          | **patches-to-green**   |
| compile time         | **tests-run-per-fix**  |
| —                    | **regressions-per-run**|

```console
$ perdure bench
Perdure bench · agent loop
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
$ perdure check --json
```
```json
{
  "code": "E0421",
  "kind": "effect_undeclared",
  "span": { "start": 240, "end": 252 },
  "repair_strategies": ["add_effect"],
  "preferred_patch": {
    "file": "src/auth.pdr",
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

- **reaches outside its declared scope** — `touched file outside allowed scope: src/billing.pdr`
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
$ perdure audit
  load_session (String) -> Result<Session, AuthError> effects [db.read, log.write, time.read]
      ⚠ db.read   reads from the database
      log.write   writes to the log
      time.read   reads the wall clock
  session_summary (Session) -> Int
      pure — no declared effects
```

## Racing repair strategies

Agentic coding is parallel. `perdure race` runs several repair strategies speculatively,
each in an **isolated copy** of the workspace, then merges the first verified winner —
green, with the smallest diff:

```console
$ perdure race
Perdure race · 3 branches, isolated
  ● minimal   green · 3 laps · diff  54 · 795µs   ← winner
  ● convert   green · 3 laps · diff  71 · 880µs
  ● strict    green · 3 laps · diff  54 · 755µs
  ● winner: minimal — green with the smallest verified diff
```

Because every run is deterministic, `perdure trace` and `perdure replay` re-open and reproduce
any loop exactly.

## Install / build

Requires a Rust toolchain (1.75+).

```console
$ git clone <this-repo> perdure && cd perdure
$ cargo build --release
$ ./target/release/perdure --help
```

The result is a single static binary. Put `target/release/perdure` on your `PATH`.

## Commands

| Command | What it does |
| --- | --- |
| `perdure new <name>` | Scaffold a project (`--clean` for an empty one, `--goal chargebacks` for a plan-goal demo) |
| `perdure init --existing` | Adopt an existing repo: write `Perdurefile`, `PERDURE_AGENT.md`, `AGENTS.md` (if absent), `.perdureignore` (`--force` to overwrite) |
| `perdure goal init <template>` | Write a `Perdurefile` coding goal (e.g. `coding.fix-tests`) |
| `perdure guard begin <Goal>` | Open a coding-agent session over the working tree |
| `perdure guard status` / `context` | Status line, or the agent's operating contract (`--json`); `context --for-agent generic --json` for the full agent packet |
| `perdure guard next` | The single next required action for an agent, with the exact command to run (`--json`) |
| `perdure guard diff` | Changed files since the baseline, classified by `fs.write` scope (`--json`) |
| `perdure guard verify` | Run the goal's required commands for real; set the `verified` bit; `--json` adds repair hints on refusal (`--rerun`) |
| `perdure guard finalize` / `abort` | Finalize verified changes into Perdure's ledger (ledger-only, never git; `commit` is an alias), or cancel the session |
| `perdure guard audit` | Verify a run's ledger is untampered — hash chain, receipt anchoring, and the `verified` bit (`--json`; exits non-zero if forged) |
| `perdure serve-mcp` | Serve the safe guard/goal operations to an external agent over MCP (stdio, server-only) |
| `perdure check [file]` | Type- and effect-check; `--json` for the machine view |
| `perdure run [file]` | Run the project's `main` |
| `perdure test [filter]` | Run tests (blocked while the project has errors) |
| `perdure fix` | Run the agentic repair loop to green (`--strategy`, `--dry-run`, `--coder fixture`) |
| `perdure fmt [file]` | Format to the one canonical style (`--check` for CI) |
| `perdure race` | Race repair strategies in isolation; `--apply` the winner |
| `perdure trace` | Show the last fix/race run (`--json`) |
| `perdure replay` | Re-run the last loop and prove it reproduces |
| `perdure bench` | Report agent-loop metrics (time-to-green, laps, …); `--suite <dir>` over a corpus |
| `perdure audit [file]` | Show every function's effect surface |
| `perdure goal run <name>` | Start a durable run (`--crash-after step:N`, `--strategy`, `--dry-run`) |
| `perdure goal check <name>` | Statically validate a goal's plan before running it (`--json`) |
| `perdure goal list` | List runs in the store |
| `perdure goal inspect <id>` | Show a run's state and event history (`--json`) |
| `perdure goal resume <id>` | Resume a crashed/incomplete run from its last checkpoint |
| `perdure goal replay <id>` | Re-run from base and prove it reproduces |
| `perdure goal cancel <id>` | Cancel a run |
| `perdure goal approvals <id>` | List a run's approval gates |
| `perdure goal approve <id> <apr>` | Grant a pending approval (`--note`) |
| `perdure goal deny <id> <apr>` | Deny a pending approval (`--reason`) |
| `perdure goal receipts <id>` | List a run's effect receipts |
| `perdure goal receipt <id> <rcpt>` | Show one receipt in full (`--json`) |
| `perdure doctor` | Hermetic health check of the toolchain + workspace |
| `perdure explain <code>` | Long-form explanation of a diagnostic code |
| `perdure schema [name]` | Print a versioned JSON schema for any machine output — `diagnostic`, `patch`, `event`, `goal`, `run`, `approval`, `receipt`, `bench`, `test`, and the guard/agent packets `guard-context`, `guard-status`, `guard-diff`, `guard-verify`, `guard-commit`, `guard-audit`, `guard-next`, `agent-context` |

## A taste of the language

Perdure is deliberately boring for humans and easy for models: one formatter's worth of
style, explicit effects, explicit `?`, no macro magic.

```perdure
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

```perdure
type Parity = Even | Odd

fn describe(p: Parity) -> String {
  return match p {
    Even => "even"
    Odd => "odd"
  }
}
```

There's **one formatter**: `perdure fmt` renders any file to a single canonical, idempotent
style, and `perdure fmt --check` gates it in CI.

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
  trace · render · cli       persistence, pretty output, the `perdure` binary
```

The design notes are in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## What's real, and what's next

**Real today:** the language runs — records, `Result`, effects, and user-defined sum
types with `match`; the checker finds effect, type, import, field, and exhaustiveness
bugs and emits machine-applicable patches (including did-you-mean renames and an
insert-the-missing-arm fix for non-exhaustive matches); the typed-patch pipeline enforces
scope, effects, and regressions; the deterministic loop drives the demo to green;
`perdure bench --suite corpus` benchmarks the loop over a suite of broken projects, one per
diagnostic family. **The goal runtime is real:** `goal` is a first-class language
construct with `budget`/`allow`/`require` blocks; `perdure goal run` drives the loop under
those constraints, checkpointing after every step into a durable store with an append-only
`perdure.event.v0` event history; `perdure goal resume` recovers a crashed run from its last
checkpoint **without repeating work**; `perdure goal replay` proves a run reproduces; and the
`allow` block is enforced as real authority by the verification pipeline. **The action layer
is real too:** built-in business goals (`perdure goal run ResolveDuplicateCharge`) run a fixed
plan that proposes effectful actions, pauses for human approval (`perdure goal approvals` /
`approve` / `deny`), calls offline **fake tools**, and records a durable **receipt** for every
effect (`perdure goal receipts` / `receipt`). Idempotency keys make it survive crash/resume with
*no duplicate side effect*, and `perdure goal replay` reproduces the effects from the recorded
approvals. **The plan language is real:** a `plan { ... }` block (`let`/`call`/`approve`/`if`/
`for`/`while`) drives built-in goals like `ReconcileChargebacks` (a `for` loop with a per-duplicate
approval gate) and `RetryFlakyDeploy` (a `while` retry loop). Its **durable re-execution interpreter**
re-walks the plan on every run and resume and memoizes completed calls by receipt, so loops and
crashes still produce each effect exactly once, and `perdure goal replay` reproduces the run. Durable
writes are atomic **and** durable — staged to a `.tmp`, `fsync`'d, `rename`d, and the directory
`fsync`'d — so a crash mid-write can't strand a half-written receipt that a resume would read as
"not yet done," and the guarantee holds across a power loss, not just a clean process exit. The
event log is a SHA-256 hash chain (`perdure.event.v1`), so a tampered or forged history is detectable
(`perdure guard audit`); content hashing in the scope gate is cryptographic for the same reason.
**The coding harness is real:** `perdure init --existing` adopts a real Cargo/npm/Bun/Go/pytest repo
(writing `Perdurefile` / `PERDURE_AGENT.md` / `AGENTS.md` / `.perdureignore` and detecting the test command);
`perdure guard begin` / `verify` / `finalize` scopes an external agent's edits against `fs.write`, runs
the real test command into receipts, and finalizes **ledger-only (never git)**; the snapshot gate
tracks dot-directories and file metadata (exec bit, symlink targets). On top of it the **agent
interface** is a stable structured front door — `perdure guard next`, `perdure guard context --for-agent
generic`, machine-actionable repair hints on a refused `verify`, and a **server-only `perdure
serve-mcp`** (no raw shell, no arbitrary file writes; Perdure embeds no model). There's a pluggable
coder seam (`perdure fix --coder fixture`) whose proposals still go through the exact same pipeline;
`perdure fmt` gives one canonical, idempotent style; `perdure schema` publishes versioned JSON schemas for
every machine output (including `approval`, `receipt`, `guard-audit`, and `agent-context`); and
`perdure doctor` / `perdure explain` round out the toolchain. **160 passing tests** plus end-to-end checks
(red→green, crash→resume→replay, the approval/refund/receipt demo, the loop/approval/crash plan demo,
a user-authored plan goal that resumes off its source snapshot, the coding harness adopting a real
repo and rejecting an out-of-scope edit, power-loss torn-write recovery, ledger tamper-detection, and
the agent-interface + MCP-server surface) and a schema-validation step in CI.

**Near-term follow-ups (the roadmap the runtime is built for):** real tool integrations behind
the fake-tool seam, typed memory lanes with a context-drift detector, a scenario DSL that turns
the shell e2es into Perdure-native long-horizon regression tests, a research ledger
(source/evidence/claim/fact/citation), MCP **client/import** (the server already ships), and a
portable goal ABI. The event log, durable store, authority model, and the approval/receipt
substrate are exactly what those phases hang off. (User-authored plan goals — write a `plan` block
in your own workspace and `run`/`check`/`resume`/`replay` it off a source snapshot — already work.)
Also: multi-file user
imports and comment-preserving formatting.

**Deliberately scoped out:** native/LLVM codegen (today it interprets), a borrow checker,
a package manager, an LSP server, and a *model-backed* coder. The loop already has the
seam — a `Coder` trait, exercised offline by a deterministic fixture coder — so a real
model slots in behind a flag later, with every model output flowing through the same
patch/effect/test/authority pipeline. The core demo and the whole test suite stay
model-free, so everything is fully reproducible offline.

## Testing

```console
$ cargo test                   # unit + integration tests (151)
$ bash scripts/e2e.sh          # new → check → fix → test demo, asserts green
$ bash scripts/goal_e2e.sh     # goal run → crash → resume → replay, asserts no repeated work
$ bash scripts/action_e2e.sh   # approve → crash → resume → replay, asserts exactly one refund
$ bash scripts/plan_e2e.sh     # plan loop → per-duplicate approval → mid-loop crash → exactly-once
$ bash scripts/user_plan_e2e.sh # scaffold → check → crash → snapshot-beats-live-edit → replay
$ bash scripts/guard_e2e.sh    # coding harness: adopt → verify → crash/resume → finalize → out-of-scope reject → replay
```

CI (`.github/workflows/ci.yml`) runs all six on every push, plus `perdure fmt --check` and
JSON-schema validation. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for notes aimed at
automated/cloud agents.

## License

MIT — see [`LICENSE`](LICENSE).
