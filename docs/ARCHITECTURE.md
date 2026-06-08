# Architecture

Tach's toolchain is one Rust crate (lib + `tach` binary) with sharply separated modules.
The throughline of the design: **a span is both "where the error is" and "where to edit."**
Keeping a single byte-offset coordinate space across diagnostics and patches is what makes
machine repair clean.

## The pipeline

```
source ──▶ lexer ──▶ parser ──▶ AST ──┬──▶ checker ──▶ Diagnostics (+ preferred_patch)
                                       └──▶ interpreter ──▶ values / test results
                                                  ▲
                          Workspace ──▶ Patch ──▶ verify ──▶ commit
                                                  │
                                            agent loop (fix / race)
```

## Modules

| Module | Responsibility |
| --- | --- |
| `span` | byte-range spans; insertion points are zero-width spans |
| `source` | offset → line/column, source slicing |
| `lexer` | tokens with spans; newlines significant except inside `()` / `[]` |
| `ast` | the tree, with **patch-precise** span fields (`brace_offset`, effects-clause spans, return-type span) |
| `parser` | recursive descent + Pratt expressions; a `no_record_lit` flag disambiguates `if cond {` from `Name {` |
| `diagnostics` | the `Diagnostic` type: human message **and** machine fields (`kind`, `repair_strategies`, `preferred_patch`) |
| `types` | the type lattice and lenient structural compatibility (`Unknown` ~ anything) |
| `builtins` | builtin module members, their effects, and effect metadata |
| `check` | effect inference vs. declared effects, type checking, import checking — emits diagnostics with patches |
| `value` / `interp` | deterministic tree-walking interpreter; `?` / `ensure` / `return` modeled as non-local control flow |
| `runner` | the test runner + impact-scoped runs |
| `patch` | `Workspace`, `Edit`, `Patch`, the verify pipeline, call-graph impact analysis, glob scoping |
| `agent` | the `fix` loop, speculative `race`, and the agent-era `Metrics` |
| `trace` | persist/load runs to `.tach/trace.json` |
| `render` / `term` | pretty, colored human output (JSON is the machine path) |
| `project` / `cli` | file discovery, scaffolding, the command dispatcher |

## The verify pipeline (`patch::verify_patch`)

A patch is checked against a base `Workspace` without mutating it:

1. **Scope** — every edit's file must match the patch's `touches` globs.
2. **Apply** to a clone (edits per file applied in descending offset order so they don't
   invalidate each other).
3. **Compile** — the patched workspace must still parse and check.
4. **Effect delta** — the set of effects the program *performs* (inferred from bodies) must
   not gain a new member, unless explicitly allowed. Declaring an effect doesn't count as
   introducing one; adding a `net.post(...)` call does.
5. **API changes** — changed public signatures are reported (blocking only if requested).
6. **Impacted tests** — the call graph determines which tests can be affected; only those
   run, and a test that passed before but fails after is a rejection.

The verdict carries the post-patch workspace, so an accepted patch is committed by simply
swapping it in.

## The agent loop (`agent::fix`)

```
loop:
  diagnostics = parse_errors ++ check(workspace)
  if no errors and tests green: status = green; stop
  pick the earliest diagnostic that carries a preferred_patch
  patch = build a typed patch from it (scoped to its file)
  verdict = verify_patch(workspace, patch)
  if accepted: workspace = verdict.workspace   # advance one lap
  else: status = stuck; stop
```

Re-checking every lap means spans are always fresh, so applying one patch at a time never
trips over offsets shifted by an earlier edit. The loop is deterministic, so `race` can run
strategies on threads and `replay` can reproduce a run from its recorded base files.

## Why an interpreter (for now)

v0 prioritizes the *loop*, not raw runtime speed. A deterministic tree-walker is the fastest
path to a language that genuinely cooperates with agents, and determinism is a feature here
(replayable runs, trustworthy metrics). Native/LLVM codegen is a later concern; the
front-end, checker, and patch pipeline are the parts that carry the thesis and they're all
backend-agnostic.
