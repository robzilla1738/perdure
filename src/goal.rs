//! The resolved goal contract.
//!
//! A `GoalDecl` (in the AST) is what the parser produces from source. A
//! `GoalSpec` is its serializable, runtime-facing resolution: the same authority
//! and budget, decoupled from spans and source text so it can be written to
//! `goal.json`, replayed, and one day exported as a portable goal ABI. Keeping
//! this separate from the AST is deliberate — the durable store should not depend
//! on byte offsets that only mean something against a specific source file.

use crate::ast::GoalDecl;
use crate::ast::Item;
use crate::program::Program;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Default step budget when a goal omits one (paired with the `goal_unbounded`
/// lint, so this only applies to goals constructed programmatically).
pub const DEFAULT_STEPS: u64 = 32;
/// Default consecutive-rejection budget before a run is declared stuck.
pub const DEFAULT_RETRIES: u64 = 3;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoalSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<String>,
    pub budget: BudgetSpec,
    pub allow: AllowSpec,
    pub require: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct BudgetSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AllowSpec {
    #[serde(default)]
    pub effects: Vec<String>,
    #[serde(default)]
    pub fs_read: Vec<String>,
    #[serde(default)]
    pub fs_write: Vec<String>,
    #[serde(default)]
    pub shell: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
}

impl GoalSpec {
    pub fn from_decl(g: &GoalDecl) -> Self {
        GoalSpec {
            name: g.name.clone(),
            success: g.success.clone(),
            budget: BudgetSpec {
                steps: g.budget.steps,
                retries: g.budget.retries,
                time: g.budget.time.clone(),
                cost: g.budget.cost,
            },
            allow: AllowSpec {
                effects: g.allow.effects.iter().map(|e| e.name.clone()).collect(),
                fs_read: g.allow.fs_read.clone(),
                fs_write: g.allow.fs_write.clone(),
                shell: g.allow.shell.clone(),
                tools: g.allow.tools.clone(),
            },
            require: g
                .require
                .conditions
                .iter()
                .map(|c| match &c.arg {
                    // `command("cargo test").passes` serializes to `command:cargo test`
                    // so the runtime-facing `Vec<String>` stays flat (no goal.json
                    // schema change) while preserving the command argument.
                    Some(arg) => format!("command:{arg}"),
                    None => c.name.clone(),
                })
                .collect(),
        }
    }

    pub fn step_budget(&self) -> u64 {
        self.budget.steps.unwrap_or(DEFAULT_STEPS)
    }

    pub fn retry_budget(&self) -> u64 {
        self.budget.retries.unwrap_or(DEFAULT_RETRIES)
    }

    /// The run's wall-clock budget in milliseconds, parsed from
    /// `budget { time: 20m }` (`Ns`/`Nm`/`Nh`). `None` when the goal sets no
    /// time budget or the text does not parse — an unparseable budget must not
    /// silently become an infinite one, but goal *validation* is the checker's
    /// job; the runtime treats it as absent and the recorded text stands.
    ///
    /// Billing is deterministic on resume: the runtime sums `duration_ms`
    /// across existing receipts (durations are receipt evidence, never read
    /// from a live clock), so the same receipts always bill the same.
    pub fn time_budget_ms(&self) -> Option<u64> {
        let text = self.budget.time.as_deref()?.trim();
        let (num, unit_ms) = if let Some(n) = text.strip_suffix('h') {
            (n, 3_600_000)
        } else if let Some(n) = text.strip_suffix('m') {
            (n, 60_000)
        } else if let Some(n) = text.strip_suffix('s') {
            (n, 1_000)
        } else {
            return None;
        };
        num.trim().parse::<u64>().ok()?.checked_mul(unit_ms)
    }

    /// The exact set of effects this goal is authorized to perform. Always a
    /// concrete set — a goal that lists none is granted none, which is the safe
    /// default: a patch that would perform *any* new effect is then rejected.
    pub fn allowed_effects(&self) -> BTreeSet<String> {
        self.allow.effects.iter().cloned().collect()
    }

    /// The file-write authority, or `None` when the goal places no file-scope
    /// restriction (an empty `fs.write`), in which case the patch's own declared
    /// scope is the only gate.
    pub fn allowed_writes(&self) -> Option<Vec<String>> {
        if self.allow.fs_write.is_empty() {
            None
        } else {
            Some(self.allow.fs_write.clone())
        }
    }

    pub fn requires_tests_pass(&self) -> bool {
        self.require.iter().any(|c| c == "tests.pass")
    }

    /// The exact commands a coding goal grants for execution (`allow { shell.run
    /// [...] }`). Unlike file scopes, shell commands are matched by exact string —
    /// a command runs only if it appears here verbatim.
    pub fn allowed_commands(&self) -> &[String] {
        &self.allow.shell
    }

    /// Is `cmd` exactly one of the allowlisted commands? No globbing: `cargo *`
    /// would defeat the point of an allowlist.
    pub fn command_allowed(&self, cmd: &str) -> bool {
        self.allow.shell.iter().any(|c| c == cmd)
    }

    /// The commands a coding goal *requires* to pass, taken from the serialized
    /// `require { command("…").passes }` conditions (stored as `command:<cmd>`).
    pub fn required_commands(&self) -> Vec<String> {
        self.require
            .iter()
            .filter_map(|c| c.strip_prefix("command:").map(|s| s.to_string()))
            .collect()
    }

    /// Does the goal require that no edit fall outside its `fs.write` scope?
    pub fn requires_no_out_of_scope(&self) -> bool {
        self.require.iter().any(|c| c == "no_out_of_scope_writes")
    }

    /// The exact set of tools this goal is authorized to call. Always a concrete
    /// set — a goal that grants none can call none, the safe default. The action
    /// runtime checks every tool call against this before invoking it, so a plan
    /// can only ever do what its `allow` block names: no ambient authority.
    pub fn allowed_tools(&self) -> BTreeSet<String> {
        self.allow.tools.iter().cloned().collect()
    }
}

/// Find a goal declaration by name across a parsed program.
pub fn find_goal<'a>(program: &'a Program, name: &str) -> Option<&'a GoalDecl> {
    for u in &program.units {
        for it in &u.module.items {
            if let Item::Goal(g) = it {
                if g.name == name {
                    return Some(g);
                }
            }
        }
    }
    None
}

/// Every goal declared in a program, in source order.
pub fn all_goals(program: &Program) -> Vec<&GoalDecl> {
    let mut v = Vec::new();
    for u in &program.units {
        for it in &u.module.items {
            if let Item::Goal(g) = it {
                v.push(g);
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_with_time(time: Option<&str>) -> GoalSpec {
        GoalSpec {
            name: "G".into(),
            success: None,
            budget: BudgetSpec {
                time: time.map(|s| s.to_string()),
                ..BudgetSpec::default()
            },
            allow: AllowSpec::default(),
            require: vec![],
        }
    }

    #[test]
    fn time_budget_parses_seconds_minutes_hours() {
        assert_eq!(spec_with_time(Some("45s")).time_budget_ms(), Some(45_000));
        assert_eq!(
            spec_with_time(Some("20m")).time_budget_ms(),
            Some(1_200_000)
        );
        assert_eq!(spec_with_time(Some("2h")).time_budget_ms(), Some(7_200_000));
        assert_eq!(spec_with_time(Some("0s")).time_budget_ms(), Some(0));
    }

    #[test]
    fn absent_or_unparseable_time_budget_is_none() {
        assert_eq!(spec_with_time(None).time_budget_ms(), None);
        assert_eq!(spec_with_time(Some("20")).time_budget_ms(), None);
        assert_eq!(spec_with_time(Some("soon")).time_budget_ms(), None);
        assert_eq!(spec_with_time(Some("m")).time_budget_ms(), None);
    }
}
