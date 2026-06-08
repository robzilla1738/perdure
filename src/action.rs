//! The Action Layer: fake long-horizon *business* goals.
//!
//! The repair runtime (`runtime::drive`) drives a `.tach` source workspace from red
//! to green by proposing typed patches. A business goal is a different shape: it has
//! no source to fix — it runs a fixed **action plan**, where each step calls a tool,
//! some steps pause for human approval, and effectful steps must produce a durable
//! **receipt** so the side effect happens exactly once across crashes and resumes.
//!
//! Two things live here:
//!   * the plan model (`PlannedAction`/`ActionPlan`) and the offline, deterministic
//!     **fake tools** (`invoke_fake_tool`) — no network, no clock, no randomness; and
//!   * a small built-in **catalog** (`builtin_action_goal`) so `tach goal run
//!     ResolveDuplicateCharge` works turn-key. The goal's authority/budget/`require`
//!     is written in the real Tach language (parsed into a `GoalSpec`); only the plan
//!     itself is Rust data, since the workflow syntax for expressing plans in-language
//!     is deliberately not built yet.
//!
//! The plan is *not* persisted into `goal.json`; it is re-derived from this catalog
//! by goal name on resume/replay, exactly as the repair runtime re-derives its repair
//! leaves from the binary. The catalog is part of the binary's identity.

use crate::goal::GoalSpec;
use crate::program::Program;
use crate::source::SourceFile;
use crate::store::short_digest;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One step of an action plan.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannedAction {
    /// Stable within the plan; part of the idempotency key and the approval id.
    pub id: String,
    /// The tool to invoke, e.g. `fake.stripe.refund`. Must be granted by the goal's
    /// `allow` block or the runtime refuses to call it.
    pub tool: String,
    /// Deterministic input to the tool.
    pub input: Value,
    /// Pauses the run for a human `tach goal approve`/`deny` before executing.
    #[serde(default)]
    pub requires_approval: bool,
    /// Performs a side effect → must produce a receipt and is idempotent on resume.
    #[serde(default)]
    pub effectful: bool,
    /// Human-readable intent, shown in `action.proposed`/approval listings.
    #[serde(default)]
    pub summary: String,
}

/// An ordered list of actions — the body of a business goal.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ActionPlan {
    pub steps: Vec<PlannedAction>,
}

/// Invoke a fake tool. **Pure and deterministic**: the result is a function of the
/// input only (ids are short digests of the input), so a replay re-derives the exact
/// same output and there is never a clock or random value to diverge on. An unknown
/// tool is an error — there are no real integrations here.
pub fn invoke_fake_tool(tool: &str, input: &Value) -> Result<Value, String> {
    let get = |k: &str| input.get(k).cloned().unwrap_or(Value::Null);
    match tool {
        "fake.stripe.find_duplicate" => Ok(json!({
            "charge_id": get("charge_id"),
            "duplicate_charge_id": format!("ch_dup_{}", short_digest(input)),
            "amount_cents": input.get("amount_cents").and_then(|v| v.as_i64()).unwrap_or(4200),
            "currency": "usd",
            "is_duplicate": true,
        })),
        "fake.stripe.refund" => Ok(json!({
            "refund_id": format!("re_{}", short_digest(input)),
            "charge_id": get("charge_id"),
            "amount_cents": input.get("amount_cents").and_then(|v| v.as_i64()).unwrap_or(0),
            "status": "succeeded",
        })),
        "fake.email.send" => Ok(json!({
            "message_id": format!("msg_{}", short_digest(input)),
            "to": get("to"),
            "status": "sent",
        })),
        "fake.github.create_pr" => {
            // A small, stable PR number derived from the input digest.
            let d = short_digest(input);
            let n = u64::from_str_radix(&d[..6], 16).unwrap_or(0) % 9000 + 1000;
            Ok(json!({
                "pr_number": n,
                "pr_url": format!("https://github.example/{}/pull/{}",
                    input.get("repo").and_then(|v| v.as_str()).unwrap_or("repo"), n),
                "state": "open",
            }))
        }
        "fake.zendesk.comment" => Ok(json!({
            "comment_id": format!("cmt_{}", short_digest(input)),
            "ticket_id": get("ticket_id"),
            "status": "posted",
        })),
        // ----- tools used by the plan-language showcase goals -----
        // A read that returns a *list*, so a plan can `for charge in
        // disputes.charges { ... }`. The set is fixed and derived from the
        // customer digest — three charges, a mix of duplicates and not — so a
        // loop visibly branches and refunds only the genuine duplicates.
        "fake.stripe.list_disputes" => {
            let d = short_digest(input);
            let tag = &d[..6];
            Ok(json!({
                "customer": get("customer"),
                "charges": [
                    { "charge_id": format!("ch_{tag}_1"), "amount_cents": 4200, "is_duplicate": true },
                    { "charge_id": format!("ch_{tag}_2"), "amount_cents": 1599, "is_duplicate": false },
                    { "charge_id": format!("ch_{tag}_3"), "amount_cents": 8800, "is_duplicate": true },
                ],
            }))
        }
        // A deploy whose success depends on the attempt number, so a `while`
        // retry loop genuinely converges (fails on attempts 1–2, succeeds on 3+)
        // while staying perfectly deterministic — no clock, no randomness.
        "fake.ci.deploy" => {
            let attempt = input.get("attempt").and_then(|v| v.as_i64()).unwrap_or(1);
            Ok(json!({
                "service": get("service"),
                "attempt": attempt,
                "ok": attempt >= 3,
                "build_id": format!("bld_{}", short_digest(input)),
            }))
        }
        other => Err(format!("unknown tool `{other}`")),
    }
}

/// The built-in action goals, by name. Returns the resolved `GoalSpec` (parsed from
/// the in-language goal declaration, so authority/budget/`require` are real) plus the
/// Rust action plan.
pub fn builtin_action_goal(name: &str) -> Option<(GoalSpec, ActionPlan)> {
    let (src, plan) = match name {
        "ResolveDuplicateCharge" => (RESOLVE_DUPLICATE_CHARGE, resolve_duplicate_charge_plan()),
        "ShipHotfixPR" => (SHIP_HOTFIX_PR, ship_hotfix_pr_plan()),
        _ => return None,
    };
    let spec = parse_builtin_goal(name, src)?;
    Some((spec, plan))
}

/// The names of every built-in action goal, in stable order (for `tach goal`).
pub fn builtin_action_goal_names() -> &'static [&'static str] {
    &["ResolveDuplicateCharge", "ShipHotfixPR"]
}

fn parse_builtin_goal(name: &str, src: &str) -> Option<GoalSpec> {
    let (prog, diags) = Program::parse_sources(vec![SourceFile::new("builtin.tach", src)]);
    debug_assert!(
        diags.iter().all(|d| !d.is_error()),
        "built-in goal `{name}` must parse cleanly: {diags:?}"
    );
    crate::goal::find_goal(&prog, name).map(GoalSpec::from_decl)
}

/// The killer demo. A support agent resolves a duplicate Stripe charge: look it up,
/// refund it **behind an approval gate**, tell the customer, and close the ticket.
/// Exactly one refund must ever happen, even across a crash and resume.
const RESOLVE_DUPLICATE_CHARGE: &str = r#"goal ResolveDuplicateCharge -> Success {
  budget {
    steps: 30
  }
  allow {
    fake.stripe.find_duplicate
    fake.stripe.refund
    fake.email.send
    fake.zendesk.comment
  }
  require {
    refund.receipted
  }
}
"#;

fn resolve_duplicate_charge_plan() -> ActionPlan {
    ActionPlan {
        steps: vec![
            PlannedAction {
                id: "lookup".into(),
                tool: "fake.stripe.find_duplicate".into(),
                input: json!({ "charge_id": "ch_1001", "customer": "cus_42", "amount_cents": 4200 }),
                requires_approval: false,
                effectful: false,
                summary: "look up the suspected duplicate charge".into(),
            },
            PlannedAction {
                id: "refund".into(),
                tool: "fake.stripe.refund".into(),
                input: json!({ "charge_id": "ch_1001", "amount_cents": 4200, "reason": "duplicate" }),
                requires_approval: true,
                effectful: true,
                summary: "refund the duplicate $42.00 charge".into(),
            },
            PlannedAction {
                id: "notify".into(),
                tool: "fake.email.send".into(),
                input: json!({ "to": "customer@example.com", "template": "refund_issued", "charge_id": "ch_1001" }),
                requires_approval: false,
                effectful: true,
                summary: "email the customer that the refund was issued".into(),
            },
            PlannedAction {
                id: "close".into(),
                tool: "fake.zendesk.comment".into(),
                input: json!({ "ticket_id": "zd_777", "body": "Duplicate charge refunded.", "public": true }),
                requires_approval: false,
                effectful: true,
                summary: "close the support ticket".into(),
            },
        ],
    }
}

/// A second business goal, to prove the action layer generalizes beyond one scenario:
/// open a hotfix PR behind an approval gate, then note it on the incident ticket.
const SHIP_HOTFIX_PR: &str = r#"goal ShipHotfixPR -> Success {
  budget {
    steps: 20
  }
  allow {
    fake.github.create_pr
    fake.zendesk.comment
  }
  require {
    pr.opened
  }
}
"#;

fn ship_hotfix_pr_plan() -> ActionPlan {
    ActionPlan {
        steps: vec![
            PlannedAction {
                id: "open_pr".into(),
                tool: "fake.github.create_pr".into(),
                input: json!({ "repo": "acme/api", "branch": "hotfix/null-deref", "title": "Fix null deref in /charge" }),
                requires_approval: true,
                effectful: true,
                summary: "open the hotfix pull request".into(),
            },
            PlannedAction {
                id: "comment".into(),
                tool: "fake.zendesk.comment".into(),
                input: json!({ "ticket_id": "zd_900", "body": "Hotfix PR opened.", "public": false }),
                requires_approval: false,
                effectful: true,
                summary: "note the PR on the incident ticket".into(),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_tools_are_deterministic() {
        for tool in [
            "fake.stripe.find_duplicate",
            "fake.stripe.refund",
            "fake.email.send",
            "fake.github.create_pr",
            "fake.zendesk.comment",
        ] {
            let input = json!({ "a": 1, "b": "x", "charge_id": "ch_1", "repo": "acme/api", "ticket_id": "zd_1", "to": "c@x" });
            let a = invoke_fake_tool(tool, &input).expect("tool ok");
            let b = invoke_fake_tool(tool, &input).expect("tool ok");
            assert_eq!(a, b, "{tool} must be deterministic");
        }
    }

    #[test]
    fn unknown_tool_is_an_error() {
        assert!(invoke_fake_tool("fake.unknown", &json!({})).is_err());
    }

    #[test]
    fn builtin_goals_parse_and_grant_their_plans_tools() {
        for name in builtin_action_goal_names() {
            let (spec, plan) = builtin_action_goal(name).expect("catalog entry");
            assert_eq!(&spec.name, name);
            let tools = spec.allowed_tools();
            for step in &plan.steps {
                assert!(
                    tools.contains(&step.tool),
                    "{name}: plan calls `{}` but the goal does not grant it",
                    step.tool
                );
            }
        }
    }

    #[test]
    fn unknown_goal_is_none() {
        assert!(builtin_action_goal("NoSuchGoal").is_none());
    }
}
