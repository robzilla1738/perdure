# Contributing & testing

## Build

```console
cargo build            # debug
cargo build --release  # single optimized static binary at target/release/tach
```

## Test

```console
cargo test             # 11 unit/integration tests across the front-end, checker,
                       # runtime, patch pipeline, and agent loop
bash scripts/e2e.sh    # full end-to-end: new → check (expect red) → fix → check/test (green)
```

CI runs all of the above on every push — see `.github/workflows/ci.yml`.

## For automated / cloud agents

This repo is friendly to headless verification:

- **No network, no services, no API keys.** The whole demo and test suite run offline and
  deterministically. `time.now()` is a fixed clock; there is no randomness.
- **Single binary.** `cargo build --release` produces `target/release/tach` with no runtime
  dependencies.
- **Machine-readable everywhere.** `tach check --json`, `tach test --json`, `tach trace --json`,
  and `tach audit --json` emit stable JSON. Diagnostics include a `preferred_patch`
  (`file` + `span` + `replacement`) you can apply directly.
- **Deterministic, replayable runs.** `tach fix` writes `.tach/trace.json`; `tach replay`
  re-runs it and asserts byte-identical results. Use this as an oracle.
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
`agent` loop sit on top. Spans are byte offsets so they double as edit coordinates.

## Conventions

- `cargo fmt` before committing; keep modules small and single-purpose.
- New diagnostics should carry a `preferred_patch` whenever a mechanical fix exists — that
  is the core contract of the language.
