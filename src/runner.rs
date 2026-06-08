use crate::interp::{Env, Interp, Signal};
use crate::program::Program;
use serde::Serialize;

/// The outcome of a single `test` item.
#[derive(Clone, Debug, Serialize)]
pub struct TestOutcome {
    pub name: String,
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The result of running a (filtered) test suite.
#[derive(Clone, Debug, Serialize, Default)]
pub struct TestReport {
    pub outcomes: Vec<TestOutcome>,
    pub passed: usize,
    pub failed: usize,
}

impl TestReport {
    pub fn all_green(&self) -> bool {
        self.failed == 0
    }

    pub fn total(&self) -> usize {
        self.outcomes.len()
    }
}

/// Core test runner: runs every test for which `keep(name)` is true.
///
/// World state (the fake DB, the log) is reset before each test, so tests are
/// hermetic and order-independent.
fn run_core(program: &Program, keep: impl Fn(&str) -> bool) -> TestReport {
    let interp = Interp::new(program);
    let mut report = TestReport::default();
    for t in program.tests() {
        if !keep(&t.name) {
            continue;
        }
        interp.reset_state();
        let mut env = Env::new();
        let outcome = match interp.eval_block(&t.body, &mut env) {
            Ok(_) => TestOutcome {
                name: t.name.clone(),
                passed: true,
                reason: None,
            },
            Err(sig) => TestOutcome {
                name: t.name.clone(),
                passed: false,
                reason: Some(describe_failure(sig)),
            },
        };
        if outcome.passed {
            report.passed += 1;
        } else {
            report.failed += 1;
        }
        report.outcomes.push(outcome);
    }
    report
}

/// Run every test whose name contains `filter` (or all tests if `None`).
pub fn run_tests(program: &Program, filter: Option<&str>) -> TestReport {
    run_core(program, |n| filter.map_or(true, |f| n.contains(f)))
}

/// Run only the named tests — used by the patch pipeline to run just the tests a
/// change can actually affect (impact analysis).
pub fn run_tests_named(
    program: &Program,
    names: &std::collections::BTreeSet<String>,
) -> TestReport {
    run_core(program, |n| names.contains(n))
}

fn describe_failure(sig: Signal) -> String {
    match sig {
        Signal::Ensure(info) => format!("ensure failed: {}", info.text),
        Signal::Error(msg, _) => msg,
        Signal::Propagate(errv) => format!("uncaught {}", errv),
        Signal::Return(_) => "unexpected return outside a function".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SourceFile;

    const CODE: &str = r#"
import db
import time

type Session = { token: String, expires_at: Int }

fn load_session(token: String) -> Result<Session, AuthError> effects [db.read, time.read] {
  let row = db.query("select * from sessions where token = ?", token)?
  ensure row.expires_at > time.now()
  return Ok(Session { token: row.token, expires_at: row.expires_at })
}
"#;

    const TESTS: &str = r#"
import db

test "valid session loads" {
  db.seed("abc", { token: "abc", expires_at: 9999 })
  ensure load_session("abc").is_ok()
}

test "expired session rejected" {
  db.seed("old", { token: "old", expires_at: 1 })
  ensure load_session("old").is_err()
}

test "missing session rejected" {
  ensure load_session("nope").is_err()
}
"#;

    #[test]
    fn runs_demo_logic_green() {
        let (prog, diags) = Program::parse_sources(vec![
            SourceFile::new("auth.tach", CODE),
            SourceFile::new("auth_test.tach", TESTS),
        ]);
        assert!(
            diags.iter().all(|d| !d.is_error()),
            "unexpected parse errors: {:?}",
            diags
        );
        let report = run_tests(&prog, None);
        assert_eq!(report.failed, 0, "failures: {:?}", report.outcomes);
        assert_eq!(report.passed, 3);
    }

    #[test]
    fn filter_selects_subset() {
        let (prog, _) = Program::parse_sources(vec![
            SourceFile::new("auth.tach", CODE),
            SourceFile::new("auth_test.tach", TESTS),
        ]);
        let report = run_tests(&prog, Some("expired"));
        assert_eq!(report.total(), 1);
        assert!(report.all_green());
    }
}
