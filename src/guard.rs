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
    /// The check that proves the work itself is correct: every required command
    /// passed and no out-of-scope write is present. Necessary, but *not* sufficient
    /// to claim the task done — the run must still be finalized into the ledger.
    pub verification_condition: String,
    /// The single check an agent must use to decide it is finished, stated so the
    /// packet is self-describing: do not claim done until this holds. Stronger than
    /// `verification_condition` — it also requires the run to be committed.
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

    // A guard session governs an external agent editing a real repo, so its goal
    // must be fully specified before we open it — no ambient authority, no
    // evidence-free "success". Reject under-specified coding goals up front.
    if spec.required_commands().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "coding goal `{}` must declare at least one `require {{ command(\"…\").passes }}` — \
                 a guard session cannot verify without command evidence",
                spec.name
            ),
        ));
    }
    if spec.allow.fs_write.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "coding goal `{}` must declare an `allow {{ fs.write [...] }}` scope — \
                 a guard session never grants unrestricted write authority",
                spec.name
            ),
        ));
    }

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
    // For a guard session, the goal *must* declare `fs.write` (enforced at
    // `begin`). Treat an absent scope as "no writes allowed" so a missing grant can
    // never silently mean unrestricted authority — the opposite of Tach's thesis.
    let in_writable = |path: &str| match &writes {
        Some(globs) => globs.iter().any(|g| glob_match(g, path)),
        None => false,
    };
    let is_symlink = |path: &str| {
        matches!(
            head.get(path).map(|e| e.kind),
            Some(snapshot::EntryKind::Symlink)
        )
    };
    let mut in_scope = Vec::new();
    let mut out_of_scope = Vec::new();
    for path in diff.changed() {
        // A symlink under writable scope is rejected by default: Tach never follows
        // it, so a write *through* the link lands outside the gate's view.
        if in_writable(&path) && !is_symlink(&path) {
            in_scope.push(path);
        } else {
            out_of_scope.push(path);
        }
    }
    // A pre-existing symlink under writable scope is a write-through vector even
    // when the link itself never appears in the diff (its target path is unchanged,
    // only the pointed-to bytes moved). Reject any such link in the head tree.
    for (path, entry) in &head {
        if entry.kind == snapshot::EntryKind::Symlink
            && in_writable(path)
            && !out_of_scope.contains(path)
        {
            out_of_scope.push(path.clone());
        }
    }
    out_of_scope.sort();
    out_of_scope.dedup();
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

/// The history payload for one out-of-scope write, naming *why* it was rejected so
/// the agent can react: a `symlink` under writable scope (a write-through vector
/// Tach refuses by default) versus an ordinary `out_of_scope` path.
fn scope_violation(head: &Manifest, path: &str, allowed: &[String]) -> Value {
    let reason = if matches!(
        head.get(path).map(|e| e.kind),
        Some(snapshot::EntryKind::Symlink)
    ) {
        "symlink"
    } else {
        "out_of_scope"
    };
    json!({ "path": path, "allowed": allowed, "reason": reason })
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
        verification_condition: "`tach guard status --json` reports verified=true".to_string(),
        done_condition: "`tach guard status --json` reports verified=true and phase=committed \
             (run `tach guard commit` after a green verify)"
            .to_string(),
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
                scope_violation(&head, path, &spec.allow.fs_write),
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
            let attempts = store::list_receipts_strict(repo, run_id)?
                .iter()
                .filter(|r| r.input.get("command").and_then(|v| v.as_str()) == Some(cmd.as_str()))
                .count();
            key_input["rerun"] = json!(attempts);
        }
        let key = store::idempotency_key(run_id, &action_id, "shell.run", &key_input);

        let receipt = if let Some(existing) = store::find_receipt_by_key(repo, run_id, &key)? {
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
                home: &store::sandbox_home(repo),
            })?;
            let output = json!({
                "exit_code": res.exit_code,
                "timed_out": res.timed_out,
                "duration_ms": res.duration_ms,
                "argv": res.argv,
                "program_path": res.program_path,
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
    // `!required.is_empty()` is defense in depth: `begin` already rejects a
    // zero-command goal, but a hand-crafted `goal.json` must never let an empty
    // requirement set make `passed == required.len()` vacuously true.
    state.verified = !required.is_empty() && passed == required.len() && failures.is_empty();
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
                scope_violation(&head, path, &spec.allow.fs_write),
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
                home: &store::sandbox_home(repo),
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
    // receipts say, and the terminal status must follow. Strict read — a corrupt
    // receipt blocks the replay rather than silently skewing the derived verdict.
    let receipts = store::list_receipts_strict(repo, run_id)?;
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
    fn begin_rejects_goal_with_no_required_command() {
        let r = TempRepo::new("nocmd");
        r.write("Cargo.toml", "[package]\nname=\"x\"\n");
        // Declares fs.write but no `command(...).passes` — nothing proves success.
        r.write(
            "Tachfile",
            "goal NoCmd -> Success {\n  budget {\n    steps: 40\n  }\n  \
             allow {\n    fs.write [\"src/**\"]\n    shell.run [\"sh check.sh\"]\n  }\n  \
             require {\n    no_out_of_scope_writes\n  }\n}\n",
        );
        let err = begin(r.path(), None).unwrap_err();
        assert!(
            err.to_string().contains("command"),
            "begin must reject an evidence-free coding goal: {err}"
        );
    }

    #[test]
    fn begin_rejects_goal_with_no_fs_write() {
        let r = TempRepo::new("nowrite");
        r.write("Cargo.toml", "[package]\nname=\"x\"\n");
        // Declares a required command but no fs.write scope — no ambient authority.
        r.write(
            "Tachfile",
            "goal NoWrite -> Success {\n  budget {\n    steps: 40\n  }\n  \
             allow {\n    shell.run [\"sh check.sh\"]\n  }\n  \
             require {\n    command(\"sh check.sh\").passes\n  }\n}\n",
        );
        let err = begin(r.path(), None).unwrap_err();
        assert!(
            err.to_string().contains("fs.write"),
            "begin must reject a coding goal with no write scope: {err}"
        );
    }

    #[test]
    fn verify_never_passes_with_zero_required_commands() {
        // Defense in depth: even if a goal.json is hand-crafted past `begin` with no
        // required commands, `verify` must never report verified=true.
        let r = TempRepo::new("zerocmd");
        scaffold(&r, true);
        let id = begin(r.path(), None).unwrap().run_id;
        let mut record = store::load_goal(r.path(), &id).unwrap();
        record.spec.require.retain(|c| !c.starts_with("command:"));
        store::save_goal(r.path(), &id, &record).unwrap();
        let s = verify(r.path(), &id, false).unwrap();
        assert!(
            !s.verified,
            "no required commands must never read as verified"
        );
    }

    #[cfg(unix)]
    #[test]
    fn newly_added_symlink_under_scope_is_rejected() {
        use std::os::unix::fs::symlink;
        let r = TempRepo::new("symlink_new");
        scaffold(&r, true);
        let id = begin(r.path(), None).unwrap().run_id;
        // Agent adds a symlink inside writable scope pointing outside the repo — a
        // write-through vector Tach never follows.
        symlink("../secret.txt", r.path().join("src/outside")).unwrap();
        let d = diff(r.path(), &id).unwrap();
        assert!(
            d.out_of_scope.contains(&"src/outside".to_string()),
            "symlink under fs.write must be flagged: {:?}",
            d.out_of_scope
        );
        assert!(d.rejected);
        assert!(!verify(r.path(), &id, false).unwrap().verified);
        assert!(!commit(r.path(), &id).unwrap().ok);
        let events = read_all(&store::events_path(r.path(), &id)).unwrap();
        assert!(
            events.iter().any(|e| e.kind == kind::SCOPE_VIOLATION
                && e.payload.get("reason").and_then(|v| v.as_str()) == Some("symlink")),
            "violation must name the symlink reason"
        );
    }

    #[cfg(unix)]
    #[test]
    fn preexisting_symlink_under_scope_blocks_verify() {
        use std::os::unix::fs::symlink;
        // The dangerous case: a symlink that existed *before* the session. Its target
        // path never changes, so it never appears in the diff — yet a write through it
        // lands outside the gate's view. The gate must still reject it.
        let r = TempRepo::new("symlink_pre");
        scaffold(&r, true);
        symlink("../secret.txt", r.path().join("src/outside")).unwrap();
        let id = begin(r.path(), None).unwrap().run_id;
        let d = diff(r.path(), &id).unwrap();
        assert!(
            d.added.is_empty() && d.modified.is_empty(),
            "unchanged link should be absent from the diff"
        );
        assert!(
            d.out_of_scope.contains(&"src/outside".to_string()),
            "pre-existing symlink under scope must still be rejected"
        );
        assert!(d.rejected);
        assert!(!verify(r.path(), &id, false).unwrap().verified);
    }
}
