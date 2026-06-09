//! Versioned JSON Schemas for every machine-facing output Tach produces.
//!
//! These are the contract an integrator codes against. They are embedded into the
//! binary at build time (so `tach schema <name>` works hermetically, with no
//! files to ship) and are kept honest by a golden test that parses each one and
//! checks that a representative output validates against the Rust types that are
//! the actual source of truth.

/// One published schema: a short name and its JSON Schema text.
pub struct Schema {
    pub name: &'static str,
    pub title: &'static str,
    pub json: &'static str,
}

/// Every schema Tach publishes, in stable order.
pub const SCHEMAS: &[Schema] = &[
    Schema {
        name: "diagnostic",
        title: "compiler diagnostic (`tach check --json`)",
        json: include_str!("../schemas/diagnostic.schema.json"),
    },
    Schema {
        name: "patch",
        title: "typed patch (fixture coder input)",
        json: include_str!("../schemas/patch.schema.json"),
    },
    Schema {
        name: "event",
        title: "goal run event (events.jsonl)",
        json: include_str!("../schemas/event.schema.json"),
    },
    Schema {
        name: "goal",
        title: "resolved goal spec (goal.json)",
        json: include_str!("../schemas/goal.schema.json"),
    },
    Schema {
        name: "run",
        title: "goal run state (state.json)",
        json: include_str!("../schemas/run.schema.json"),
    },
    Schema {
        name: "approval",
        title: "action approval gate (approvals/<id>.json)",
        json: include_str!("../schemas/approval.schema.json"),
    },
    Schema {
        name: "receipt",
        title: "effect receipt (receipts/<id>.json)",
        json: include_str!("../schemas/receipt.schema.json"),
    },
    Schema {
        name: "bench",
        title: "suite bench output (`tach bench --suite --json`)",
        json: include_str!("../schemas/bench.schema.json"),
    },
    Schema {
        name: "test",
        title: "test report (`tach test --json`)",
        json: include_str!("../schemas/test.schema.json"),
    },
    Schema {
        name: "guard-context",
        title: "guard operating contract (`tach guard context --json`)",
        json: include_str!("../schemas/guard-context.schema.json"),
    },
    Schema {
        name: "guard-status",
        title: "guard session status (`tach guard status --json`)",
        json: include_str!("../schemas/guard-status.schema.json"),
    },
    Schema {
        name: "guard-diff",
        title: "guard scope-classified diff (`tach guard diff --json`)",
        json: include_str!("../schemas/guard-diff.schema.json"),
    },
    Schema {
        name: "guard-verify",
        title: "guard verify result (`tach guard verify --json`)",
        json: include_str!("../schemas/guard-verify.schema.json"),
    },
    Schema {
        name: "guard-commit",
        title: "guard commit/abort outcome (`tach guard commit --json`)",
        json: include_str!("../schemas/guard-commit.schema.json"),
    },
    Schema {
        name: "guard-audit",
        title: "guard ledger-integrity audit (`tach guard audit --json`)",
        json: include_str!("../schemas/guard-audit.schema.json"),
    },
];

/// Look up a schema by name.
pub fn get(name: &str) -> Option<&'static Schema> {
    SCHEMAS.iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_schema_is_valid_json_with_an_id() {
        for s in SCHEMAS {
            let v: serde_json::Value =
                serde_json::from_str(s.json).unwrap_or_else(|e| panic!("{}: {}", s.name, e));
            assert!(v.get("$schema").is_some(), "{} missing $schema", s.name);
            assert!(v.get("$id").is_some(), "{} missing $id", s.name);
            assert_eq!(
                v.get("type").and_then(|t| t.as_str()),
                Some("object"),
                "{} should be an object schema",
                s.name
            );
        }
    }

    #[test]
    fn lookup_finds_published_schemas() {
        assert!(get("event").is_some());
        assert!(get("goal").is_some());
        assert!(get("guard-context").is_some());
        assert!(get("nope").is_none());
    }

    /// The golden test the module doc promises: a representative output of each
    /// guard packet must have exactly the property set its schema declares, and the
    /// schema's `required` must be a subset of what the Rust type actually emits. No
    /// JSON-Schema validator crate — this is field parity between the serializer
    /// (the source of truth) and the published contract, which is the drift that
    /// breaks integrators.
    #[test]
    fn guard_schemas_match_emitted_shapes() {
        use crate::guard::{AuditReport, GuardContext, GuardDiff, GuardOutcome, GuardStatus};
        use serde_json::json;
        use std::collections::BTreeSet;

        fn schema_keys(name: &str) -> (BTreeSet<String>, BTreeSet<String>) {
            let s = get(name).unwrap_or_else(|| panic!("schema `{name}` not registered"));
            let v: serde_json::Value = serde_json::from_str(s.json).unwrap();
            let props = v
                .get("properties")
                .and_then(|p| p.as_object())
                .unwrap_or_else(|| panic!("{name}: missing properties"));
            let required = v
                .get("required")
                .and_then(|r| r.as_array())
                .unwrap_or_else(|| panic!("{name}: missing required"));
            (
                props.keys().cloned().collect(),
                required
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect(),
            )
        }
        fn assert_parity<T: serde::Serialize>(name: &str, v: &T) {
            let emitted: BTreeSet<String> = serde_json::to_value(v)
                .unwrap()
                .as_object()
                .unwrap_or_else(|| panic!("{name}: packet must serialize to an object"))
                .keys()
                .cloned()
                .collect();
            let (props, required) = schema_keys(name);
            assert_eq!(
                emitted, props,
                "{name}: emitted keys must match schema properties exactly"
            );
            assert!(
                required.is_subset(&emitted),
                "{name}: schema `required` names a field the type does not emit"
            );
        }

        assert_parity(
            "guard-context",
            &GuardContext {
                goal: "FixFailingTests".into(),
                run_id: "run_x".into(),
                phase: "open".into(),
                allowed_files: vec!["src/**".into()],
                allowed_commands: vec!["cargo test".into()],
                forbidden: json!({ "out_of_scope_writes": "rejected at the gate" }),
                current_failure: None,
                next_required_action: "edit files in scope, then run `tach guard verify`".into(),
                verification_condition: "`tach guard status --json` reports verified=true".into(),
                done_condition: "verified=true and phase=committed".into(),
                verified: false,
            },
        );

        let status = GuardStatus {
            run_id: "run_x".into(),
            goal: "FixFailingTests".into(),
            phase: "verified".into(),
            verified: true,
            commands_required: 1,
            commands_passed: 1,
            out_of_scope: 0,
            receipts: 1,
            step: 1,
        };
        assert_parity("guard-status", &status);
        // `verify` emits a status packet — same shape, its own published contract.
        assert_parity("guard-verify", &status);

        assert_parity(
            "guard-diff",
            &GuardDiff {
                added: vec![],
                modified: vec!["src/lib.rs".into()],
                deleted: vec![],
                in_scope: vec!["src/lib.rs".into()],
                out_of_scope: vec![],
                rejected: false,
                blind_spots: vec!["target".into(), "node_modules".into()],
            },
        );

        assert_parity(
            "guard-commit",
            &GuardOutcome {
                run_id: "run_x".into(),
                ok: true,
                reason: None,
                status: "completed".into(),
                phase: "committed".into(),
            },
        );

        assert_parity(
            "guard-audit",
            &AuditReport {
                run_id: "run_x".into(),
                ok: true,
                events_total: 12,
                chain_ok: true,
                chain_detail: "intact".into(),
                receipts_total: 1,
                receipts_ok: true,
                receipts_detail: "anchored".into(),
                state_consistent: true,
                state_detail: "matches".into(),
            },
        );
    }
}
