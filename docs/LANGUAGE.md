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
add any missing arm. Naming a variant the enum doesn't declare is an error with a
did-you-mean suggestion.

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

## Diagnostics you'll meet

| Code | Kind | Fix it offers |
| --- | --- | --- |
| `E0322` | `unknown_module` | insert the missing `import` |
| `E0421` | `effect_undeclared` | add/extend the `effects [...]` clause |
| `E0450` | `effect_unused` (warning) | trim effects the function doesn't perform |
| `E0309` | `type_mismatch` | correct the annotation, or convert the value |
| `E0330` | `unknown_field` | rename to the nearest field (did-you-mean) |
| `E0340` | `non_exhaustive_match` | insert an arm for each missing variant |
| `E0341` | `unknown_variant` | rename to the nearest variant (did-you-mean) |
| `E0460` | `unused_import` (warning) | remove the unused `import` line |
| `E0461` | `unused_variable` (warning) | prefix the binding with `_` |

Each carries a `preferred_patch` so `tach fix` can apply it without guessing. The
did-you-mean diagnostics only suggest a rename when a real name is a small edit away —
they never guess wildly, because an agent would dutifully apply a bad rename.
