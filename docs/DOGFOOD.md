# Dogfood: perdure develops perdure under its own guard

This repo is itself adopted (`perdure init --existing` wrote the `Perdurefile`,
`PERDURE_AGENT.md`, `AGENTS.md`, and `.perdureignore` at the root), and the change that
closed out the 0.2.0-alpha.1 docs pass was driven through a real guard session — run
`run_75b061739868f023`.

## What happened, straight off the ledger

```text
$ perdure guard begin FixFailingTests
  ● guard session run_75b061739868f023 open — goal FixFailingTests

# … edited src/cli.rs (perdure explain coverage for E0438/E0439/E0470–E0473),
#   inside the goal's fs.write ["src/**", "tests/**"] scope …

$ perdure guard verify
  ✗ not verified — 0/1 command(s) passed, 0 out-of-scope
    · cargo test: exit 1
```

That first failure was not noise — **it was the dogfood finding a real bug**. The
guard's sandbox `HOME` (which exists so a child can never read
`~/.cargo/credentials`) also broke rustup's `cargo` shim: with no `$HOME/.rustup`, it
cannot resolve a toolchain, so *every* Rust repo's verify was dead on arrival on a
rustup-managed machine. The artifact captured by the receipt says it plainly:

```text
error: rustup could not choose a version of cargo to run, because one wasn't
specified explicitly, and no default is configured.
```

The fix (in the same session, also in scope): `shell.rs` now passes `RUSTUP_HOME`
through to children — derived from the *real* home when unset, because `~/.rustup`
holds compilers, not credentials — while `CARGO_HOME` deliberately stays under the
sandbox home, which is exactly where credential files live. Plus a verify timeout
generous enough for a cold first build.

```text
$ perdure guard verify
  ✓ verified — 1/1 command(s) passed, no out-of-scope writes
$ perdure guard finalize
  ✓ finalized — run run_75b061739868f023 is completed (Perdure ledger only; git untouched)
$ perdure guard audit run_75b061739868f023
  ● ledger intact — run_75b061739868f023
    ✓ chain     intact — 11 event(s), chain unbroken
    ✓ receipts  2 receipt(s) anchored and untampered
    ✓ verified  recorded verified=true matches the receipts
```

The full hash-chained history, including the failure:

```text
 1 run.started        2 guard.begun       3 fs.snapshotted
 4 shell.executed     5 receipt.created   6 verify.failed     ← the bug, on the record
 7 shell.executed     8 receipt.created   9 verify.passed     ← the fix, receipted
10 guard.committed   11 run.completed
```

Two receipts, one per real `cargo test` execution — the second ran only because the
tree changed (receipts key on the tree digest), and the failed run is permanent,
auditable evidence rather than a swallowed retry.

## Why this is the point

A harness you don't run on yourself is a brochure. This session exercised adoption
(`init --existing` detecting `cargo test`), scope enforcement, real command execution
with artifact capture, receipt reuse semantics, ledger-only finalize, and the audit —
and the very first thing it did was catch a bug that would have hit every Rust adopter
on day one.
