# Contributing & testing

## Build

```console
cargo build            # debug
cargo build --release  # single optimized static binary at target/release/tach
```

## Test

```console
cargo test                  # 125 unit/integration tests across the front-end, checker, runtime,
                            # patch pipeline, agent loop, goal + action + plan runtime, formatter,
                            # and the coding harness (guard sessions, snapshots/scope gate, real shell)
bash scripts/e2e.sh         # full end-to-end: new → check (expect red) → fix → check/test (green)
bash scripts/goal_e2e.sh    # goal runtime: run → crash → resume → replay, asserts no repeated work
bash scripts/action_e2e.sh  # action layer: approve → crash → resume → replay, asserts one refund
bash scripts/plan_e2e.sh    # plan language: loop → per-duplicate approval → mid-loop crash → exactly-once
bash scripts/user_plan_e2e.sh # user-authored plan goal: scaffold → check → crash → snapshot-beats-live-edit → replay
bash scripts/guard_e2e.sh   # coding harness: adopt → begin → verify → crash/resume → finalize → out-of-scope reject → replay
tach fmt --check            # the project's .tach files are in one canonical style
```

CI runs all of the above on every push — `cargo fmt --check`, `cargo build`, `cargo test`,
all six end-to-end scripts (`e2e.sh`, `goal_e2e.sh`, `action_e2e.sh`, `plan_e2e.sh`, `user_plan_e2e.sh`, `guard_e2e.sh`),
`tach fmt --check` over the corpus, and a JSON-schema validation step. See `.github/workflows/ci.yml`.

## For automated / cloud agents

This repo is friendly to headless verification:

- **No network, no services, no API keys.** The whole demo and test suite run offline and
  deterministically. `time.now()` is a fixed clock; there is no randomness.
- **Single binary.** `cargo build --release` produces `target/release/tach` with no runtime
  dependencies.
- **Machine-readable everywhere.** `tach check --json`, `tach test --json`, `tach trace --json`,
  `tach bench --suite corpus --json`, and `tach audit --json` emit stable JSON. Diagnostics
  include a `preferred_patch` (`file` + `span` + `replacement`) you can apply directly.
- **Deterministic, replayable runs.** `tach fix` writes `.tach/trace.json`; `tach replay`
  re-runs it and asserts byte-identical results. `tach fmt` is idempotent. Use these as oracles.
- **A repair corpus.** `corpus/` holds one broken project per diagnostic family; `tach bench
  --suite corpus` drives them all red → green and reports the agent-era metrics.
- **One-command smoke test:**

  ```console
  bash scripts/e2e.sh && echo OK
  ```

  It scaffolds a fresh project, asserts `tach check` fails with the three planted bugs,
  runs `tach fix`, and asserts the project is then green with passing tests. Exit code 0
  means everything works.

## Code map

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). The short version: the front-end
(`lexer`/`parser`/`ast`) feeds both the `check`er (which produces patch-carrying
diagnostics) and the `interp`reter (which runs code and tests). The `patch` pipeline and
`agent` loop sit on top, the `fmt` module renders the AST back to canonical source, and
spans are byte offsets so they double as edit coordinates. The `runtime`/`store`/`event`
modules make that loop durable (goals, checkpoints, receipts, replay), and the coding
harness (`adopt`/`guard`/`snapshot`/`shell`) turns the same machinery outward onto a real
repo — `shell` is the one audited place Tach spawns a real process.

## Conventions

- `cargo fmt` (Rust) before committing; keep modules small and single-purpose.
- `tach fmt` (Tach) keeps any committed `.tach` files canonical; `tach fmt --check` is a CI
  gate over the corpus.
- New diagnostics should carry a `preferred_patch` whenever a mechanical fix exists — that
  is the core contract of the language. A guessed fix is worse than none: if the repair
  would be a guess, emit the diagnostic without a patch.
- Determinism is sacred — no wall-clock or randomness in anything that feeds a result, a
  trace, or the metrics, so `tach replay` and `tach goal replay` stay byte-exact. Ship
  model/network features behind a flag with the offline path fully covered.
- The durable goal store is append-only and never clobbers history. A *fingerprint*
  (`store::fingerprint`) is a deterministic content hash, not an identity; a fresh run gets
  a unique id from `store::allocate_run`, and `EventLog::create` uses `create_new` so it is
  physically incapable of overwriting an existing log. Anything that records run state must
  preserve those invariants — events are immutable, the working tree is only written on
  `completed`, and the same authority pipeline (`VerifyOpts`) gates every change.
- Every new diagnostic ships a test asserting its exact `preferred_patch`.
