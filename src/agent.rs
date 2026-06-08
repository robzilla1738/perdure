//! The agentic repair loop.
//!
//! `fix` is intentionally a *dumb* agent: it reads structured diagnostics, takes
//! the `preferred_patch` each one carries, and runs it through the typed-patch
//! pipeline. It does no language-model reasoning at all. That it nonetheless
//! drives a red project to green is the entire thesis — the intelligence lives in
//! the compiler's output, so the agent can be trivial. A model-backed coder
//! slots into the same loop for the cases structured repair can't reach.

use crate::check::check_program;
use crate::diagnostics::Diagnostic;
use crate::patch::{verify_patch, Edit, Patch, PatchVerdict, VerifyOpts, Workspace};
use crate::runner::run_tests;
use crate::source::SourceFile;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Instant;

/// A repair strategy. The deterministic loop is the same; strategies differ only
/// in *which* valid repair they pick when a diagnostic offers more than one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Smallest correct change — take each diagnostic's preferred patch.
    Minimal,
    /// Prefer value conversions over signature changes.
    Convert,
    /// Fix warnings too, not just errors.
    Strict,
}

impl Strategy {
    pub fn label(&self) -> &'static str {
        match self {
            Strategy::Minimal => "minimal",
            Strategy::Convert => "convert",
            Strategy::Strict => "strict",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiagSummary {
    pub code: String,
    pub kind: String,
    pub message: String,
    pub file: String,
    pub line: usize,
    pub col: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PatchSummary {
    pub name: String,
    pub reason: String,
    pub file: String,
    pub replacement: String,
    pub touches: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerdictSummary {
    pub accepted: bool,
    pub rejections: Vec<String>,
    pub new_effects: Vec<String>,
    pub impacted_tests: Vec<String>,
    pub tests_run: usize,
    pub tests_passed: usize,
    pub tests_failed: usize,
}

/// One iteration ("lap") of the repair loop.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lap {
    pub index: usize,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub targeted: Option<DiagSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<PatchSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<VerdictSummary>,
    pub errors_remaining: usize,
}

/// Agent-era metrics — the benchmark category that actually matters for AI
/// coding: not "how fast did it run" but "how fast did it get to green".
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Metrics {
    pub laps: usize,
    pub patches_applied: usize,
    pub patches_rejected: usize,
    pub tests_run: usize,
    pub regressions: usize,
    pub diff_chars: usize,
    /// Wall-clock to green, in microseconds (the loop is sub-millisecond).
    /// `u64` rather than `u128` so it round-trips through serde's internally
    /// tagged `TraceFile` enum, whose content buffer rejects 128-bit integers.
    pub us: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FixOutcome {
    pub strategy: String,
    /// "green" | "stuck" | "exhausted"
    pub status: String,
    pub laps: Vec<Lap>,
    pub metrics: Metrics,
    pub final_errors: usize,
    pub final_tests_passed: usize,
    pub final_tests_failed: usize,
    pub base_files: BTreeMap<String, String>,
    pub final_files: BTreeMap<String, String>,
}

impl FixOutcome {
    pub fn is_green(&self) -> bool {
        self.status == "green"
    }
}

/// Run the repair loop on a workspace until it is green, stuck, or out of laps.
pub fn fix(base: Workspace, strategy: Strategy, max_laps: usize) -> FixOutcome {
    let start = Instant::now();
    let mut ws = base.clone();
    let mut laps: Vec<Lap> = Vec::new();
    let mut metrics = Metrics::default();
    let mut status = String::from("exhausted");

    for lap_i in 1..=max_laps {
        let (prog, pdiags) = ws.program();
        let mut errors: Vec<Diagnostic> = pdiags.into_iter().filter(|d| d.is_error()).collect();
        let semantic = check_program(&prog);
        let warnings: Vec<Diagnostic> =
            semantic.iter().filter(|d| !d.is_error()).cloned().collect();
        errors.extend(semantic.into_iter().filter(|d| d.is_error()));
        let report = run_tests(&prog, None);
        let error_count = errors.len();

        if error_count == 0 && report.all_green() {
            status = "green".into();
            break;
        }

        // Pick the next actionable diagnostic. Errors first; in strict mode, fall
        // back to warnings once the errors are gone.
        let mut candidates: Vec<&Diagnostic> = errors
            .iter()
            .filter(|d| d.preferred_patch.is_some())
            .collect();
        if candidates.is_empty() && strategy == Strategy::Strict {
            candidates = warnings
                .iter()
                .filter(|d| d.preferred_patch.is_some())
                .collect();
        }
        candidates.sort_by(|a, b| a.file.cmp(&b.file).then(a.span.start.cmp(&b.span.start)));

        let diag = match candidates.first().copied() {
            Some(d) => d,
            None => {
                laps.push(Lap {
                    index: lap_i,
                    action: "no actionable diagnostic — needs a model-backed coder".into(),
                    targeted: None,
                    patch: None,
                    verdict: None,
                    errors_remaining: error_count,
                });
                status = "stuck".into();
                break;
            }
        };

        let patch = build_patch(diag, strategy, &ws);
        let verdict = verify_patch(&ws, &patch, &VerifyOpts::default());

        let dsum = diag_summary(diag, &ws);
        let psum = patch_summary(&patch);
        let vsum = verdict_summary(&verdict);
        metrics.tests_run += verdict.tests_run();
        if verdict.rejections.iter().any(|r| r.contains("regressed")) {
            metrics.regressions += 1;
        }

        if verdict.accepted {
            metrics.patches_applied += 1;
            metrics.diff_chars += patch
                .edits
                .iter()
                .map(|e| e.replacement.len())
                .sum::<usize>();
            ws = verdict.workspace.clone();
            laps.push(Lap {
                index: lap_i,
                action: format!("applied {}", patch.name),
                targeted: Some(dsum),
                patch: Some(psum),
                verdict: Some(vsum),
                errors_remaining: verdict.errors_after,
            });
        } else {
            metrics.patches_rejected += 1;
            laps.push(Lap {
                index: lap_i,
                action: format!("rejected {}", patch.name),
                targeted: Some(dsum),
                patch: Some(psum),
                verdict: Some(vsum),
                errors_remaining: error_count,
            });
            status = "stuck".into();
            break;
        }
    }

    metrics.laps = laps.len();
    metrics.us = start.elapsed().as_micros() as u64;

    // Final accounting.
    let (prog, pdiags) = ws.program();
    let final_errors = pdiags.iter().filter(|d| d.is_error()).count()
        + check_program(&prog).iter().filter(|d| d.is_error()).count();
    let final_report = run_tests(&prog, None);

    FixOutcome {
        strategy: strategy.label().into(),
        status,
        laps,
        metrics,
        final_errors,
        final_tests_passed: final_report.passed,
        final_tests_failed: final_report.failed,
        base_files: base.files.clone(),
        final_files: ws.files.clone(),
    }
}

fn build_patch(diag: &Diagnostic, strategy: Strategy, ws: &Workspace) -> Patch {
    let pp = diag
        .preferred_patch
        .clone()
        .expect("only diagnostics with a preferred_patch are targeted");

    // For a type mismatch, the Convert strategy rewrites the *value* instead of
    // the annotation — a different, valid repair the race can weigh against the
    // minimal one.
    let edit = if diag.kind == "type_mismatch" && strategy == Strategy::Convert {
        let original = ws.slice(&pp.file, diag.span);
        Edit {
            file: pp.file.clone(),
            span: diag.span,
            replacement: format!("to_string({})", original),
        }
    } else {
        Edit {
            file: pp.file.clone(),
            span: pp.span,
            replacement: pp.replacement.clone(),
        }
    };

    Patch {
        name: format!("fix:{}", diag.kind),
        reason: diag.message.clone(),
        touches: vec![pp.file.clone()],
        edits: vec![edit],
        prove: vec!["tach check".into(), "tach test".into()],
    }
}

fn diag_summary(d: &Diagnostic, ws: &Workspace) -> DiagSummary {
    let (line, col) = ws
        .files
        .get(&d.file)
        .map(|t| SourceFile::new(d.file.clone(), t.clone()).line_col(d.span.start))
        .unwrap_or((0, 0));
    DiagSummary {
        code: d.code.clone(),
        kind: d.kind.clone(),
        message: d.message.clone(),
        file: d.file.clone(),
        line,
        col,
    }
}

fn patch_summary(p: &Patch) -> PatchSummary {
    let (file, replacement) = p
        .edits
        .first()
        .map(|e| (e.file.clone(), e.replacement.clone()))
        .unwrap_or_default();
    PatchSummary {
        name: p.name.clone(),
        reason: p.reason.clone(),
        file,
        replacement,
        touches: p.touches.clone(),
    }
}

fn verdict_summary(v: &PatchVerdict) -> VerdictSummary {
    VerdictSummary {
        accepted: v.accepted,
        rejections: v.rejections.clone(),
        new_effects: v.new_effects.clone(),
        impacted_tests: v.impacted_tests.clone(),
        tests_run: v.tests.total(),
        tests_passed: v.tests.passed,
        tests_failed: v.tests.failed,
    }
}

// ----- racing -----

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RaceOutcome {
    pub branches: Vec<FixOutcome>,
    pub winner: Option<usize>,
    pub base_files: BTreeMap<String, String>,
}

/// Run several repair strategies speculatively, each in isolation on its own
/// copy of the workspace, then pick the first verified winner — green, with the
/// smallest diff.
pub fn race(base: Workspace, max_laps: usize) -> RaceOutcome {
    let strategies = [Strategy::Minimal, Strategy::Convert, Strategy::Strict];
    let handles: Vec<_> = strategies
        .into_iter()
        .map(|s| {
            let b = base.clone();
            std::thread::spawn(move || fix(b, s, max_laps))
        })
        .collect();
    let branches: Vec<FixOutcome> = handles
        .into_iter()
        .map(|h| h.join().expect("branch panicked"))
        .collect();

    let winner = branches
        .iter()
        .enumerate()
        .filter(|(_, b)| b.is_green())
        .min_by_key(|(i, b)| (b.metrics.diff_chars, b.metrics.laps, *i))
        .map(|(i, _)| i);

    RaceOutcome {
        branches,
        winner,
        base_files: base.files.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BROKEN_AUTH: &str = r#"
import db
import time

type Session = {
  token: String
  user_id: Int
  expires_at: Int
}

fn load_session(token: String) -> Result<Session, AuthError> {
  let row = db.query("select * from sessions where token = ?", token)?
  ensure row.expires_at > time.now()
  log.info("session loaded")
  return Ok(Session { token: row.token, user_id: row.user_id, expires_at: row.expires_at })
}

fn session_summary(s: Session) -> String {
  return s.user_id
}
"#;

    const TESTS: &str = r#"
import db

test "valid session loads" {
  db.seed("abc", { token: "abc", user_id: 7, expires_at: 9999 })
  ensure load_session("abc").is_ok()
}

test "expired session rejected" {
  db.seed("old", { token: "old", user_id: 7, expires_at: 1 })
  ensure load_session("old").is_err()
}
"#;

    fn broken_ws() -> Workspace {
        let mut w = Workspace::new();
        w.insert("src/auth.tach", BROKEN_AUTH);
        w.insert("tests/auth_test.tach", TESTS);
        w
    }

    #[test]
    fn fix_reaches_green() {
        let out = fix(broken_ws(), Strategy::Minimal, 12);
        assert_eq!(out.status, "green", "laps: {:#?}", out.laps);
        assert_eq!(out.final_errors, 0);
        assert!(out.final_tests_passed >= 2);
        assert_eq!(out.metrics.patches_applied, 3, "expected 3 fixes");
        assert_eq!(out.metrics.regressions, 0);
    }

    #[test]
    fn fix_is_deterministic() {
        let a = fix(broken_ws(), Strategy::Minimal, 12);
        let b = fix(broken_ws(), Strategy::Minimal, 12);
        assert_eq!(a.final_files, b.final_files);
        assert_eq!(a.laps.len(), b.laps.len());
    }

    #[test]
    fn race_prefers_smaller_diff() {
        let out = race(broken_ws(), 12);
        let w = out.winner.expect("a branch should win");
        // minimal (annotation fix) has a smaller diff than convert (to_string).
        assert_eq!(out.branches[w].strategy, "minimal");
        assert!(out.branches.iter().all(|b| b.is_green()));
    }
}
