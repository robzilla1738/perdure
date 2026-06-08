<div align="center">

# Tach

**The fast language for coding agents.**

*Prompt-to-passing-tests at compiled speed.*

</div>

---

Tach is a small compiled language whose **compiler is also an agent harness**. Most
languages were designed for humans to write and a machine to run. Tach is designed
for the loop that AI coding agents actually live inside:

```
prompt → patch → compile → test → repair → merge
```

The bet is simple: if the compiler emits repairs instead of just complaints, the agent
driving it can be *trivial*. Every Tach diagnostic carries a machine-applicable
`preferred_patch` — a byte-span replacement that fixes the problem. So a repair loop
doesn't need to reason about your code; it reads the fix off the error and applies it
through a pipeline that proves the change is safe before it touches a file.

This repo is a **working v0**: a real lexer, parser, type + effect checker, deterministic
interpreter, test runner, typed-patch pipeline, and the agent loop — all in Rust, with a
single static binary and zero language-model dependency for the core demo.

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
| `tach new <name>` | Scaffold a project (`--clean` for an empty one) |
| `tach check [file]` | Type- and effect-check; `--json` for the machine view |
| `tach run [file]` | Run the project's `main` |
| `tach test [filter]` | Run tests (blocked while the project has errors) |
| `tach fix` | Run the agentic repair loop to green (`--strategy`, `--dry-run`, `--coder fixture`) |
| `tach race` | Race repair strategies in isolation; `--apply` the winner |
| `tach trace` | Show the last fix/race run (`--json`) |
| `tach replay` | Re-run the last loop and prove it reproduces |
| `tach bench` | Report agent-loop metrics (time-to-green, laps, …); `--suite <dir>` over a corpus |
| `tach audit [file]` | Show every function's effect surface |

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

See [`docs/LANGUAGE.md`](docs/LANGUAGE.md) for the full tour.

## How it's built

Single Rust crate, cleanly separated modules:

```
src/
  lexer · parser · ast       front-end (spans double as edit coordinates)
  check                      type + effect analysis → structured diagnostics
  interp · builtins · runner deterministic tree-walking runtime + test runner
  patch                      Workspace, typed patches, the verify pipeline, impact analysis
  agent                      the fix loop, race, metrics
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
diagnostic family. There's a pluggable coder seam (`tach fix --coder fixture`) for the
cases structured repair can't reach — a logic bug — whose proposals still go through the
exact same verification pipeline. 26 passing tests plus an end-to-end check in CI.

**Deliberately scoped out:** native/LLVM codegen (today it interprets), a borrow checker,
a package manager, and a *model-backed* coder. The loop already has the seam — a `Coder`
trait, exercised offline by a deterministic fixture coder — so a real model slots in
behind a flag later. The core demo and the whole test suite stay model-free, so
everything is fully reproducible offline.

## Testing

```console
$ cargo test          # unit + integration tests
$ bash scripts/e2e.sh # full new → check → fix → test demo, asserts green
```

CI (`.github/workflows/ci.yml`) runs both on every push. See
[`CONTRIBUTING.md`](CONTRIBUTING.md) for notes aimed at automated/cloud agents.

## License

MIT — see [`LICENSE`](LICENSE).
