# The Tach language (v0)

Tach is intentionally small and unambiguous. The guiding rule: **there should be one
obvious way to write a thing.** Fewer valid spellings of the same idea means an agent
editing Tach has fewer ways to produce something that parses but means the wrong thing.

## Lexical structure

- **Comments:** `// to end of line`.
- **Statements** are separated by newlines. Newlines inside `(...)` and `[...]` are
  ignored, so call arguments and `effects [...]` clauses can wrap across lines. There are
  no semicolons.
- **Identifiers:** `[A-Za-z_][A-Za-z0-9_]*`.
- **Literals:** integers (`42`), floats (`3.14`), strings (`"hi\n"`), `true` / `false`.

## Items

A file is a sequence of items.

### `import`

```tach
import db
```

Brings a builtin module into scope. Using a module without importing it is an error
(`E0322`) with a patch that inserts the missing `import`.

### `type`

```tach
type Session = {
  token: String
  user_id: Int
  expires_at: Int
}
```

Record types. Field types may themselves be records, named types, or generics like
`Result<T, E>`.

A `type` can also declare a **sum type** — a set of payload-less variants joined by `|`:

```tach
type Parity = Even | Odd
```

Each variant is a value of that type (`Even`, `Odd`), variants compare with `==`, and a
`match` (see below) selects on them. A field can hold an enum just like any named type.

### `fn`

```tach
fn load_session(token: String) -> Result<Session, AuthError>
  effects [db.read, time.read]
{
  ...
}
```

- Parameters are `name: Type`.
- The return type follows `->` and is optional.
- The optional `effects [...]` clause lists the effects the function performs. The checker
  verifies this against what the body actually does (see **Effects**).

### `test`

```tach
test "expired session is rejected" {
  db.seed("old", { token: "old", user_id: 7, expires_at: 1 })
  ensure load_session("old").is_err()
}
```

A named block run by `tach test`. A test passes if it completes without a failing
`ensure` or an uncaught error.

## Statements

| Statement | Meaning |
| --- | --- |
| `let x = expr` / `let x: Type = expr` | bind a local |
| `return expr` | return a value from the function |
| `ensure cond` | precondition; in a `Result` function a failure becomes `Err(...)`, in a test it fails the test |
| `ensure cond else err` | on failure, `return Err(err)` |
| `if cond { … } else { … }` | conditional |
| `expr` | expression statement |

## Expressions

Literals, identifiers, records (`Name { field: value, … }`), `Ok(x)`, `Err(x)`, field
access (`r.field`), calls (`f(a, b)`), method calls (`r.method(a)`), and the operators:

```
||  &&  == != < <= > >=  + -  * /   unary ! -   postfix ?
```

The `?` operator unwraps `Ok(v)` to `v` and short-circuits an `Err` out of the current
function — the same propagation you know from Rust.

### `match`

A `match` selects an arm by the scrutinee's variant; `_` is a catch-all. Each arm is
`pattern => expression`, and the `match` evaluates to the chosen arm's value:

```tach
fn describe(p: Parity) -> String {
  return match p {
    Even => "even"
    Odd => "odd"
  }
}
```

A `match` on an enum must cover every variant (or use `_`); the checker offers a patch to
add any missing arm. This one patch is special: its arm body is a **placeholder** (a copy
of the first arm's expression — the only generically type-correct value available), so it
makes the match compile and exhaustive but does *not* synthesize the right behavior for the
new variant. The diagnostic flags it as a placeholder to replace, and the verify pipeline is
a backstop — a placeholder that regresses a test is rejected, so the repair loop never greens
on a wrong body a test actually covers. (Every *other* mechanical patch is semantically
complete; this is the lone exception, and it is labeled as such.) Naming a variant the enum
doesn't declare is an error with a did-you-mean suggestion.

## Builtin modules and effects

Calling a builtin module member performs an **effect**, which the enclosing function must
declare.

| Call | Effect | Notes |
| --- | --- | --- |
| `db.query(sql, key)` | `db.read` | returns `Ok(row)` or `Err` |
| `db.seed(key, row)` | `db.write` | seeds the in-memory store (used by tests) |
| `db.exec(...)` | `db.write` | |
| `time.now()` | `time.read` | a fixed clock, for deterministic runs |
| `log.info(msg)` / `log.warn(msg)` | `log.write` | |
| `net.get(url)` / `net.post(url, body)` | `net.read` / `net.write` | disabled in the sandbox |
| `math.abs/max/min(...)` | — | pure |

Value methods that don't need an import: `.is_ok()`, `.is_err()`, `.unwrap()`,
`.unwrap_err()`. The free function `to_string(x)` converts a value to a `String`.

## Determinism

The interpreter has a fixed clock, no randomness, and no real I/O. Every run is
reproducible — which is what lets `tach replay` re-execute a recorded agent loop and get
byte-identical results, and what makes the agent-loop metrics trustworthy.

## Goals

A `goal` is a top-level declaration that turns the repair loop into a durable,
authority-scoped run. It is declarative: it states the budget a run may spend, the
authority it is granted, and the conditions required for success. The runtime
(`tach goal run`) supplies the loop.

```tach
goal FixFailingTests -> Success {
  budget {
    steps: 30        // hard cap on repair steps (enforced)
    retries: 3       // consecutive rejections tolerated before giving up (enforced)
    time: 20m        // recorded; wall-clock budgets are not part of the replayable core
    cost: 0          // recorded
  }
  allow {
    effect db.read              // effects the run may newly perform
    effect db.write
    fs.read "."                 // a single glob, bare
    fs.write ["src/**", "tests/**"]   // or a list
    shell.run ["cargo test", "bun test"]
    tach.check                  // a bare dotted name is a tool grant
  }
  require {
    tests.pass                  // success conditions the runtime can evaluate
    no_new_effects
  }
}
```

The `allow` block is **authority**, not documentation. A patch that would write outside the
`fs.write` globs, or perform an effect the goal never granted, is rejected by the same
verification pipeline that runs the tests — before it touches disk. The success conditions
the runtime understands are `tests.pass`, `no_new_effects`, `no_forbidden_effects`,
`check.clean`, and — for the coding harness — `no_out_of_scope_writes` and the parameterized
`command("cargo test").passes` (each `shell.run` command a run must prove). Naming any other
condition is a warning, since the goal could never be satisfied. A goal with no `steps` or
`retries` budget is also flagged — long-horizon runs must be bounded.

The same grammar drives the **coding harness** over an existing repo: a `Tachfile` at the
repo root holds a goal whose `shell.run` is a real command and whose `require` is
`command("…").passes`. `tach init --existing` writes one, and `tach guard begin/verify/commit`
runs it against the live working tree — scoping edits, executing the command for real, and
proving the result with a receipt. See the README's coding-harness section.

Runs are durable: every step appends an immutable event to
`.tach/goals/<run_id>/events.jsonl` and writes a checkpoint, so `tach goal resume` can
recover a crashed run from exactly where it stopped without repeating work, and
`tach goal replay` reproduces it byte-for-byte. See the architecture notes for the full
model.

### Plan goals (workflows)

A goal may carry a `plan { ... }` block instead of leaning on the repair loop. A plan is a small
**workflow language** — a goal that *acts*. It calls offline, deterministic tools, pauses at
human approval gates, and branches and loops over what those tools return. It is the general form
of the action layer.

```tach
goal ReconcileChargebacks -> Success {
  budget { steps: 60 }                       // budget bounds the number of tool calls
  allow {
    fake.stripe.list_disputes                // every tool a `call` uses must be granted
    fake.stripe.refund
    fake.email.send
    fake.zendesk.comment
  }
  plan {
    let disputes = call fake.stripe.list_disputes { customer: "cus_42" }
    for charge in disputes.charges {         // loop over a tool's output list
      if charge.is_duplicate {               // branch on a field of the output
        approve "refund the duplicate charge" {   // pause for a human; body runs once granted
          let refund = call fake.stripe.refund {
            charge_id: charge.charge_id
            amount_cents: charge.amount_cents
          }
          call fake.email.send { to: "billing@acme.test", charge_id: charge.charge_id }
        }
      } else {
        call fake.zendesk.comment { ticket_id: "zd_dispute", body: "Not a duplicate." }
      }
    }
  }
}
```

Plan statements:

| Form | Meaning |
| --- | --- |
| `let x = <expr>` | bind (or rebind) a name in the plan's single, flat scope |
| `let x = call <tool> { k: v, … }` | call a tool and bind its output (a JSON value) |
| `call <tool> { k: v, … }` | call a tool, discarding the output |
| `approve "summary" { … }` | human approval gate — the body runs only once `tach goal approve` grants it |
| `if <cond> { … } else { … }` | branch on a boolean (`else` optional; no truthiness coercion) |
| `for <x> in <list-expr> { … }` | iterate a JSON array (typically a tool's output) |
| `while <cond> { … }` | repeat while a boolean holds (bounded by the budget) |

Expressions reuse the ordinary grammar — literals, identifiers, field access (`charge.amount_cents`),
arithmetic, comparisons, and `&&`/`||`/`!` — evaluated in JSON-value space (the type tools speak).
`call`/`approve`/`for`/`while`/`in` are contextual keywords, so don't name a variable after them.

Every tool `call` produces a durable **receipt**, and the plan runs by **re-execution**: a run
and a resume both walk the plan from the top, and a call whose receipt already exists returns its
recorded output without invoking the tool again. That one rule is what makes loops crash-safe. A
refund inside a loop, crashed the instant after it commits, is replayed for free on the next
resume and never issued twice. Drive the gates with `tach goal approvals`/`approve`/`deny`, list
the effects with `tach goal receipts`, and prove a run reproduces with `tach goal replay`. Two
plan goals ship built-in: `ReconcileChargebacks` (a `for` loop with a per-duplicate refund gate)
and `RetryFlakyDeploy` (a `while` retry loop). See the architecture notes for the durable
interpreter and its exactly-once invariant.

You can also author a plan goal in your own workspace and run it the same way — nothing about it
is built-in. `tach new demo --goal chargebacks` scaffolds a `goal.tach` you own, and
`tach goal check <name>` validates the plan before you run it: it reports a `call` to a tool the
`allow` block doesn't grant (`E0433`), an unknown tool (`E0434`, with a did-you-mean), a variable
that is never bound (`E0435`), an expression form a plan can't evaluate (`E0436`), and a `while`
loop that makes no tool call and so can only spin against the iteration limit (`E0437`). When a
run starts, the goal's source is snapshotted into the run record; resume and replay re-parse that
frozen snapshot, never the live file, so editing the source mid-run can't change a run in flight.

## Formatting

There is **one formatter**. `tach fmt` renders any file to a single canonical style
(2-space indent, multi-line records and `match` arms, canonical spacing); it is
deterministic and idempotent, and only parenthesizes a subexpression when dropping the
parens would change the parse. `tach fmt --check` writes nothing and exits non-zero if
anything would change — the CI gate. The formatter never reformats a file it can't render
losslessly: files with syntax errors or comments are left untouched (comment-preserving
formatting is a planned follow-up).

## Diagnostics you'll meet

| Code | Kind | Fix it offers |
| --- | --- | --- |
| `E0001` | `syntax` (lexer) | — (e.g. an unterminated string; no autofix) |
| `E0002` | `number_out_of_range` (lexer) | — (a literal that overflows `Int`/`Float`; reported, never silently truncated to `0`) |
| `E0322` | `unknown_module` | insert the missing `import` |
| `E0421` | `effect_undeclared` | add/extend the `effects [...]` clause |
| `E0450` | `effect_unused` (warning) | trim effects the function doesn't perform |
| `E0309` | `type_mismatch` | correct the annotation, or convert the value |
| `E0330` | `unknown_field` | rename to the nearest field (did-you-mean) |
| `E0340` | `non_exhaustive_match` | insert an arm for each missing variant |
| `E0341` | `unknown_variant` | rename to the nearest variant (did-you-mean) |
| `E0460` | `unused_import` (warning) | remove the unused `import` line |
| `E0461` | `unused_variable` (warning) | prefix the binding with `_` |
| `E0431` | `unknown_require_condition` (warning) | name a condition the runtime can check |
| `E0432` | `goal_unbounded` (warning) | add a `budget { steps: N }` block |
| `E0433` | `tool_ungranted` | add the tool to the goal's `allow` block |
| `E0434` | `unknown_tool` | rename to the nearest known tool (did-you-mean) |
| `E0435` | `unbound_plan_var` | bind the variable (a `let` or a `for`) before using it |
| `E0436` | `unsupported_plan_expr` | use a form a plan expression can evaluate |
| `E0437` | `unbounded_plan_loop` (warning) | call a tool inside the loop so it makes progress |

The mechanical code diagnostics each carry a `preferred_patch` so `tach fix` can apply them
without guessing. The
did-you-mean diagnostics only suggest a rename when a real name is a small edit away —
they never guess wildly, because an agent would dutifully apply a bad rename.
