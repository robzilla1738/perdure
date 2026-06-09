# Perdure threat model

What Perdure enforces, what it only detects, and what it deliberately does not claim.
The honest version, because an authority system that overstates itself is worse than
none.

## The actors

- **The operator** — the human who writes goals, grants approvals, and reads audits.
- **The agent** — whatever drives the work: `perdure fix`'s deterministic loop, a plan
  run, or an external coding agent (Claude Code, Codex, Cursor) operating through
  `perdure guard`. Assumed *fallible and over-eager*, not necessarily malicious — but
  several guarantees below hold even against deliberate misbehavior, and each one says
  which.
- **The world** — processes spawned by `shell.run`, endpoints reached by `http.*`,
  the repository tree a guard session edits.

## Enforced (the action cannot happen)

These are checked *before* the effect fires; failure is an `authority.denied` event on
the ledger, with no receipt — a receipt is proof an effect happened, and nothing did.

- **Shell commands.** A plan `call shell.run { cmd }` runs only if `cmd` appears
  *verbatim* in the goal's `allow { shell.run [...] }` list. No globbing, no prefix
  matching. `cwd` must stay inside the repo. Commands run without a shell (tokenized
  argv), with a scrubbed environment (allowlisted names only), a sandbox `HOME` (so a
  child cannot read `~/.netrc`, `~/.cargo/credentials`, …), and a process-group timeout.
- **HTTP requests.** A URL must match the goal's `http.get`/`http.post` globs
  (default-deny). HTTPS is required unless the matching glob itself grants `http://`.
  Redirects are never followed — a 3xx is returned as data, because auto-following
  would launder an allowed URL into a forbidden one. Userinfo, fragments, and
  non-ASCII hosts are rejected outright.
- **Patch scope and effects.** A typed patch that writes outside `fs.write` globs or
  introduces an ungranted effect is rejected by the verification pipeline before
  touching disk.
- **Budgets.** Step budgets cap tool calls; `budget { time: … }` caps wall-clock for
  plan runs, billed from receipt durations (deterministic on resume — there is no live
  clock in the control flow).
- **Approvals.** A `approve "…" { … }` body cannot execute until the named approval is
  granted; the grant is a durable file, and every receipt records the approval that
  authorized it.

## Guaranteed across crashes (exactly-once, with one honest window)

The receipt write is the commit point: staged to a `.tmp`, `fsync`'d, renamed, directory
`fsync`'d — durable across power loss, not just clean exits. On resume the plan re-walks
from the top and every call whose receipt exists returns the recorded output without
re-invoking. Loops derive their iteration ordinals from walk order, so the memoization
keys are stable.

**The honest window:** if the process crashes *after* a real effect fires but *before*
its receipt commits, a resume will re-attempt it. That window is irreducible without
two-phase commit against the outside world. Mitigations: every `http.post` carries an
`Idempotency-Key` derived from the receipt key (stable across retries), so idempotent
APIs — Stripe-class — deduplicate the retry to true exactly-once; for shell commands,
prefer idempotent commands in `allow` lists.

A *failed* effect is handled asymmetrically on purpose: a process that ran and exited
nonzero (or a request that returned a 500) **is receipted** as `ok:false` output the
plan branches on; only a spawn/transport failure — where the effect never fired — is a
tool error with no receipt, which is exactly the case where re-attempting is safe.

## Secrets

Perdure core stores no API keys and embeds no model. The only sanctioned path for a
credential is `headers_env { authorization: "ENV_VAR_NAME" }`: the value is read from
the environment at call time, attached to the request, and never serialized — events,
receipts, checkpoints, and artifacts carry the env-var *names* that were resolved, never
values. A literal credential header is a check-time error (`E0439 secret_in_source`)
and a runtime refusal (defense in depth). Response `Set-Cookie` headers are stripped
before recording. The e2e suite greps the entire `.perdure/` tree for the live secret
to prove this stays true.

What this does **not** cover: a granted command or endpoint that itself exfiltrates
(`shell.run ["env"]` would print what the scrubbed environment still contains; an
allowed API can be sent anything the plan composes). The allowlist is the operator's
statement of trust — grant narrowly.

## Detected, not prevented (the ledger is evidence, not a wall)

The event log is an append-only SHA-256 hash chain (`perdure.event.v1`); receipts are
anchored to chain events and carry input hashes; `perdure guard audit` re-derives the
chain, the receipt anchoring, and the verified-bit consistency, and exits nonzero on any
inconsistency. **This is tamper-evidence, not tamper-prevention**: the ledger lives in
`.perdure/`, which an agent with filesystem access can write. It cannot forge a
*self-consistent* history without inverting SHA-256, so an audit catches it — but only
an out-of-process authority can prevent it. That is what `perdure serve-mcp` is for: run
the server outside the agent's sandbox and the agent gets the guard/goal operations
without write access to the ledger that judges it.

Similarly, the guard's scope gate is **detect-and-reject at the gate** (verify/finalize
refuse out-of-scope edits), not write prevention — Perdure does not sandbox the agent's
editor.

## Out of scope

- **Sandboxing.** Perdure constrains what *it* executes; it does not contain what a
  granted command does (a granted `cargo test` can run arbitrary build scripts). Pair
  the guard with an OS-level sandbox when the agent is untrusted.
- **Network egress control** beyond the URL allowlist — DNS, IP-level redirection, and
  what an allowed endpoint does with a request are the environment's concern.
- **Model behavior.** Perdure embeds no model and stores no keys; the agent's reasoning
  is outside the trust boundary by design. The whole point is that the ledger, budgets,
  and authority hold *regardless* of what the agent decides to try.
