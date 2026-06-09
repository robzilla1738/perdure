# Changelog

All notable changes to perdure are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/) with pre-release tags during the alpha.

## [0.2.0-alpha.1] — 2026-06-09

The first public alpha, and the release that renames the project: **tach is
now perdure** (`.pdr` sources, `Perdurefile`, `.perdure/` state directory,
`perdure.*.v1` schemas). PyPI's `tach` is an unrelated Python tool; `perdure`
— to endure permanently — is exactly the pitch.

### Added

- **Real tools in plans.** The plan language is no longer a fake-tool demo:
  - `call shell.run { cmd: "cargo test" }` executes a real process under the
    goal's exact-match command allowlist, with receipts, artifacts, scrubbed
    environment, sandbox `HOME`, and process-group timeouts.
  - `call http.get / http.post { url: ..., body: ..., headers: ...,
    headers_env: ... }` makes real HTTPS calls under URL-glob authority
    (`allow { http.post ["https://api.stripe.com/v1/refunds"] }`),
    default-deny, redirects never followed, plaintext http only when the glob
    explicitly grants it.
  - Secrets never enter the store: `headers_env` names environment variables;
    values are read at call time and never serialized. A literal credential
    header is a check-time error (`E0439 secret_in_source`).
  - Exactly-once: receipts memoize completed calls across crash/resume, and
    every POST carries an `Idempotency-Key` derived from the receipt key.
  - A failing command or non-2xx response is an `ok:false` output the plan
    branches on — only a spawn/transport failure is a tool error (no receipt,
    safe to retry).
  - `authority.denied` ledger events record every refusal, before any I/O.
  - `budget { time: 20m }` is enforced for plan runs, billed deterministically
    from receipt durations.
  - Replay-from-receipts: `goal replay` re-walks the plan feeding recorded
    outputs back in, never re-fires a real effect, stops at a crashed run's
    exact frontier, and verifies the event hash chain.
- **Multi-file imports.** `import "./billing.pdr"` grants visibility into
  another file's functions and types (transitive; workspace-relative;
  cycles, escapes, and missing targets are errors E0470–E0472). A missing
  import is `E0473 symbol_not_imported` with a machine-applicable patch —
  `perdure fix` inserts it.
- **Comment-preserving formatter.** `perdure fmt` now formats files with
  comments instead of refusing them; no comment is ever lost, and formatting
  stays idempotent. Adjacent imports group into one block.
- **Distribution.** Prebuilt binaries (macOS arm64/x86_64, Linux x86_64 musl,
  Windows x86_64) on every tagged release, `install.sh`, and the `perdure`
  crate on crates.io.
- `perdure new <name> --goal shell` — a runnable real-command workflow
  scaffold (allowlist, approval gate, receipts, replay).
- New checker diagnostics: `E0438 command_ungranted`, `E0439
  secret_in_source`, `E0470 import_not_found`, `E0471
  import_outside_workspace`, `E0472 import_cycle`, `E0473
  symbol_not_imported`.

### Fixed

- The guard's sandbox `HOME` broke rustup's `cargo` shim ("no default toolchain"),
  killing verify on every rustup-managed Rust repo. `RUSTUP_HOME` now passes through
  (derived from the real home when unset) — it holds compilers, not credentials —
  while `CARGO_HOME` deliberately stays sandboxed. Found by dogfooding the guard on
  this repo (`docs/DOGFOOD.md`); the failed verify is preserved on that run's ledger.
- The guard verify timeout was 2 minutes — shorter than a cold first build of a real
  project. Now 10 minutes.

### Changed

- Renamed from tach: binary/crate `perdure`, sources `.pdr`, goal file
  `Perdurefile`, state dir `.perdure/`, ignore file `.perdureignore`, agent
  doc `PERDURE_AGENT.md`, MCP server/tools `perdure`/`perdure_guard_*`.
- Schema ids reset under the new namespace: `perdure.event.v1` (hash-chained;
  was `tach.event.v2`), `perdure.agent-context.v1`.
- The dependency posture: still no async runtime, no LLM SDKs, and no network
  code outside `src/http.rs` — which now exists, built on synchronous
  ureq + rustls (no system OpenSSL, fully static musl builds).

### Earlier history

Pre-rename development (the tach prototype: the repair loop, typed patches,
goal runtime, action layer, plan language, guard harness, agent interface,
and the hardening passes) is preserved in git history.
