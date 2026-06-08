# Repair corpus

A suite of small, deliberately-broken Tach projects — one per repairable
diagnostic family — used to benchmark the agent loop over more than the single
`tach new` demo. Run it with:

```
tach bench --suite corpus
```

Each case is a self-contained project (`src/` + `tests/`) that starts red and
must reach green through the deterministic, model-free repair loop. The
benchmark reports the metrics that matter for AI coding — time-to-green,
patches-to-green, tests-run, regressions — per case and in aggregate.

| case                | planted bug            | diagnostic        |
|---------------------|------------------------|-------------------|
| `field_typo`        | misspelled field       | `E0330` unknown_field |
| `missing_import`    | builtin used unimported| `E0322` unknown_module |
| `wrong_return`      | return type mismatch   | `E0309` type_mismatch |
| `undeclared_effect` | effect not declared    | `E0421` effect_undeclared |
| `non_exhaustive`    | match missing a variant| `E0340` non_exhaustive_match |
| `unknown_variant`   | misspelled match arm   | `E0341` unknown_variant |

`corpus_all_reaches_green` (in `src/agent.rs`) guards the suite in CI: every
case must end green with zero regressions, deterministically.
