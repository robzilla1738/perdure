//! The durable goal store: everything a long-horizon run leaves on disk.
//!
//! Layout under a repository root:
//!
//! ```text
//! .tach/goals/<run_id>/
//!   goal.json              the resolved GoalSpec + the base source snapshot
//!   state.json             the current RunState (status, step, metrics)
//!   events.jsonl           append-only history
//!   checkpoints/<step>.json a workspace snapshot taken after each step
//! ```
//!
//! Nothing here touches the repository's real source files; the runtime works on
//! an in-memory workspace and only writes verified results back when a run
//! completes green. That separation is what makes a crash safe: the store is the
//! durable state, and the working tree is never left half-edited.

use crate::goal::GoalSpec;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The persisted goal record: its contract plus the exact source it started from,
/// so a run is fully reproducible without depending on the live working tree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoalRecord {
    pub spec: GoalSpec,
    pub strategy: String,
    pub base_files: BTreeMap<String, String>,
    /// `""`/`"repair"` for the deterministic code-repair runtime; `"action"` for a
    /// business goal that runs a fixed action plan. Defaulted so records written
    /// before the action layer still load as repair runs.
    #[serde(default)]
    pub kind: String,
}

/// The mutable head of a run. Overwritten after every step (the events log is the
/// immutable history; this is the latest summary).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunState {
    pub run_id: String,
    pub goal: String,
    /// running | completed | failed | cancelled | budget_exhausted | awaiting_approval | denied
    pub status: String,
    /// Durable transitions recorded so far. For repair runs this equals the highest
    /// checkpoint; for action runs it counts every committed transition (including
    /// an approval pause), and is what `--crash-after step:N` keys off.
    pub step: u64,
    pub consecutive_rejections: u64,
    pub patches_applied: usize,
    pub patches_rejected: usize,
    pub tests_run: usize,
    pub regressions: usize,
    pub diff_chars: usize,
    pub final_errors: usize,
    pub tests_passed: usize,
    pub tests_failed: usize,

    // ----- Action-layer fields (defaulted for back-compat with pre-action state.json) -----
    /// `""`/`"repair"` vs `"action"`; mirrors `GoalRecord.kind` for rendering.
    #[serde(default)]
    pub kind: String,
    /// Next plan action index for an action run. This — not `step` — is the plan
    /// progress, and it is what the budget is billed against. Advancing it (and
    /// saving) is the sole commit that moves past an action / clears an approval gate.
    #[serde(default)]
    pub cursor: u64,
    /// The approval id this run is currently blocked on, when `status` is
    /// `awaiting_approval`. Cosmetic mirror of the approval file, which is the truth.
    #[serde(default)]
    pub pending_approval: Option<String>,
    #[serde(default)]
    pub actions_executed: usize,
    #[serde(default)]
    pub receipts_created: usize,
}

/// A workspace snapshot taken after a step, enough to resume from exactly here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub step: u64,
    pub status: String,
    pub files: BTreeMap<String, String>,
    pub consecutive_rejections: u64,
    pub patches_applied: usize,
    pub patches_rejected: usize,
    pub tests_run: usize,
    pub regressions: usize,
    pub diff_chars: usize,
}

/// A human approval gate on an effectful action. The driver writes it `pending`
/// when it proposes the action; the `tach goal approve`/`deny` command flips it to
/// `granted`/`denied`. **The approval file is the durable truth** for whether a
/// gate is resolved — the runtime only ever reads it after creating it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    pub action_id: String,
    pub tool: String,
    pub summary: String,
    /// pending | granted | denied
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Durable proof that an effectful action ran. Keyed by an idempotency key derived
/// from the action; re-entering the same action on resume finds this receipt and
/// skips the tool call, so a side effect happens exactly once.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub receipt_id: String,
    pub idempotency_key: String,
    pub action_id: String,
    pub tool: String,
    pub input: Value,
    pub output: Value,
}

pub fn goals_root(repo: &Path) -> PathBuf {
    repo.join(".tach").join("goals")
}

pub fn run_dir(repo: &Path, run_id: &str) -> PathBuf {
    goals_root(repo).join(run_id)
}

fn checkpoints_dir(repo: &Path, run_id: &str) -> PathBuf {
    run_dir(repo, run_id).join("checkpoints")
}

fn approvals_dir(repo: &Path, run_id: &str) -> PathBuf {
    run_dir(repo, run_id).join("approvals")
}

fn receipts_dir(repo: &Path, run_id: &str) -> PathBuf {
    run_dir(repo, run_id).join("receipts")
}

pub fn events_path(repo: &Path, run_id: &str) -> PathBuf {
    run_dir(repo, run_id).join("events.jsonl")
}

fn goal_path(repo: &Path, run_id: &str) -> PathBuf {
    run_dir(repo, run_id).join("goal.json")
}

fn state_path(repo: &Path, run_id: &str) -> PathBuf {
    run_dir(repo, run_id).join("state.json")
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into());
    // Atomic write: stage to a sibling `.tmp` file, then rename into place.
    // `fs::write` truncates first, so a crash mid-write would otherwise leave a
    // half-written receipt/approval/state on disk; `read_dir_json` silently skips
    // an unparseable file, so a resume would read it as "missing" and re-run the
    // effect — a double side effect. `rename` is atomic, so a reader only ever
    // sees the whole old file or the whole new one. A crash can at worst leave a
    // stale `.tmp` behind, which the next write to the same target overwrites.
    let tmp = tmp_sibling(path);
    fs::write(&tmp, json)?;
    atomic_replace(&tmp, path)
}

/// The deterministic `.tmp` staging path for an atomic write to `path`. No clock
/// or randomness (the durable store stays replayable): the rename is the atomic
/// step, and each durable file has a unique target name so its `.tmp` is unique.
fn tmp_sibling(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Replace `dst` with `tmp` as atomically as the platform allows.
#[cfg(not(windows))]
fn atomic_replace(tmp: &Path, dst: &Path) -> io::Result<()> {
    // POSIX `rename` atomically replaces an existing destination — the ideal case.
    fs::rename(tmp, dst)
}

/// Replace `dst` with `tmp`. Windows `rename` refuses to overwrite an existing
/// file (unlike POSIX), so remove the old one first. The window between the
/// remove and the rename is not crash-atomic, but a crash there leaves either the
/// fully-written new file or no file — never a half-written one — which is all the
/// resume logic needs (a missing receipt is simply re-derived).
#[cfg(windows)]
fn atomic_replace(tmp: &Path, dst: &Path) -> io::Result<()> {
    let _ = fs::remove_file(dst);
    fs::rename(tmp, dst)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn save_goal(repo: &Path, run_id: &str, record: &GoalRecord) -> io::Result<()> {
    write_json(&goal_path(repo, run_id), record)
}

pub fn load_goal(repo: &Path, run_id: &str) -> io::Result<GoalRecord> {
    read_json(&goal_path(repo, run_id))
}

pub fn save_state(repo: &Path, state: &RunState) -> io::Result<()> {
    write_json(&state_path(repo, &state.run_id), state)
}

pub fn load_state(repo: &Path, run_id: &str) -> io::Result<RunState> {
    read_json(&state_path(repo, run_id))
}

pub fn save_checkpoint(repo: &Path, run_id: &str, cp: &Checkpoint) -> io::Result<()> {
    write_json(
        &checkpoints_dir(repo, run_id).join(format!("{}.json", cp.step)),
        cp,
    )
}

/// Load the highest-numbered checkpoint, i.e. the most recent durable state.
pub fn load_latest_checkpoint(repo: &Path, run_id: &str) -> io::Result<Checkpoint> {
    let dir = checkpoints_dir(repo, run_id);
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if let Ok(n) = stem.parse::<u64>() {
                if best.as_ref().is_none_or(|(b, _)| n > *b) {
                    best = Some((n, path));
                }
            }
        }
    }
    match best {
        Some((_, path)) => read_json(&path),
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no checkpoints recorded for this run",
        )),
    }
}

// ----- Approvals -----

pub fn save_approval(repo: &Path, run_id: &str, a: &Approval) -> io::Result<()> {
    write_json(
        &approvals_dir(repo, run_id).join(format!("{}.json", a.id)),
        a,
    )
}

pub fn load_approval(repo: &Path, run_id: &str, approval_id: &str) -> io::Result<Approval> {
    read_json(&approvals_dir(repo, run_id).join(format!("{}.json", approval_id)))
}

/// Every approval recorded for a run, sorted by id (so output is deterministic).
pub fn list_approvals(repo: &Path, run_id: &str) -> Vec<Approval> {
    read_dir_json(&approvals_dir(repo, run_id), |a: &Approval| a.id.clone())
}

// ----- Receipts -----

pub fn save_receipt(repo: &Path, run_id: &str, r: &Receipt) -> io::Result<()> {
    write_json(
        &receipts_dir(repo, run_id).join(format!("{}.json", r.receipt_id)),
        r,
    )
}

pub fn load_receipt(repo: &Path, run_id: &str, receipt_id: &str) -> io::Result<Receipt> {
    read_json(&receipts_dir(repo, run_id).join(format!("{}.json", receipt_id)))
}

/// Every receipt recorded for a run, sorted by id.
pub fn list_receipts(repo: &Path, run_id: &str) -> Vec<Receipt> {
    read_dir_json(&receipts_dir(repo, run_id), |r: &Receipt| {
        r.receipt_id.clone()
    })
}

/// Find a receipt by its idempotency key (per-run-dir scan). The presence of a
/// receipt for a key means the effect already happened — the driver reuses it
/// instead of calling the tool again.
pub fn find_receipt_by_key(repo: &Path, run_id: &str, key: &str) -> Option<Receipt> {
    list_receipts(repo, run_id)
        .into_iter()
        .find(|r| r.idempotency_key == key)
}

/// Read every `*.json` in a directory into `T`, skipping unparseable files, sorted
/// by a caller-supplied key. Returns empty if the directory does not exist.
fn read_dir_json<T, K>(dir: &Path, key: K) -> Vec<T>
where
    T: for<'de> Deserialize<'de>,
    K: Fn(&T) -> String,
{
    let mut items: Vec<T> = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(v) = read_json::<T>(&path) {
                    items.push(v);
                }
            }
        }
    }
    items.sort_by_key(&key);
    items
}

/// Every run id present in the store, sorted.
pub fn list_runs(repo: &Path) -> Vec<String> {
    let mut ids = Vec::new();
    if let Ok(entries) = fs::read_dir(goals_root(repo)) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    ids.push(name.to_string());
                }
            }
        }
    }
    ids.sort();
    ids
}

/// A deterministic, offline *fingerprint* of a goal and the exact source it
/// starts from. No clock, no randomness — the same goal over the same code always
/// fingerprints the same, which is what makes a run human-recognizable and keeps
/// the first run of a goal addressable as the clean `run_<hash>`.
///
/// A fingerprint is **not** a run id. Two separate runs of the same goal over the
/// same source are distinct events in history and must not share an identity; see
/// [`allocate_run`], which turns a fingerprint into a unique, collision-free id.
pub fn fingerprint(goal: &str, base: &BTreeMap<String, String>) -> String {
    // FNV-1a over a stable serialization of (goal, files).
    let mut h: u64 = 0xcbf29ce484222325;
    let mut mix = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    };
    mix(goal.as_bytes());
    mix(&[0]);
    for (path, text) in base {
        mix(path.as_bytes());
        mix(&[0]);
        mix(text.as_bytes());
        mix(&[0]);
    }
    format!("run_{:016x}", h)
}

/// The run ids already present in the store that belong to a given fingerprint —
/// i.e. the prior runs of the same goal over the same source. Used to warn the
/// operator that starting a fresh run will not touch those histories.
pub fn runs_for_fingerprint(repo: &Path, fingerprint: &str) -> Vec<String> {
    list_runs(repo)
        .into_iter()
        .filter(|id| id == fingerprint || id.starts_with(&format!("{}-", fingerprint)))
        .collect()
}

/// Atomically claim a fresh, unique run directory for a new run, and return its
/// id. The first run of a fingerprint gets the clean `run_<hash>`; each subsequent
/// run gets `run_<hash>-2`, `run_<hash>-3`, … The claim is the directory itself:
/// `create_dir` fails if it already exists, so two processes racing to start the
/// same goal can never be handed the same id, and an existing run's history is
/// never reused or overwritten.
pub fn allocate_run(repo: &Path, fingerprint: &str) -> io::Result<String> {
    fs::create_dir_all(goals_root(repo))?;
    for n in 1u64..=1_000_000 {
        let id = if n == 1 {
            fingerprint.to_string()
        } else {
            format!("{}-{}", fingerprint, n)
        };
        match fs::create_dir(run_dir(repo, &id)) {
            Ok(()) => return Ok(id),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique run id (too many runs of this goal)",
    ))
}

// ----- Deterministic identity for the action layer -----
//
// Idempotency keys, approval ids, and receipt ids must be stable functions of
// their inputs with no clock and no randomness — the same property the run
// `fingerprint` above relies on. They are derived from the same FNV-1a mixer, fed
// a *canonical* byte serialization so a JSON object never hashes differently just
// because its keys were written in a different order.

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

/// FNV-1a over a sequence of byte slices, each followed by a 0 separator so that
/// `["ab","c"]` and `["a","bc"]` hash differently.
fn fnv1a_hex(parts: &[&[u8]]) -> String {
    let mut h: u64 = FNV_OFFSET;
    for part in parts {
        for b in *part {
            h ^= *b as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
        h ^= 0;
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("{:016x}", h)
}

/// A canonical, deterministic byte serialization of a JSON value: object keys are
/// emitted in sorted order regardless of the map's iteration order, so the result
/// (and any hash of it) depends only on the value's content.
pub fn canonical_bytes(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::String(s) => {
            out.push(b'"');
            out.extend_from_slice(s.as_bytes());
            out.push(b'"');
        }
        Value::Array(a) => {
            out.push(b'[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(x, out);
            }
            out.push(b']');
        }
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.push(b'"');
                out.extend_from_slice(k.as_bytes());
                out.extend_from_slice(b"\":");
                write_canonical(&m[*k], out);
            }
            out.push(b'}');
        }
    }
}

/// The idempotency key for an effectful action in a run. Includes the `run_id` so
/// re-running the *goal* (a fresh run) genuinely performs the effect again, while a
/// *resume* of the same run re-derives the identical key and reuses the receipt.
pub fn idempotency_key(run_id: &str, action_id: &str, tool: &str, input: &Value) -> String {
    let cb = canonical_bytes(input);
    format!(
        "idem_{}",
        fnv1a_hex(&[
            run_id.as_bytes(),
            action_id.as_bytes(),
            tool.as_bytes(),
            &cb,
        ])
    )
}

/// The deterministic approval id for a gated action, so `resume` can find the same
/// approval file without threading extra state.
pub fn approval_id(run_id: &str, action_id: &str) -> String {
    format!(
        "apr_{}",
        &fnv1a_hex(&[run_id.as_bytes(), action_id.as_bytes()])[..12]
    )
}

/// The deterministic receipt id for an idempotency key.
pub fn receipt_id(idempotency_key: &str) -> String {
    format!("rcpt_{}", &fnv1a_hex(&[idempotency_key.as_bytes()])[..12])
}

/// A short stable hex digest of a JSON value, used by fake tools to mint
/// deterministic result ids (e.g. `re_<digest>`) with no clock or randomness.
pub fn short_digest(v: &Value) -> String {
    fnv1a_hex(&[&canonical_bytes(v)])[..12].to_string()
}
