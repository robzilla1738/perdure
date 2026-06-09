//! The coding-agent guard session — Tach's runtime for an externally-edited repo.
//!
//! Unlike the repair loop (which fixes toy source in memory) or the action layer
//! (which calls fake tools), a *guard* session governs a real repo that an
//! external agent (Claude Code, Codex, …) edits with its own tools. Tach does not
//! make the edits; it is the guardrail and the ledger:
//!
//!   begin   snapshot the working tree as a baseline; open the session
//!   diff    classify what changed against the goal's `fs.write` scope (read-only)
//!   verify  reject out-of-scope edits; run the required commands for real, each
//!           captured as a receipt; set the `verified` bit
//!   commit  if verified and unchanged-since-verify, finalize into Tach's ledger
//!   abort   cancel the session
//!
//! Honesty: with no sandbox, Tach cannot *prevent* an out-of-scope write — the
//! agent has already touched the tree by the time `verify` runs. It is a
//! detect-and-reject gate: the violation is recorded and the commit is blocked.
//!
//! The verify step reuses the action layer's correctness spine — a receipt is the
//! commit point and is keyed by an idempotency key, so a crashed-then-re-run
//! `verify` over an unchanged tree reuses the receipt instead of re-running the
//! command. The key folds in a digest of the working tree, so an *edited* tree
//! correctly yields a fresh run rather than a stale reuse.

use crate::event::{kind, read_all, EventLog};
use crate::goal::{self, GoalSpec};
use crate::patch::glob_match;
use crate::project;
use crate::shell::{self, ShellRequest};
use crate::snapshot::{self, Ignore, Manifest};
use crate::store::{self, GoalRecord, Receipt, RunState};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

/// Wall-clock ceiling for a single verified command. Generous enough for a real
/// `cargo test`, short enough that a hung command can't wedge a session.
const VERIFY_TIMEOUT_MS: u64 = 120_000;

/// A test-only crash hook for `verify`, mirroring `runtime::ActionCrash`. Crashes
/// right after the `n`th command receipt is durable but before the `verified` bit
/// is saved — to prove resume reuses the receipt instead of re-running.
#[derive(Clone, Copy, Debug)]
pub enum GuardCrash {
    AfterReceipt(usize),
}

// ----- the JSON packets the CLI surfaces -----

/// The operating-contract packet an agent reads each loop (`guard context`).
#[derive(Serialize)]
pub struct GuardContext {
    pub goal: String,
    pub run_id: String,
    pub phase: String,
    pub allowed_files: Vec<String>,
    pub allowed_commands: Vec<String>,
    pub forbidden: Value,
    pub current_failure: Option<String>,
    pub next_required_action: String,
    /// The single check an agent must use to decide it is finished, stated so the
    /// packet is self-describing: do not claim done until this holds.
    pub done_condition: String,
    pub verified: bool,
}

/// A compact status line (`guard status`).
#[derive(Serialize)]
pub struct GuardStatus {
    pub run_id: String,
    pub goal: String,
    pub phase: String,
    pub verified: bool,
    pub commands_required: usize,
    pub commands_passed: usize,
    pub out_of_scope: usize,
    pub receipts: usize,
    pub step: u64,
}

/// The classified diff (`guard diff`).
#[derive(Serialize)]
pub struct GuardDiff {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    pub in_scope: Vec<String>,
    pub out_of_scope: Vec<String>,
    /// True when at least one changed file is outside the goal's `fs.write` scope.
    pub rejected: bool,
}

/// The outcome of `commit`/`abort`.
#[derive(Serialize)]
pub struct GuardOutcome {
    pub run_id: String,
    pub ok: bool,
    pub reason: Option<String>,
    pub status: String,
    pub phase: String,
}

// ----- agent-interface packets (the structured front door for external agents) -----

/// One out-of-scope write, with the machine-actionable reason and the scope that
/// would have allowed it — the compiler-diagnostic shape applied to a guard refusal.
#[derive(Serialize, Clone)]
pub struct ScopeViolation {
    pub path: String,
    pub reason: String,
    pub allowed: Vec<String>,
}

/// The single best next move for an agent that hit a rejection, named so a caller
/// can act on it without parsing prose.
#[derive(Serialize)]
pub struct PreferredNextAction {
    /// `revert_file` | `edit_then_verify` | `ask_human`
    pub kind: String,
    /// The file to act on, when the action is file-scoped (e.g. `revert_file`).
    pub path: Option<String>,
}

/// Why a `verify` was refused, with repair strategies an agent can choose between.
/// Present only when the run is currently rejected; `None` on a clean/verified run.
#[derive(Serialize)]
pub struct Rejection {
    /// `scope_violation` | `command_failed` | `misconfigured`
    pub kind: String,
    pub message: String,
    /// The offending paths for a `scope_violation` (empty for other kinds).
    pub violations: Vec<ScopeViolation>,
    /// Named strategies, most-preferred first.
    pub repair_strategies: Vec<String>,
    pub preferred_next_action: PreferredNextAction,
}

/// The classified change set, as a nested object inside the agent context.
#[derive(Serialize)]
pub struct ChangedFiles {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    pub in_scope: Vec<String>,
    pub out_of_scope: Vec<String>,
}

/// A receipt distilled to what an agent needs: did the command pass, and where are
/// its captured stdout/stderr artifacts.
#[derive(Serialize)]
pub struct ReceiptSummary {
    pub command: String,
    pub action_id: String,
    pub exit_code: Option<i64>,
    pub timed_out: bool,
    pub duration_ms: Option<u64>,
    pub passed: bool,
    pub stdout_artifact: Option<String>,
    pub stderr_artifact: Option<String>,
}

/// The next-required-action packet (`guard next`): one stable answer to "what do I
/// do now?", so an agent need not infer state from several commands.
#[derive(Serialize)]
pub struct GuardNext {
    pub run_id: String,
    pub goal: String,
    /// An agent-facing status label: `editing` | `verified` | `finalized` |
    /// `aborted` | `blocked`.
    pub status: String,
    /// `edit_then_verify` | `fix_scope_violation` | `run_verify` | `finalize` |
    /// `done` | `aborted` | `blocked`.
    pub next_action: String,
    pub allowed_files: Vec<String>,
    pub allowed_commands: Vec<String>,
    /// The live out-of-scope writes, if any — non-empty means `fix_scope_violation`.
    pub scope_violations: Vec<ScopeViolation>,
    pub done_condition: String,
    /// The exact Tach command to run next (empty when the run is terminal).
    pub recommended_command: String,
    pub instructions: Vec<String>,
}

/// The full agent operating context (`guard context --for-agent generic`): the
/// stable, vendor-neutral packet an external coding agent reads before each edit.
#[derive(Serialize)]
pub struct AgentContext {
    /// Versioned contract tag, so an integrator can pin the shape.
    pub schema: String,
    /// Which agent shape this is rendered for (currently always `generic`).
    pub agent: String,
    pub goal: String,
    pub run_id: String,
    pub status: String,
    pub allowed_files: Vec<String>,
    /// The files you have changed that are outside `allowed_files` — revert these.
    /// A flat companion to `scope_violations` (which carries the reasons).
    pub forbidden_files: Vec<String>,
    pub allowed_commands: Vec<String>,
    pub required_commands: Vec<String>,
    pub changed_files: ChangedFiles,
    pub scope_violations: Vec<ScopeViolation>,
    pub latest_receipts: Vec<ReceiptSummary>,
    pub current_failure: Option<String>,
    pub done_condition: String,
    pub next_action: String,
    pub recommended_command: String,
    pub instructions: Vec<String>,
    pub verified: bool,
}

/// The verify result (`guard verify --json`): the status line plus, on refusal,
/// machine-actionable repair hints and the recommended next move.
#[derive(Serialize)]
pub struct GuardVerify {
    #[serde(flatten)]
    pub status: GuardStatus,
    /// Repair hints when the verify was refused; `null` on a clean verify.
    pub rejection: Option<Rejection>,
    pub next_action: String,
    pub recommended_command: String,
}

// ----- begin -----

/// Open a new guard session over the current working tree. Resolves the goal from
/// the repo's `Tachfile` (by name, or the sole goal if `goal_name` is `None`),
/// snapshots the tree as the baseline, and records the run.
pub fn begin(repo: &Path, goal_name: Option<&str>) -> io::Result<RunState> {
    let (prog, diags) = project::load_goal_file(repo).map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no Tachfile in this repo — run `tach init --existing` first",
        )
    })?;
    if diags.iter().any(|d| d.is_error()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Tachfile has errors — run `tach check` after fixing it",
        ));
    }
    let decl = match goal_name {
        Some(name) => goal::find_goal(&prog, name).ok_or_else(|| {
            let names: Vec<String> = goal::all_goals(&prog)
                .iter()
                .map(|g| g.name.clone())
                .collect();
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no goal `{name}` in Tachfile (available: {})",
                    names.join(", ")
                ),
            )
        })?,
        None => {
            let mut all = goal::all_goals(&prog);
            if all.len() == 1 {
                all.remove(0)
            } else if all.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "Tachfile declares no goal",
                ));
            } else {
                let names: Vec<String> = all.iter().map(|g| g.name.clone()).collect();
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Tachfile has several goals; name one: {}", names.join(", ")),
                ));
            }
        }
    };
    let spec = GoalSpec::from_decl(decl);

    let fingerprint = store::fingerprint(&spec.name, &BTreeMap::new());
    let run_id = store::allocate_run(repo, &fingerprint)?;
    let record = GoalRecord {
        spec: spec.clone(),
        strategy: "coding".into(),
        base_files: BTreeMap::new(),
        kind: "coding".into(),
    };
    store::save_goal(repo, &run_id, &record)?;

    let baseline = snapshot::snapshot(repo, &Ignore::load(repo))?;
    store::save_baseline(repo, &run_id, &baseline)?;

    let mut log = EventLog::create(&store::events_path(repo, &run_id), &run_id)?;
    log.append(
        kind::RUN_STARTED,
        json!({ "goal": spec.name, "kind": "coding", "strategy": "coding" }),
    )?;
    log.append(
        kind::GUARD_BEGUN,
        json!({
            "allowed_files": spec.allow.fs_write,
            "allowed_commands": spec.allow.shell,
            "required_commands": spec.required_commands(),
        }),
    )?;
    log.append(kind::FS_SNAPSHOTTED, json!({ "files": baseline.len() }))?;

    let state = RunState {
        run_id: run_id.clone(),
        goal: spec.name.clone(),
        status: "running".into(),
        kind: "coding".into(),
        guard_phase: "open".into(),
        commands_required: spec.required_commands().len(),
        ..Default::default()
    };
    store::save_state(repo, &state)?;
    Ok(state)
}

// ----- shared session helpers -----

/// Load a coding run's record + state, rejecting a non-coding run id.
fn session(repo: &Path, run_id: &str) -> io::Result<(GoalRecord, RunState)> {
    let record = store::load_goal(repo, run_id)?;
    if record.kind != "coding" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "run `{run_id}` is a `{}` run, not a guard session",
                record.kind
            ),
        ));
    }
    let state = store::load_state(repo, run_id)?;
    Ok((record, state))
}

/// Snapshot the head and classify every changed file against the goal's write
/// scope. Returns the raw diff plus (in_scope, out_of_scope) partitions.
fn scope_diff(
    repo: &Path,
    run_id: &str,
    spec: &GoalSpec,
) -> io::Result<(Manifest, snapshot::Diff, Vec<String>, Vec<String>)> {
    let baseline = store::load_baseline(repo, run_id)?;
    let head = snapshot::snapshot(repo, &Ignore::load(repo))?;
    let diff = snapshot::diff(&baseline, &head);
    let writes = spec.allowed_writes();
    let mut in_scope = Vec::new();
    let mut out_of_scope = Vec::new();
    for path in diff.changed() {
        let ok = match &writes {
            Some(globs) => globs.iter().any(|g| glob_match(g, &path)),
            None => true, // no fs.write restriction declared → nothing is out of scope
        };
        if ok {
            in_scope.push(path);
        } else {
            out_of_scope.push(path);
        }
    }
    Ok((head, diff, in_scope, out_of_scope))
}

/// A stable digest of a tree manifest — folded into the verify idempotency key so
/// reuse is sound only when the tree the command ran against is unchanged. Folds
/// each entry's full signature (content, kind, exec bit, symlink target), so a
/// metadata-only change still mints a fresh run rather than reusing a stale proof.
fn manifest_digest(m: &Manifest) -> String {
    let mut s = String::new();
    for (p, entry) in m {
        s.push_str(p);
        s.push('\0');
        s.push_str(&entry.signature());
        s.push('\n');
    }
    snapshot::content_hash(s.as_bytes())
}

/// The repo-relative, forward-slashed form of an artifact path for a receipt.
fn rel(repo: &Path, p: &Path) -> String {
    p.strip_prefix(repo)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// A receipt is a pass iff its command exited 0 and did not time out.
fn receipt_passed(r: &Receipt) -> bool {
    r.output.get("exit_code").and_then(|v| v.as_i64()) == Some(0)
        && r.output.get("timed_out").and_then(|v| v.as_bool()) != Some(true)
}

// ----- status / context / diff (pure reads) -----

pub fn status(repo: &Path, run_id: &str) -> io::Result<GuardStatus> {
    let (_record, state) = session(repo, run_id)?;
    Ok(GuardStatus {
        run_id: run_id.to_string(),
        goal: state.goal.clone(),
        phase: state.guard_phase.clone(),
        verified: state.verified,
        commands_required: state.commands_required,
        commands_passed: state.commands_passed,
        out_of_scope: state.out_of_scope,
        receipts: store::list_receipts(repo, run_id).len(),
        step: state.step,
    })
}

pub fn context(repo: &Path, run_id: &str) -> io::Result<GuardContext> {
    let (record, state) = session(repo, run_id)?;
    let spec = &record.spec;
    let next = match state.guard_phase.as_str() {
        "verified" => "run `tach guard commit` to finalize the verified changes",
        "committed" => "done — this run is committed",
        "aborted" => "this run was aborted; begin a new one",
        _ => "edit files within allowed_files, then run `tach guard verify`",
    };
    Ok(GuardContext {
        goal: state.goal.clone(),
        run_id: run_id.to_string(),
        phase: state.guard_phase.clone(),
        allowed_files: spec.allow.fs_write.clone(),
        allowed_commands: spec.allow.shell.clone(),
        forbidden: json!({
            "out_of_scope_writes": "edits outside allowed_files are rejected at the gate and block commit",
            "unallowlisted_commands": "only allowed_commands are run by tach",
        }),
        current_failure: last_failure(repo, run_id),
        next_required_action: next.to_string(),
        done_condition: DONE_CONDITION.to_string(),
        verified: state.verified,
    })
}

/// The summary of the most recent `verify.failed` event, if the last verify failed.
fn last_failure(repo: &Path, run_id: &str) -> Option<String> {
    let events = read_all(&store::events_path(repo, run_id)).ok()?;
    events
        .iter()
        .rev()
        .find(|e| e.kind == kind::VERIFY_FAILED || e.kind == kind::VERIFY_PASSED)
        .filter(|e| e.kind == kind::VERIFY_FAILED)
        .and_then(|e| e.payload.get("summary").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

pub fn diff(repo: &Path, run_id: &str) -> io::Result<GuardDiff> {
    let (record, _state) = session(repo, run_id)?;
    let (_head, diff, in_scope, out_of_scope) = scope_diff(repo, run_id, &record.spec)?;
    Ok(GuardDiff {
        added: diff.added,
        modified: diff.modified,
        deleted: diff.deleted,
        rejected: !out_of_scope.is_empty(),
        in_scope,
        out_of_scope,
    })
}

// ----- agent interface: next / agent-context / verify-report -----

/// The one canonical statement of "you are done", shared by every agent-facing
/// packet so the bar an agent must clear is spelled the same everywhere.
const DONE_CONDITION: &str =
    "`tach guard status --json` reports verified=true, then finalize with `tach guard finalize`";

/// The schema tag stamped on the generic agent context, so an integrator can pin
/// the contract shape independently of the binary version.
const AGENT_CONTEXT_SCHEMA: &str = "tach.agent-context.v1";

/// Everything the agent-facing packets need, computed from one live scope diff so
/// `next`, `context --for-agent`, and `verify --json` all agree on the same view of
/// the tree. Pure read: snapshots and classifies, runs no command.
struct AgentPlan {
    status_label: &'static str,
    next_action: &'static str,
    recommended_command: String,
    instructions: Vec<String>,
    violations: Vec<ScopeViolation>,
    diff: snapshot::Diff,
    in_scope: Vec<String>,
    out_of_scope: Vec<String>,
    current_failure: Option<String>,
    cmd_misconfigured: bool,
}

fn compute_plan(
    repo: &Path,
    run_id: &str,
    spec: &GoalSpec,
    state: &RunState,
) -> io::Result<AgentPlan> {
    let (head, diff, in_scope, out_of_scope) = scope_diff(repo, run_id, spec)?;
    let digest_matches = state.verified && manifest_digest(&head) == state.verified_digest;
    let has_changes =
        !(diff.added.is_empty() && diff.modified.is_empty() && diff.deleted.is_empty());
    let cmd_misconfigured = spec
        .required_commands()
        .iter()
        .any(|c| !spec.command_allowed(c));
    let current_failure = last_failure(repo, run_id);
    // A genuine command failure: a verify ran, nothing is out of scope, the command
    // is allowlisted, and the recorded failure was not the scope gate.
    let command_failing = !state.verified
        && out_of_scope.is_empty()
        && !cmd_misconfigured
        && state.commands_required > 0
        && state.commands_passed < state.commands_required
        && current_failure
            .as_deref()
            .map(|f| !f.contains("out-of-scope"))
            .unwrap_or(false);
    let (status_label, next_action, recommended_command, instructions) = agent_state(
        &state.guard_phase,
        state.verified,
        digest_matches,
        out_of_scope.len(),
        has_changes,
        cmd_misconfigured,
        command_failing,
    );
    let violations = out_of_scope
        .iter()
        .map(|p| ScopeViolation {
            path: p.clone(),
            reason: "not allowed by fs.write".into(),
            allowed: spec.allow.fs_write.clone(),
        })
        .collect();
    Ok(AgentPlan {
        status_label,
        next_action,
        recommended_command,
        instructions,
        violations,
        diff,
        in_scope,
        out_of_scope,
        current_failure,
        cmd_misconfigured,
    })
}

/// The state machine an external agent follows, derived purely from the live view.
/// Returns `(status_label, next_action, recommended_command, instructions)`. Order
/// matters: terminal states first, then misconfiguration, then scope, then verify.
#[allow(clippy::too_many_arguments)]
fn agent_state(
    phase: &str,
    verified: bool,
    digest_matches: bool,
    n_violations: usize,
    has_changes: bool,
    cmd_misconfigured: bool,
    command_failing: bool,
) -> (&'static str, &'static str, String, Vec<String>) {
    match phase {
        "committed" => (
            "finalized",
            "done",
            String::new(),
            vec!["This run is finalized in Tach's ledger. Begin a new session for more work.".into()],
        ),
        "aborted" => (
            "aborted",
            "aborted",
            String::new(),
            vec!["This run was aborted. Run `tach guard begin <Goal>` to start over.".into()],
        ),
        _ if cmd_misconfigured => (
            "blocked",
            "blocked",
            String::new(),
            vec![
                "A required command is not in the goal's allow.shell list, so Tach cannot run it."
                    .into(),
                "This is a goal-configuration problem: a human must fix the Tachfile.".into(),
            ],
        ),
        _ if n_violations > 0 => (
            "blocked",
            "fix_scope_violation",
            "tach guard diff --json".into(),
            vec![
                "You changed files outside allowed_files. Revert them, or move the change into scope."
                    .into(),
                "Run `tach guard diff --json` to see exactly which files are out of scope.".into(),
                "Out-of-scope writes block verify and finalize until reverted.".into(),
            ],
        ),
        _ if verified && digest_matches => (
            "verified",
            "finalize",
            "tach guard finalize".into(),
            vec![
                "All required commands passed and no out-of-scope files changed.".into(),
                "Run `tach guard finalize` to record this run in Tach's ledger (this never touches git)."
                    .into(),
            ],
        ),
        _ if verified && !digest_matches => (
            "editing",
            "run_verify",
            "tach guard verify".into(),
            vec![
                "The tree changed since the last verify. Run `tach guard verify` again before finalizing."
                    .into(),
            ],
        ),
        _ if command_failing => (
            "editing",
            "edit_then_verify",
            "tach guard verify".into(),
            vec![
                "A required command is still failing. Edit files within allowed_files to fix it."
                    .into(),
                "Then run `tach guard verify`; do not claim done until verified=true.".into(),
            ],
        ),
        _ if has_changes => (
            "editing",
            "run_verify",
            "tach guard verify".into(),
            vec![
                "You have in-scope changes that are not yet verified.".into(),
                "Run `tach guard verify` to run the required commands and set the verified bit.".into(),
            ],
        ),
        _ => (
            "editing",
            "edit_then_verify",
            "tach guard verify".into(),
            vec![
                "Edit only files matching allowed_files.".into(),
                "After editing, run `tach guard verify`. Do not claim done until verified=true.".into(),
            ],
        ),
    }
}

/// Build the rejection hints for a verify, reusing the plan's live view (no second
/// snapshot). `None` when the run is clean, terminal, or verified.
fn rejection_from_plan(spec: &GoalSpec, state: &RunState, plan: &AgentPlan) -> Option<Rejection> {
    if matches!(state.guard_phase.as_str(), "committed" | "aborted") {
        return None;
    }
    if !plan.out_of_scope.is_empty() {
        return Some(Rejection {
            kind: "scope_violation".into(),
            message: format!(
                "{} changed file(s) outside the allowed write scope.",
                plan.out_of_scope.len()
            ),
            violations: plan.violations.clone(),
            repair_strategies: vec![
                "revert_out_of_scope_file".into(),
                "move_change_into_allowed_scope".into(),
                "ask_human_to_expand_goal_authority".into(),
            ],
            preferred_next_action: PreferredNextAction {
                kind: "revert_file".into(),
                path: plan.out_of_scope.first().cloned(),
            },
        });
    }
    if state.verified {
        return None;
    }
    if plan.cmd_misconfigured {
        return Some(Rejection {
            kind: "misconfigured".into(),
            message: "A required command is not allowlisted in the goal's allow.shell.".into(),
            violations: vec![],
            repair_strategies: vec![
                "add_command_to_allow_shell".into(),
                "ask_human_to_fix_tachfile".into(),
            ],
            preferred_next_action: PreferredNextAction {
                kind: "ask_human".into(),
                path: None,
            },
        });
    }
    // A genuine command failure (a verify ran and a command did not pass).
    if state.commands_required > 0 && state.commands_passed < state.commands_required {
        let _ = spec; // scope/command authority already reflected in the plan
        return Some(Rejection {
            kind: "command_failed".into(),
            message: plan
                .current_failure
                .clone()
                .unwrap_or_else(|| "a required command failed".into()),
            violations: vec![],
            repair_strategies: vec!["edit_files_to_fix_failure".into(), "rerun_verify".into()],
            preferred_next_action: PreferredNextAction {
                kind: "edit_then_verify".into(),
                path: None,
            },
        });
    }
    None
}

/// The latest receipt for each required command, distilled for an agent.
fn receipt_summaries(repo: &Path, run_id: &str, required: &[String]) -> Vec<ReceiptSummary> {
    let receipts = store::list_receipts(repo, run_id);
    required
        .iter()
        .filter_map(|cmd| {
            receipts
                .iter()
                .filter(|r| r.input.get("command").and_then(|v| v.as_str()) == Some(cmd.as_str()))
                .max_by_key(|r| r.step)
                .map(|r| ReceiptSummary {
                    command: cmd.clone(),
                    action_id: r.action_id.clone(),
                    exit_code: r.output.get("exit_code").and_then(|v| v.as_i64()),
                    timed_out: r
                        .output
                        .get("timed_out")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    duration_ms: r.output.get("duration_ms").and_then(|v| v.as_u64()),
                    passed: receipt_passed(r),
                    stdout_artifact: r
                        .output
                        .get("stdout_artifact")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    stderr_artifact: r
                        .output
                        .get("stderr_artifact")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                })
        })
        .collect()
}

/// `guard next` — the single next required action for an external agent.
pub fn next(repo: &Path, run_id: &str) -> io::Result<GuardNext> {
    let (record, state) = session(repo, run_id)?;
    let spec = &record.spec;
    let plan = compute_plan(repo, run_id, spec, &state)?;
    Ok(GuardNext {
        run_id: run_id.to_string(),
        goal: state.goal.clone(),
        status: plan.status_label.to_string(),
        next_action: plan.next_action.to_string(),
        allowed_files: spec.allow.fs_write.clone(),
        allowed_commands: spec.allow.shell.clone(),
        scope_violations: plan.violations.clone(),
        done_condition: DONE_CONDITION.to_string(),
        recommended_command: plan.recommended_command.clone(),
        instructions: plan.instructions.clone(),
    })
}

/// `guard context --for-agent <agent>` — the full generic operating contract.
pub fn agent_context(repo: &Path, run_id: &str, agent: &str) -> io::Result<AgentContext> {
    let (record, state) = session(repo, run_id)?;
    let spec = &record.spec;
    let plan = compute_plan(repo, run_id, spec, &state)?;
    let required = spec.required_commands();
    Ok(AgentContext {
        schema: AGENT_CONTEXT_SCHEMA.to_string(),
        agent: agent.to_string(),
        goal: state.goal.clone(),
        run_id: run_id.to_string(),
        status: plan.status_label.to_string(),
        allowed_files: spec.allow.fs_write.clone(),
        forbidden_files: plan.out_of_scope.clone(),
        allowed_commands: spec.allow.shell.clone(),
        required_commands: required.clone(),
        changed_files: ChangedFiles {
            added: plan.diff.added.clone(),
            modified: plan.diff.modified.clone(),
            deleted: plan.diff.deleted.clone(),
            in_scope: plan.in_scope.clone(),
            out_of_scope: plan.out_of_scope.clone(),
        },
        scope_violations: plan.violations.clone(),
        latest_receipts: receipt_summaries(repo, run_id, &required),
        current_failure: plan.current_failure.clone(),
        done_condition: DONE_CONDITION.to_string(),
        next_action: plan.next_action.to_string(),
        recommended_command: plan.recommended_command.clone(),
        instructions: plan.instructions.clone(),
        verified: state.verified,
    })
}

// ----- verify -----

pub fn verify(repo: &Path, run_id: &str, rerun: bool) -> io::Result<GuardStatus> {
    verify_inner(repo, run_id, rerun, None)
}

/// Verify with an optional crash hook (test-only). Scope gate first; then run each
/// required command for real, writing a receipt per command; then set `verified`.
pub fn verify_inner(
    repo: &Path,
    run_id: &str,
    rerun: bool,
    crash: Option<GuardCrash>,
) -> io::Result<GuardStatus> {
    let (record, mut state) = session(repo, run_id)?;
    let spec = &record.spec;
    let (head, _diff, _in_scope, out_of_scope) = scope_diff(repo, run_id, spec)?;
    let mut log = EventLog::resume(&store::events_path(repo, run_id), run_id)?;

    // Scope gate: an out-of-scope edit blocks the whole verify before any command
    // runs — record each violation in history.
    if !out_of_scope.is_empty() {
        for path in &out_of_scope {
            log.append(
                kind::SCOPE_VIOLATION,
                json!({ "path": path, "allowed": spec.allow.fs_write }),
            )?;
        }
        let summary = format!(
            "{} out-of-scope write(s): {}",
            out_of_scope.len(),
            out_of_scope.join(", ")
        );
        state.out_of_scope = out_of_scope.len();
        state.verified = false;
        state.guard_phase = "open".into();
        log.append(
            kind::VERIFY_FAILED,
            json!({ "reason": "out_of_scope", "summary": summary }),
        )?;
        store::save_state(repo, &state)?;
        return status(repo, run_id);
    }
    state.out_of_scope = 0;

    let digest = manifest_digest(&head);
    let required = spec.required_commands();
    state.commands_required = required.len();
    let mut passed = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for cmd in &required {
        // No ambient authority: a required command must also be allowlisted.
        if !spec.command_allowed(cmd) {
            failures.push(format!("`{cmd}` is required but not in allow.shell"));
            continue;
        }

        let action_id = format!("verify:{cmd}");
        let mut key_input = json!({ "command": cmd, "cwd": ".", "tree": digest });
        if rerun {
            // A fresh attempt nonce forces a new receipt even if the tree is unchanged.
            let attempts = store::list_receipts(repo, run_id)
                .iter()
                .filter(|r| r.input.get("command").and_then(|v| v.as_str()) == Some(cmd.as_str()))
                .count();
            key_input["rerun"] = json!(attempts);
        }
        let key = store::idempotency_key(run_id, &action_id, "shell.run", &key_input);

        let receipt = if let Some(existing) = store::find_receipt_by_key(repo, run_id, &key) {
            // Same command over the same tree — reuse the proof, do not re-run.
            log.append(
                kind::RECEIPT_REUSED,
                json!({ "receipt_id": existing.receipt_id, "command": cmd }),
            )?;
            existing
        } else {
            log.append(kind::SHELL_EXECUTED, json!({ "command": cmd, "cwd": "." }))?;
            let res = shell::run(&ShellRequest {
                command: cmd,
                cwd: repo,
                timeout_ms: VERIFY_TIMEOUT_MS,
                artifact_dir: &store::artifacts_dir(repo, run_id),
                key: &key,
            })?;
            let output = json!({
                "exit_code": res.exit_code,
                "timed_out": res.timed_out,
                "duration_ms": res.duration_ms,
                "argv": res.argv,
                "stdout_artifact": rel(repo, &res.stdout_path),
                "stderr_artifact": rel(repo, &res.stderr_path),
                "stdout_bytes": res.stdout_bytes,
                "stderr_bytes": res.stderr_bytes,
                "env_redacted": shell::allowed_env_names(),
            });
            let receipt = Receipt {
                receipt_id: store::receipt_id(&key),
                idempotency_key: key.clone(),
                action_id: action_id.clone(),
                tool: "shell.run".into(),
                input: key_input.clone(),
                output,
                run_id: run_id.to_string(),
                step: (state.receipts_created + 1) as u64,
                effect: "shell.run".into(),
                input_hash: store::input_hash(&key_input),
                approval_id: None,
                created_event_id: format!("evt_{:06}", log.peek_seq()),
            };
            store::save_receipt(repo, run_id, &receipt)?; // <- COMMIT POINT
            state.receipts_created += 1;
            log.append(
                kind::RECEIPT_CREATED,
                json!({ "receipt_id": receipt.receipt_id, "command": cmd, "idempotency_key": key }),
            )?;
            // Intra-verify danger window (test-only): the receipt is durable but the
            // verified bit is not yet saved. Resume must reuse this receipt.
            if matches!(crash, Some(GuardCrash::AfterReceipt(n)) if n == state.receipts_created) {
                state.verified = false;
                state.guard_phase = "open".into();
                store::save_state(repo, &state)?;
                return status(repo, run_id);
            }
            receipt
        };

        if receipt_passed(&receipt) {
            passed += 1;
        } else {
            let code = receipt
                .output
                .get("exit_code")
                .and_then(|v| v.as_i64())
                .map(|c| c.to_string())
                .unwrap_or_else(|| "timeout".into());
            failures.push(format!("{cmd}: exit {code}"));
        }
    }

    state.commands_passed = passed;
    state.verified = passed == required.len() && failures.is_empty();
    if state.verified {
        state.guard_phase = "verified".into();
        state.verified_digest = digest;
        log.append(
            kind::VERIFY_PASSED,
            json!({ "commands": required, "summary": format!("{passed} command(s) passed") }),
        )?;
    } else {
        state.guard_phase = "open".into();
        state.verified_digest = String::new();
        log.append(
            kind::VERIFY_FAILED,
            json!({ "reason": "command_failed", "summary": failures.join("; ") }),
        )?;
    }
    store::save_state(repo, &state)?;
    status(repo, run_id)
}

/// `guard verify --json` enriched with repair hints: run the verify, then derive
/// the rejection (if any) and the recommended next move from a fresh live view, so
/// a refused verify tells an agent exactly what to fix and what to run next.
pub fn verify_report(repo: &Path, run_id: &str, rerun: bool) -> io::Result<GuardVerify> {
    let status = verify_inner(repo, run_id, rerun, None)?;
    let (record, state) = session(repo, run_id)?;
    let plan = compute_plan(repo, run_id, &record.spec, &state)?;
    let rejection = rejection_from_plan(&record.spec, &state, &plan);
    Ok(GuardVerify {
        status,
        rejection,
        next_action: plan.next_action.to_string(),
        recommended_command: plan.recommended_command.clone(),
    })
}

// ----- commit / abort -----

pub fn commit(repo: &Path, run_id: &str) -> io::Result<GuardOutcome> {
    let (record, mut state) = session(repo, run_id)?;
    let spec = &record.spec;
    let mut log = EventLog::resume(&store::events_path(repo, run_id), run_id)?;

    // Re-check scope at commit (the agent may have edited after verify).
    let (head, _diff, _in_scope, out_of_scope) = scope_diff(repo, run_id, spec)?;
    if !out_of_scope.is_empty() {
        for path in &out_of_scope {
            log.append(
                kind::SCOPE_VIOLATION,
                json!({ "path": path, "allowed": spec.allow.fs_write }),
            )?;
        }
        state.out_of_scope = out_of_scope.len();
        store::save_state(repo, &state)?;
        return Ok(refuse(
            run_id,
            &state,
            format!("out-of-scope writes present: {}", out_of_scope.join(", ")),
        ));
    }

    if !state.verified {
        return Ok(refuse(
            run_id,
            &state,
            "not verified — run `tach guard verify` first".into(),
        ));
    }
    // The verified bit is only trustworthy if the tree is unchanged since verify.
    if manifest_digest(&head) != state.verified_digest {
        return Ok(refuse(
            run_id,
            &state,
            "tree changed since verify — run `tach guard verify` again".into(),
        ));
    }

    state.status = "completed".into();
    state.guard_phase = "committed".into();
    log.append(
        kind::GUARD_COMMITTED,
        json!({
            "commands_passed": state.commands_passed,
            "receipts": store::list_receipts(repo, run_id).len(),
        }),
    )?;
    log.append(kind::RUN_COMPLETED, json!({ "kind": "coding" }))?;
    store::save_state(repo, &state)?;
    Ok(GuardOutcome {
        run_id: run_id.to_string(),
        ok: true,
        reason: None,
        status: state.status,
        phase: state.guard_phase,
    })
}

/// A commit refusal: record nothing terminal, leave the run resumable.
fn refuse(run_id: &str, state: &RunState, reason: String) -> GuardOutcome {
    GuardOutcome {
        run_id: run_id.to_string(),
        ok: false,
        reason: Some(reason),
        status: state.status.clone(),
        phase: state.guard_phase.clone(),
    }
}

pub fn abort(repo: &Path, run_id: &str) -> io::Result<GuardOutcome> {
    let (_record, mut state) = session(repo, run_id)?;
    let mut log = EventLog::resume(&store::events_path(repo, run_id), run_id)?;
    state.status = "cancelled".into();
    state.guard_phase = "aborted".into();
    log.append(kind::GUARD_ABORTED, json!({}))?;
    log.append(kind::RUN_CANCELLED, json!({ "kind": "coding" }))?;
    store::save_state(repo, &state)?;
    Ok(GuardOutcome {
        run_id: run_id.to_string(),
        ok: true,
        reason: None,
        status: state.status,
        phase: state.guard_phase,
    })
}

// ----- replay -----

/// Replay a coding run from its durable history. By default this re-derives the
/// expected terminal state from the recorded receipts (no command is re-run) and
/// reports whether the record is internally consistent. With `rerun`, the required
/// commands are actually re-executed and the pass/fail verdict is compared.
pub fn replay(repo: &Path, run_id: &str, rerun: bool) -> io::Result<crate::runtime::ReplayResult> {
    let (record, recorded) = session(repo, run_id)?;
    let spec = &record.spec;

    if rerun {
        // Re-run the required commands against the current tree and re-derive the
        // verdict. Honest reproduction — minted fresh, compared to the record.
        let head = snapshot::snapshot(repo, &Ignore::load(repo))?;
        let digest = manifest_digest(&head);
        let mut passed = 0usize;
        for cmd in spec.required_commands() {
            if !spec.command_allowed(&cmd) {
                continue;
            }
            let key = store::idempotency_key(
                run_id,
                &format!("replay:{cmd}"),
                "shell.run",
                &json!({ "command": cmd, "tree": digest }),
            );
            let res = shell::run(&ShellRequest {
                command: &cmd,
                cwd: repo,
                timeout_ms: VERIFY_TIMEOUT_MS,
                artifact_dir: &store::artifacts_dir(repo, run_id),
                key: &format!("replay_{key}"),
            })?;
            if res.exit_code == Some(0) && !res.timed_out {
                passed += 1;
            }
        }
        let replayed = if passed == spec.required_commands().len() {
            "completed"
        } else {
            "failed"
        };
        let expected = if recorded.verified {
            "completed"
        } else {
            "failed"
        };
        return Ok(crate::runtime::ReplayResult {
            recorded_status: recorded.status.clone(),
            replayed_status: replayed.to_string(),
            identical: replayed == expected,
            steps: recorded.step,
        });
    }

    // Consistency replay: the recorded verified bit must match what the recorded
    // receipts say, and the terminal status must follow.
    let receipts = store::list_receipts(repo, run_id);
    let required = spec.required_commands();
    let passed = required
        .iter()
        .filter(|cmd| {
            receipts
                .iter()
                .filter(|r| r.input.get("command").and_then(|v| v.as_str()) == Some(cmd.as_str()))
                .any(receipt_passed)
        })
        .count();
    let derived_verified = passed == required.len() && recorded.out_of_scope == 0;
    let consistent = derived_verified == recorded.verified;
    let replayed_status = if consistent {
        recorded.status.clone()
    } else {
        "diverged".into()
    };
    Ok(crate::runtime::ReplayResult {
        recorded_status: recorded.status.clone(),
        replayed_status,
        identical: consistent,
        steps: recorded.step,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempRepo(PathBuf);
    impl TempRepo {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "tach_guard_{}_{}_{}",
                std::process::id(),
                tag,
                n
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempRepo(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, rel: &str, text: &str) {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
    }
    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A repo whose "test command" is a script that passes iff src/lib has FIXED.
    fn scaffold(r: &TempRepo, fixed: bool) {
        r.write("Cargo.toml", "[package]\nname=\"x\"\n");
        r.write(
            "Tachfile",
            &crate::adopt::coding_goal_source(
                "FixFailingTests",
                "sh check.sh",
                &["src/**".to_string(), "tests/**".to_string()],
            ),
        );
        r.write("check.sh", "grep -q FIXED src/lib.rs && exit 0 || exit 1\n");
        r.write(
            "src/lib.rs",
            if fixed { "// FIXED\n" } else { "// broken\n" },
        );
    }

    #[test]
    fn full_flow_begin_verify_commit() {
        let r = TempRepo::new("flow");
        scaffold(&r, false);
        let state = begin(r.path(), Some("FixFailingTests")).unwrap();
        assert_eq!(state.kind, "coding");
        assert_eq!(state.guard_phase, "open");
        assert!(store::load_baseline(r.path(), &state.run_id).is_ok());
        let id = state.run_id;

        // Broken tree → verify fails.
        let s = verify(r.path(), &id, false).unwrap();
        assert!(!s.verified, "broken tree should not verify");

        // Agent fixes the in-scope file → verify passes; a receipt exists.
        r.write("src/lib.rs", "// FIXED\n");
        let s = verify(r.path(), &id, false).unwrap();
        assert!(s.verified, "fixed tree should verify");
        assert_eq!(s.commands_passed, 1);
        let receipts = store::list_receipts(r.path(), &id);
        assert!(!receipts.is_empty());
        let art = receipts[0]
            .output
            .get("stdout_artifact")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(r.path().join(art).exists(), "artifact captured");

        // Commit finalizes into the ledger; git untouched.
        let out = commit(r.path(), &id).unwrap();
        assert!(out.ok, "commit should succeed: {:?}", out.reason);
        assert_eq!(
            store::load_state(r.path(), &id).unwrap().status,
            "completed"
        );
    }

    #[test]
    fn out_of_scope_edit_is_rejected() {
        let r = TempRepo::new("scope");
        scaffold(&r, true);
        let id = begin(r.path(), None).unwrap().run_id;
        r.write("README.md", "out of scope edit\n"); // outside src/**, tests/**
        let d = diff(r.path(), &id).unwrap();
        assert_eq!(d.out_of_scope, vec!["README.md"]);
        assert!(d.rejected);
        // verify refuses without running the command; commit refuses too.
        let s = verify(r.path(), &id, false).unwrap();
        assert!(!s.verified);
        let out = commit(r.path(), &id).unwrap();
        assert!(!out.ok);
        let events = read_all(&store::events_path(r.path(), &id)).unwrap();
        assert!(events.iter().any(|e| e.kind == kind::SCOPE_VIOLATION));
    }

    #[test]
    fn github_workflow_edit_is_out_of_scope_and_blocks_commit() {
        // The dot-directory blind spot, end to end: an agent that edits CI config
        // under a `src/**, tests/**` goal must be caught by the gate.
        let r = TempRepo::new("ghscope");
        scaffold(&r, true);
        let id = begin(r.path(), None).unwrap().run_id;
        r.write(".github/workflows/ci.yml", "on: push\njobs: {}\n");
        let d = diff(r.path(), &id).unwrap();
        assert_eq!(d.out_of_scope, vec![".github/workflows/ci.yml"]);
        assert!(d.rejected, "CI-config edit must be rejected at the gate");
        let s = verify(r.path(), &id, false).unwrap();
        assert!(!s.verified);
        let out = commit(r.path(), &id).unwrap();
        assert!(!out.ok, "out-of-scope CI edit must block commit");
        let events = read_all(&store::events_path(r.path(), &id)).unwrap();
        assert!(events.iter().any(|e| e.kind == kind::SCOPE_VIOLATION));
    }

    #[test]
    fn crash_after_receipt_reuses_on_resume() {
        let r = TempRepo::new("crash");
        scaffold(&r, true);
        let id = begin(r.path(), None).unwrap().run_id;

        // Crash right after the receipt is durable but before `verified` is saved.
        let _ = verify_inner(r.path(), &id, false, Some(GuardCrash::AfterReceipt(1))).unwrap();
        let st = store::load_state(r.path(), &id).unwrap();
        assert!(!st.verified, "verified not yet saved at crash");
        assert_eq!(store::list_receipts(r.path(), &id).len(), 1);

        // Resume verify (same tree): the receipt is reused, command not re-run.
        let s = verify(r.path(), &id, false).unwrap();
        assert!(s.verified);
        assert_eq!(
            store::list_receipts(r.path(), &id).len(),
            1,
            "no new receipt"
        );
        let events = read_all(&store::events_path(r.path(), &id)).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == kind::SHELL_EXECUTED)
                .count(),
            1,
            "command executed exactly once across crash+resume"
        );
        assert!(events.iter().any(|e| e.kind == kind::RECEIPT_REUSED));
    }

    #[test]
    fn replay_is_consistent_after_commit() {
        let r = TempRepo::new("replay");
        scaffold(&r, true);
        let id = begin(r.path(), None).unwrap().run_id;
        verify(r.path(), &id, false).unwrap();
        commit(r.path(), &id).unwrap();
        let rr = replay(r.path(), &id, false).unwrap();
        assert!(rr.identical, "recorded run should be self-consistent");
        assert_eq!(rr.replayed_status, "completed");
    }

    #[test]
    fn editing_after_verify_blocks_commit() {
        let r = TempRepo::new("staleverify");
        scaffold(&r, true);
        let id = begin(r.path(), None).unwrap().run_id;
        verify(r.path(), &id, false).unwrap();
        // Edit an in-scope file after the green verify → commit must refuse.
        r.write("src/lib.rs", "// FIXED but changed\n");
        let out = commit(r.path(), &id).unwrap();
        assert!(!out.ok, "stale verify must not commit");
        assert!(out.reason.unwrap().contains("tree changed"));
    }

    #[test]
    fn next_action_walks_editing_to_done() {
        let r = TempRepo::new("next");
        scaffold(&r, false); // broken tree
        let id = begin(r.path(), None).unwrap().run_id;

        // Fresh session, nothing edited yet → edit then verify.
        let n = next(r.path(), &id).unwrap();
        assert_eq!(n.status, "editing");
        assert_eq!(n.next_action, "edit_then_verify");
        assert_eq!(n.recommended_command, "tach guard verify");
        assert!(n.scope_violations.is_empty());
        assert!(n.done_condition.contains("verified=true"));

        // An in-scope edit that isn't yet verified → run verify.
        r.write("src/lib.rs", "// FIXED\n");
        let n = next(r.path(), &id).unwrap();
        assert_eq!(n.next_action, "run_verify");

        // After a green verify → finalize.
        verify(r.path(), &id, false).unwrap();
        let n = next(r.path(), &id).unwrap();
        assert_eq!(n.status, "verified");
        assert_eq!(n.next_action, "finalize");
        assert_eq!(n.recommended_command, "tach guard finalize");

        // After finalize → done, with no further command.
        commit(r.path(), &id).unwrap();
        let n = next(r.path(), &id).unwrap();
        assert_eq!(n.status, "finalized");
        assert_eq!(n.next_action, "done");
        assert!(n.recommended_command.is_empty());
    }

    #[test]
    fn next_and_verify_report_surface_scope_repair_hints() {
        let r = TempRepo::new("nextscope");
        scaffold(&r, true);
        let id = begin(r.path(), None).unwrap().run_id;
        r.write(".github/workflows/ci.yml", "on: push\n"); // out of src/**, tests/**

        let n = next(r.path(), &id).unwrap();
        assert_eq!(n.status, "blocked");
        assert_eq!(n.next_action, "fix_scope_violation");
        assert_eq!(n.scope_violations.len(), 1);
        assert_eq!(n.scope_violations[0].path, ".github/workflows/ci.yml");
        assert!(!n.scope_violations[0].allowed.is_empty());

        // `verify --json` carries the structured rejection + the preferred move.
        let v = verify_report(r.path(), &id, false).unwrap();
        assert!(!v.status.verified);
        assert_eq!(v.next_action, "fix_scope_violation");
        let rej = v.rejection.expect("scope rejection present");
        assert_eq!(rej.kind, "scope_violation");
        assert_eq!(rej.violations.len(), 1);
        assert_eq!(rej.preferred_next_action.kind, "revert_file");
        assert_eq!(
            rej.preferred_next_action.path.as_deref(),
            Some(".github/workflows/ci.yml")
        );
        assert!(rej
            .repair_strategies
            .contains(&"revert_out_of_scope_file".to_string()));
    }

    #[test]
    fn verify_report_flags_a_command_failure() {
        let r = TempRepo::new("cmdfail");
        scaffold(&r, false); // check.sh will exit 1
        let id = begin(r.path(), None).unwrap().run_id;
        r.write("src/lib.rs", "// still broken\n"); // in-scope edit, still failing

        let v = verify_report(r.path(), &id, false).unwrap();
        assert!(!v.status.verified);
        assert_eq!(v.next_action, "edit_then_verify");
        let rej = v.rejection.expect("command rejection present");
        assert_eq!(rej.kind, "command_failed");
        assert!(rej.violations.is_empty());
        assert_eq!(rej.preferred_next_action.kind, "edit_then_verify");
    }

    #[test]
    fn agent_context_exposes_the_full_contract() {
        let r = TempRepo::new("agentctx");
        scaffold(&r, true);
        let id = begin(r.path(), None).unwrap().run_id;
        verify(r.path(), &id, false).unwrap();

        let c = agent_context(r.path(), &id, "generic").unwrap();
        assert_eq!(c.agent, "generic");
        assert_eq!(c.schema, "tach.agent-context.v1");
        assert!(c.allowed_files.iter().any(|g| g.contains("src")));
        assert!(c
            .allowed_commands
            .iter()
            .any(|cmd| cmd.contains("check.sh")));
        assert_eq!(c.required_commands, vec!["sh check.sh".to_string()]);
        assert!(c.verified);
        assert_eq!(c.next_action, "finalize");
        assert!(c.scope_violations.is_empty());
        assert!(c.forbidden_files.is_empty());
        // The verified command minted a receipt with a captured stdout artifact.
        assert_eq!(c.latest_receipts.len(), 1);
        assert!(c.latest_receipts[0].passed);
        assert!(c.latest_receipts[0].stdout_artifact.is_some());
    }
}
