//! The Tach **plan language**: durable, re-executable agentic workflows.
//!
//! A `goal` may carry a `plan { ... }` block — a small, dynamically-typed,
//! JSON-valued workflow language layered on the ordinary Tach expression
//! grammar. Where the linear action layer (`action.rs`) runs a fixed list of
//! steps, a plan has real control flow:
//!
//! ```text
//! plan {
//!   let disputes = call fake.stripe.list_disputes { customer: "cus_42" }
//!   for charge in disputes.charges {
//!     if charge.is_duplicate {
//!       approve "refund the duplicate" {
//!         call fake.stripe.refund { charge_id: charge.charge_id, amount_cents: charge.amount_cents }
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! The runtime ([`crate::runtime`]) drives a plan by **re-executing it from the
//! top on every resume** and *memoizing* each completed tool call by its durable
//! receipt. That single idea is what makes loops and long-horizon flows safe: a
//! call whose receipt already exists returns its recorded output without being
//! invoked again, an un-granted `approve` pauses the walk, and a crash anywhere
//! is recovered by simply walking again. Every side effect therefore happens
//! exactly once, no matter how many times the plan is re-run.
//!
//! This module holds the *pure* pieces — the offline, deterministic expression
//! evaluator and the built-in plan catalog. The durable interpreter that calls
//! tools, writes receipts, and gates on approvals lives in `runtime.rs`, exactly
//! as `action.rs` (pure) pairs with the action driver there.

use crate::ast::{BinOp, Expr, PlanBlock, UnOp};
use crate::goal::GoalSpec;
use crate::program::Program;
use crate::source::SourceFile;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// The plan interpreter's variable scope: `let` bindings, in JSON-value space.
/// A single flat scope (no block scoping) — `let` binds or rebinds a name, and a
/// rebind inside a loop body is visible to the next iteration, which is what
/// makes a `while` counter work.
pub type Env = BTreeMap<String, Value>;

/// Evaluate a plan expression against the current environment, yielding a JSON
/// value. Pure and total over the supported subset; anything outside it (a
/// function call, `match`, `?`, `Ok`/`Err`) is a plan-language error rather than
/// a panic. Tool calls are *not* expressions — they are the `call` statement,
/// handled by the durable interpreter.
pub fn eval_expr(e: &Expr, env: &Env) -> Result<Value, String> {
    match e {
        Expr::Int(n, _) => Ok(json!(n)),
        Expr::Float(f, _) => Ok(json!(f)),
        Expr::Str(s, _) => Ok(json!(s)),
        Expr::Bool(b, _) => Ok(json!(b)),
        Expr::Ident(name, _) => env
            .get(name)
            .cloned()
            .ok_or_else(|| format!("unbound name `{name}`")),
        Expr::Field { recv, name, .. } => {
            let base = eval_expr(recv, env)?;
            match base {
                Value::Object(map) => map
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("no field `{name}` on the value")),
                other => Err(format!(
                    "cannot read field `{name}` of a {} value",
                    json_type(&other)
                )),
            }
        }
        Expr::Unary { op, expr, .. } => {
            let v = eval_expr(expr, env)?;
            match op {
                UnOp::Not => Ok(json!(!as_bool(&v)?)),
                UnOp::Neg => match num(&v)? {
                    Num::Int(i) => i
                        .checked_neg()
                        .map(|n| json!(n))
                        .ok_or_else(|| "integer overflow".into()),
                    Num::Float(f) => Ok(json!(-f)),
                },
            }
        }
        Expr::Binary { op, lhs, rhs, .. } => eval_binary(*op, lhs, rhs, env),
        Expr::Record { fields, .. } => {
            let mut map = serde_json::Map::new();
            for (k, fe) in fields {
                map.insert(k.clone(), eval_expr(fe, env)?);
            }
            Ok(Value::Object(map))
        }
        // Unsupported in plan expressions — orchestration is intentionally tiny.
        Expr::Call { .. } => Err(
            "function calls are not allowed in a plan expression (use `call <tool> { ... }`)"
                .into(),
        ),
        Expr::Method { name, .. } => Err(format!(
            "method `.{name}()` is not supported in a plan expression"
        )),
        Expr::Try { .. } => Err("`?` is not supported in a plan expression".into()),
        Expr::Match { .. } => Err("`match` is not supported in a plan expression".into()),
        Expr::Ok(..) | Expr::Err(..) => {
            Err("`Ok`/`Err` are not supported in a plan expression".into())
        }
    }
}

fn eval_binary(op: BinOp, lhs: &Expr, rhs: &Expr, env: &Env) -> Result<Value, String> {
    // Boolean operators short-circuit, so a guard like `done || call_something`
    // never evaluates the right side once the answer is known.
    match op {
        BinOp::And => {
            let l = as_bool(&eval_expr(lhs, env)?)?;
            if !l {
                return Ok(json!(false));
            }
            return Ok(json!(as_bool(&eval_expr(rhs, env)?)?));
        }
        BinOp::Or => {
            let l = as_bool(&eval_expr(lhs, env)?)?;
            if l {
                return Ok(json!(true));
            }
            return Ok(json!(as_bool(&eval_expr(rhs, env)?)?));
        }
        _ => {}
    }

    let l = eval_expr(lhs, env)?;
    let r = eval_expr(rhs, env)?;
    match op {
        // JSON value equality: values of different types are simply unequal (no
        // coercion), so `5 == "5"` is `false` rather than an error — predictable
        // and safe for branching on tool output.
        BinOp::Eq => Ok(json!(l == r)),
        BinOp::Ne => Ok(json!(l != r)),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let a = as_f64(&l)?;
            let b = as_f64(&r)?;
            Ok(json!(match op {
                BinOp::Lt => a < b,
                BinOp::Le => a <= b,
                BinOp::Gt => a > b,
                BinOp::Ge => a >= b,
                _ => unreachable!(),
            }))
        }
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => arith(op, &l, &r),
        BinOp::And | BinOp::Or => unreachable!("handled above"),
    }
}

fn arith(op: BinOp, l: &Value, r: &Value) -> Result<Value, String> {
    match (num(l)?, num(r)?) {
        (Num::Int(a), Num::Int(b)) => {
            // Checked arithmetic: an overflow is a graceful plan-language error
            // (the run fails), never a panic that would abort the interpreter.
            let v = match op {
                BinOp::Add => a.checked_add(b),
                BinOp::Sub => a.checked_sub(b),
                BinOp::Mul => a.checked_mul(b),
                BinOp::Div => {
                    if b == 0 {
                        return Err("division by zero".into());
                    }
                    a.checked_div(b)
                }
                _ => unreachable!(),
            };
            v.map(|n| json!(n)).ok_or_else(|| "integer overflow".into())
        }
        (a, b) => {
            let (a, b) = (a.to_f64(), b.to_f64());
            Ok(match op {
                BinOp::Add => json!(a + b),
                BinOp::Sub => json!(a - b),
                BinOp::Mul => json!(a * b),
                BinOp::Div => {
                    if b == 0.0 {
                        return Err("division by zero".into());
                    }
                    json!(a / b)
                }
                _ => unreachable!(),
            })
        }
    }
}

enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    fn to_f64(&self) -> f64 {
        match self {
            Num::Int(i) => *i as f64,
            Num::Float(f) => *f,
        }
    }
}

fn num(v: &Value) -> Result<Num, String> {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Num::Int(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Num::Float(f))
            } else {
                Err("not a finite number".into())
            }
        }
        other => Err(format!(
            "expected a number, found a {} value",
            json_type(other)
        )),
    }
}

fn as_f64(v: &Value) -> Result<f64, String> {
    Ok(num(v)?.to_f64())
}

/// Plan conditions must be genuinely boolean — there is no truthiness coercion,
/// so `if charge { }` (a record) is an error, not a silently-true branch.
pub fn as_bool(v: &Value) -> Result<bool, String> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => Err(format!(
            "expected a boolean, found a {} value",
            json_type(other)
        )),
    }
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "record",
    }
}

// ----- the built-in plan catalog -----

/// A built-in plan goal, by name: the resolved [`GoalSpec`] (parsed from the
/// in-language declaration, so authority/budget are real) and the plan body. The
/// plan is re-derived from this catalog on every resume/replay, never persisted
/// — the catalog is part of the binary's identity, like the repair leaves.
pub fn builtin_plan_goal(name: &str) -> Option<(GoalSpec, PlanBlock)> {
    let src = match name {
        "ReconcileChargebacks" => RECONCILE_CHARGEBACKS,
        "RetryFlakyDeploy" => RETRY_FLAKY_DEPLOY,
        _ => return None,
    };
    let (prog, diags) = Program::parse_sources(vec![SourceFile::new("builtin.tach", src)]);
    debug_assert!(
        diags.iter().all(|d| !d.is_error()),
        "built-in plan goal `{name}` must parse cleanly: {diags:?}"
    );
    let decl = crate::goal::find_goal(&prog, name)?;
    let plan = decl.plan.clone()?;
    Some((GoalSpec::from_decl(decl), plan))
}

/// Every built-in plan goal, in stable order (for `tach goal`).
pub fn builtin_plan_goal_names() -> &'static [&'static str] {
    &["ReconcileChargebacks", "RetryFlakyDeploy"]
}

/// A loop + branch showcase: pull the customer's disputed charges, and for each
/// genuine duplicate, refund it **behind a per-iteration approval gate** and tell
/// the customer; for the rest, just note the review. Each refund is its own
/// gate, so the run pauses once per duplicate, and a crash/resume never refunds
/// the same charge twice.
const RECONCILE_CHARGEBACKS: &str = r#"goal ReconcileChargebacks -> Success {
  budget {
    steps: 60
  }
  allow {
    fake.stripe.list_disputes
    fake.stripe.refund
    fake.email.send
    fake.zendesk.comment
  }
  require {
    refunds.receipted
  }
  plan {
    let disputes = call fake.stripe.list_disputes { customer: "cus_42" }
    for charge in disputes.charges {
      if charge.is_duplicate {
        approve "refund the duplicate charge" {
          let refund = call fake.stripe.refund {
            charge_id: charge.charge_id
            amount_cents: charge.amount_cents
            reason: "duplicate"
          }
          call fake.email.send {
            to: "billing@acme.test"
            template: "refund_issued"
            charge_id: charge.charge_id
          }
        }
      } else {
        call fake.zendesk.comment {
          ticket_id: "zd_dispute"
          body: "Reviewed: not a duplicate, no refund."
          public: false
        }
      }
    }
  }
}
"#;

/// A `while` + arithmetic showcase: retry a flaky deploy until it succeeds (it
/// fails on attempts 1–2 and succeeds on the 3rd, deterministically), then
/// announce it behind an approval gate. Each deploy attempt produces its own
/// receipt, so resuming a crashed retry loop never re-runs an attempt.
const RETRY_FLAKY_DEPLOY: &str = r#"goal RetryFlakyDeploy -> Success {
  budget {
    steps: 20
  }
  allow {
    fake.ci.deploy
    fake.zendesk.comment
  }
  require {
    deploy.receipted
  }
  plan {
    let attempt = 1
    let result = call fake.ci.deploy { service: "api", attempt: attempt }
    while !result.ok && attempt < 5 {
      let attempt = attempt + 1
      let result = call fake.ci.deploy { service: "api", attempt: attempt }
    }
    approve "announce the successful deploy" {
      call fake.zendesk.comment {
        ticket_id: "zd_deploy"
        body: "Deploy succeeded after retries."
        public: true
      }
    }
  }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        let mut e = Env::new();
        e.insert("attempt".into(), json!(2));
        e.insert(
            "charge".into(),
            json!({ "charge_id": "ch_9", "amount_cents": 4200, "is_duplicate": true }),
        );
        e.insert("result".into(), json!({ "ok": false }));
        e
    }

    fn ev(src_expr: &str, env: &Env) -> Result<Value, String> {
        // Parse a one-off expression by wrapping it in a tiny plan and pulling
        // the let value out — keeps the test honest about the real grammar.
        let src = format!("goal T {{ plan {{ let x = {src_expr} }} }}\n");
        let (prog, diags) = Program::parse_sources(vec![SourceFile::new("t.tach", &src)]);
        assert!(
            diags.iter().all(|d| !d.is_error()),
            "expr `{src_expr}` parse errors: {diags:?}"
        );
        let g = crate::goal::find_goal(&prog, "T").unwrap();
        let plan = g.plan.as_ref().unwrap();
        match &plan.stmts[0] {
            crate::ast::PlanStmt::Let {
                value: crate::ast::PlanValue::Expr(e),
                ..
            } => eval_expr(e, env),
            other => panic!("expected let-expr, got {other:?}"),
        }
    }

    #[test]
    fn evaluates_literals_idents_and_fields() {
        let env = env();
        assert_eq!(ev("42", &env).unwrap(), json!(42));
        assert_eq!(ev("\"hi\"", &env).unwrap(), json!("hi"));
        assert_eq!(ev("true", &env).unwrap(), json!(true));
        assert_eq!(ev("attempt", &env).unwrap(), json!(2));
        assert_eq!(ev("charge.charge_id", &env).unwrap(), json!("ch_9"));
        assert_eq!(ev("charge.is_duplicate", &env).unwrap(), json!(true));
        assert_eq!(ev("result.ok", &env).unwrap(), json!(false));
    }

    #[test]
    fn evaluates_arithmetic_and_comparison() {
        let env = env();
        assert_eq!(ev("attempt + 1", &env).unwrap(), json!(3));
        assert_eq!(ev("attempt * 10", &env).unwrap(), json!(20));
        assert_eq!(ev("attempt < 5", &env).unwrap(), json!(true));
        assert_eq!(ev("attempt >= 3", &env).unwrap(), json!(false));
        assert_eq!(
            ev("charge.amount_cents == 4200", &env).unwrap(),
            json!(true)
        );
    }

    #[test]
    fn evaluates_boolean_logic_with_short_circuit() {
        let env = env();
        // `!result.ok && attempt < 5` — the RetryFlakyDeploy loop guard.
        assert_eq!(ev("!result.ok && attempt < 5", &env).unwrap(), json!(true));
        // Short-circuit: the right side references an unbound name but is never
        // evaluated because the left side already settles the `&&`/`||`.
        assert_eq!(ev("false && missing", &env).unwrap(), json!(false));
        assert_eq!(ev("true || missing", &env).unwrap(), json!(true));
    }

    #[test]
    fn type_errors_are_reported_not_panics() {
        let env = env();
        assert!(ev("missing", &env).is_err(), "unbound name");
        assert!(ev("charge.nope", &env).is_err(), "missing field");
        assert!(ev("attempt.x", &env).is_err(), "field of a number");
        assert!(as_bool(&json!(1)).is_err(), "no truthiness coercion");
    }

    #[test]
    fn integer_overflow_is_an_error_not_a_panic() {
        let env = env();
        // i64::MAX + 1 must be a graceful plan-language error, never a panic.
        let r = ev("9223372036854775807 + 1", &env);
        assert!(r.is_err(), "overflow should error, got {r:?}");
        // Multiplication overflow too.
        assert!(ev("9223372036854775807 * 2", &env).is_err());
        // Cross-type equality is defined (false), not an error.
        assert_eq!(ev("attempt == \"2\"", &env).unwrap(), json!(false));
    }

    #[test]
    fn builtin_plan_goals_parse_and_carry_a_plan() {
        for name in builtin_plan_goal_names() {
            let (spec, plan) = builtin_plan_goal(name).expect("catalog entry");
            assert_eq!(&spec.name, name);
            assert!(!plan.stmts.is_empty(), "{name} has a non-empty plan");
            assert!(
                !spec.allowed_tools().is_empty(),
                "{name} grants at least one tool"
            );
        }
        assert!(builtin_plan_goal("NoSuchGoal").is_none());
    }
}
