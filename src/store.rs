//! The durable goal store: everything a long-horizon run leaves on disk.
//!
//! Layout under a repository root:
//!
//! ```text
//! .perdure/goals/<run_id>/
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
use crate::snapshot::Manifest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
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
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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

    // ----- Coding/guard-layer fields (defaulted for back-compat) -----
    /// `""` for non-coding runs; for a `kind: "coding"` guard session one of
    /// `open` | `verified` | `committed` | `aborted`. The cosmetic mirror of the
    /// durable truth (baseline manifest + receipts + the last verify event).
    #[serde(default)]
    pub guard_phase: String,
    /// How many of the goal's `require { command(...).passes }` commands there are.
    #[serde(default)]
    pub commands_required: usize,
    /// How many of them passed (`exit_code == 0`) at the last `verify`.
    #[serde(default)]
    pub commands_passed: usize,
    /// Count of changed files outside the goal's `fs.write` scope at the last gate.
    #[serde(default)]
    pub out_of_scope: usize,
    /// The single bit an external agent must trust: every required command passed
    /// and no out-of-scope writes were present at the last successful `verify`.
    #[serde(default)]
    pub verified: bool,
    /// The tree digest (hash of the head manifest) that the last successful
    /// `verify` validated. `commit` recomputes the digest and refuses if it has
    /// changed — so edits made *after* a green verify can never be committed
    /// unverified. Anchored to the *post-command* tree, so files the authorized
    /// command itself produced (a refreshed `Cargo.lock`, build output) are part of
    /// what was certified and don't re-trip the gate at commit.
    #[serde(default)]
    pub verified_digest: String,
    /// Out-of-scope paths the *authorized verify command* generated (e.g. a refreshed
    /// `Cargo.lock`). Because verify's pre-command gate already proved the agent made
    /// no out-of-scope edit, anything out-of-scope in the post-command tree is the
    /// command's own output — recorded here so `commit` doesn't mistake the command's
    /// work for an agent violation. Advisory only: `commit` trusts it solely while the
    /// tree is byte-identical to `verified_digest`.
    #[serde(default)]
    pub tool_generated: Vec<String>,
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
/// when it proposes the action; the `perdure goal approve`/`deny` command flips it to
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
///
/// Beyond the exactly-once core (`idempotency_key` + `output`), a receipt is
/// self-describing for audit: it records the run it belongs to, the step it
/// committed at, the effect it represents, a content hash of the input, the
/// approval that authorized it (if gated), and the history event that recorded it.
/// The extra fields are `#[serde(default)]` so receipts written before they existed
/// still load.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub receipt_id: String,
    pub idempotency_key: String,
    pub action_id: String,
    pub tool: String,
    pub input: Value,
    pub output: Value,
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub step: u64,
    #[serde(default)]
    pub effect: String,
    #[serde(default)]
    pub input_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    #[serde(default)]
    pub created_event_id: String,
}

pub fn goals_root(repo: &Path) -> PathBuf {
    repo.join(".perdure").join("goals")
}

/// The sandbox `HOME` handed to every command Perdure runs, in place of the real user
/// home. Lives under `.perdure/` (which the snapshot gate hard-excludes), so a tool
/// writing into `$HOME` neither leaks credentials from nor churns the real repo.
pub fn sandbox_home(repo: &Path) -> PathBuf {
    repo.join(".perdure").join("sandbox-home")
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

/// Where a coding (guard) run captures real command stdout/stderr. Distinct from
/// receipts: a receipt is the durable proof an effect happened; an artifact is the
/// raw, nondeterministic output bytes it points at.
pub fn artifacts_dir(repo: &Path, run_id: &str) -> PathBuf {
    run_dir(repo, run_id).join("artifacts")
}

fn baseline_path(repo: &Path, run_id: &str) -> PathBuf {
    run_dir(repo, run_id).join("baseline.json")
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
    // Atomic *and durable* write — two distinct guarantees, both required:
    //
    //   * Atomicity — stage to a sibling `.tmp`, then `rename` into place. A reader
    //     (a resume, `list_receipts`) only ever sees the whole old file or the whole
    //     new one, never a half-written record. Without this a crash mid-write could
    //     strand a torn receipt that the lossy reader skips as "missing" and a resume
    //     re-runs — a double side effect.
    //   * Durability — `fsync` the tmp file's bytes *before* the rename, and `fsync`
    //     the parent directory *after* it, so both the data and the rename survive a
    //     power loss or kernel panic, not merely a clean process crash. (A clean
    //     crash keeps the page cache, so atomicity alone already covered the
    //     `--crash-after` demos; real durability needs the syncs. The Windows replace
    //     writes through to disk on its own — see `atomic_replace`.)
    //
    // A crash can at worst leave a stale `.tmp` behind, which the next write to the
    // same target overwrites; the rename is the commit point.
    let tmp = tmp_sibling(path);
    write_durable(&tmp, json.as_bytes())?;
    atomic_replace(&tmp, path)?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

/// Write `bytes` to `path` and `fsync` the file (data + metadata) before returning,
/// so the content is on stable storage — durable across power loss, not just a clean
/// process crash. The caller renames this staging file into place.
fn write_durable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// `fsync` a directory so a `rename`/`create` of an entry within it is itself durable
/// (POSIX requires syncing the directory, not just the file). On non-unix this is a
/// no-op: the Windows `atomic_replace` uses `MOVEFILE_WRITE_THROUGH`, which already
/// flushes the change through to disk, and directories are not `fsync`'able the same
/// way there.
#[cfg(unix)]
fn fsync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}
#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
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

/// Replace `dst` with `tmp` atomically on Windows. `std::fs::rename` refuses to
/// overwrite an existing file, and the old remove-then-rename dance left a crash
/// window where the destination was *missing* — fatal for a receipt, since a
/// missing receipt reads as "effect never happened" and re-runs the side effect.
/// `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` is a true atomic replace (a
/// reader sees the whole old file or the whole new one), and `MOVEFILE_WRITE_THROUGH`
/// flushes it to disk before returning — restoring the exactly-once guarantee.
#[cfg(windows)]
fn atomic_replace(tmp: &Path, dst: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    let from = wide(tmp);
    let to = wide(dst);
    // SAFETY: both pointers reference NUL-terminated wide strings that outlive the
    // call; the flags are valid for MoveFileExW.
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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

/// Save the baseline manifest a coding run snapshots at `perdure guard begin` — the
/// repo-relative path -> content-hash map the scope/diff gate compares against.
/// Kept separate from `goal.json` so a real repo's baseline (hashes only) never
/// bloats the goal record the way `base_files` (full source) would.
pub fn save_baseline(repo: &Path, run_id: &str, manifest: &Manifest) -> io::Result<()> {
    write_json(&baseline_path(repo, run_id), manifest)
}

pub fn load_baseline(repo: &Path, run_id: &str) -> io::Result<Manifest> {
    read_json(&baseline_path(repo, run_id))
}

fn baseline_ignore_path(repo: &Path, run_id: &str) -> PathBuf {
    run_dir(repo, run_id).join("baseline-ignore.json")
}

/// Freeze the ignore globs a coding run resolved at `perdure guard begin`, so every later
/// scope diff classifies against the *same* blind spots. Without this the gate would
/// re-read a live `.perdureignore` on each `verify`/`commit`, and an agent could add a
/// pattern mid-session to hide an out-of-scope write it had already made.
pub fn save_baseline_ignore(repo: &Path, run_id: &str, globs: &[String]) -> io::Result<()> {
    write_json(&baseline_ignore_path(repo, run_id), &globs.to_vec())
}

/// Load the frozen ignore globs. Runs that predate the freeze return `NotFound`, and
/// the caller falls back to a live load (back-compat, not a security regression for
/// any run begun after this shipped).
pub fn load_baseline_ignore(repo: &Path, run_id: &str) -> io::Result<Vec<String>> {
    read_json(&baseline_ignore_path(repo, run_id))
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
/// Lossy: an unparseable file is skipped. For display/inspect only — never for an
/// approval gate, where a skipped file would read as "no such gate".
pub fn list_approvals(repo: &Path, run_id: &str) -> Vec<Approval> {
    read_dir_json(&approvals_dir(repo, run_id), |a: &Approval| a.id.clone())
}

/// Like [`list_approvals`] but a corrupt file is a hard error, not a silent skip —
/// the variant a resume/gate must use so a damaged approval can never read as
/// "missing" and let an effect through.
pub fn list_approvals_strict(repo: &Path, run_id: &str) -> io::Result<Vec<Approval>> {
    read_dir_json_strict(&approvals_dir(repo, run_id), |a: &Approval| a.id.clone())
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

/// Every receipt recorded for a run, sorted by id. Lossy: an unparseable file is
/// skipped — for display/inspect only, never for exactly-once reuse.
pub fn list_receipts(repo: &Path, run_id: &str) -> Vec<Receipt> {
    read_dir_json(&receipts_dir(repo, run_id), |r: &Receipt| {
        r.receipt_id.clone()
    })
}

/// Like [`list_receipts`] but a corrupt receipt is a hard error. The variant the
/// runtime must use on resume/replay/verify: a receipt that exists-but-won't-parse
/// must block the run (a corrupt run), never read as "missing" — which would re-run
/// a side effect and break the exactly-once guarantee.
pub fn list_receipts_strict(repo: &Path, run_id: &str) -> io::Result<Vec<Receipt>> {
    read_dir_json_strict(&receipts_dir(repo, run_id), |r: &Receipt| {
        r.receipt_id.clone()
    })
}

/// Find a receipt by its idempotency key (per-run-dir scan). The presence of a
/// receipt for a key means the effect already happened — the driver reuses it
/// instead of calling the tool again. Strict: a corrupt receipt in the run dir is
/// an error, so a damaged proof can never be mistaken for an absent one.
pub fn find_receipt_by_key(repo: &Path, run_id: &str, key: &str) -> io::Result<Option<Receipt>> {
    Ok(list_receipts_strict(repo, run_id)?
        .into_iter()
        .find(|r| r.idempotency_key == key))
}

/// Read every `*.json` in a directory into `T`, skipping unparseable files, sorted
/// by a caller-supplied key. Returns empty if the directory does not exist. Lossy
/// by design — see the strict variant for resume-critical reads.
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

/// Strict twin of [`read_dir_json`]: any `*.json` that fails to parse is an error
/// (`InvalidData`), not a silent skip. A missing directory is still an empty list —
/// "no receipts yet" is legitimate; "a receipt that won't parse" is corruption.
fn read_dir_json_strict<T, K>(dir: &Path, key: K) -> io::Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
    K: Fn(&T) -> String,
{
    let mut items: Vec<T> = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(ref e) if e.kind() == io::ErrorKind::NotFound => return Ok(items),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let v = read_json::<T>(&path).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("corrupt durable record `{}`: {e}", path.display()),
                )
            })?;
            items.push(v);
        }
    }
    items.sort_by_key(&key);
    Ok(items)
}

// ----- Active guard session pointer -----
//
// A guard session spans many CLI calls (`begin`, then repeated `verify`, then
// `commit`). So the agent need not thread a run id through every command, the
// active run id is remembered in one small file; the CLI falls back to it when no
// id is given. It is a convenience pointer only — the per-run directory remains
// the durable truth.

fn active_guard_path(repo: &Path) -> PathBuf {
    repo.join(".perdure").join("guard-active")
}

pub fn set_active_guard(repo: &Path, run_id: &str) -> io::Result<()> {
    let path = active_guard_path(repo);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, run_id)
}

pub fn active_guard(repo: &Path) -> Option<String> {
    fs::read_to_string(active_guard_path(repo))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn clear_active_guard(repo: &Path) {
    let _ = fs::remove_file(active_guard_path(repo));
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

// ----- Per-run advisory lock -----

/// An advisory lock over one run directory, so two concurrent `perdure` processes can't
/// drive the same run at once. The receipt spine already makes that *safe* (a second
/// invoke re-derives the same idempotency key and reuses the receipt) but not *free*:
/// without a lock, two processes could both pass the "no receipt yet" check and invoke
/// a tool before either writes — running a real side effect twice. The lock closes
/// that window.
///
/// On unix it is an `flock` advisory lock that the kernel releases when the file
/// descriptor closes (i.e. on process exit), so a crashed holder never strands a stale
/// lock. Elsewhere it is a best-effort `create_new` lockfile removed on drop.
///
/// Honesty: the lockfile lives in agent-writable `.perdure/`, so a determined agent could
/// delete it. This guards against *accidental* concurrency (two honest invocations) —
/// it is not, and cannot be, a defense against an adversary with filesystem access.
/// How long [`RunLock::acquire`] keeps retrying a contended lock before failing. Sized
/// to outlast a same-process release latency (microseconds–milliseconds) but stay far
/// under any real verify/commit, so a genuine cross-process conflict still fails fast.
#[cfg(unix)]
const LOCK_ACQUIRE_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

pub struct RunLock {
    #[cfg(unix)]
    _file: fs::File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl RunLock {
    /// Acquire the lock for `run_id`, or fail with `WouldBlock` if another process
    /// holds it. Held until dropped (or the process exits).
    pub fn acquire(repo: &Path, run_id: &str) -> io::Result<RunLock> {
        let dir = run_dir(repo, run_id);
        fs::create_dir_all(&dir)?;
        let path = dir.join("lock");
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)?;
            // Non-blocking exclusive advisory lock. Auto-released when `file` (and thus
            // the fd) drops, including on process exit — so no stale-lock recovery is
            // ever needed.
            //
            // We retry `EWOULDBLOCK` for a short, bounded window before giving up. A
            // genuine concurrent holder keeps the lock for its whole verify/commit —
            // far longer than this window — so a real conflict still fails fast and the
            // exactly-once guarantee is intact. The retry only absorbs the brief window
            // where a *previous* holder in this same process has dropped its lock but
            // the kernel has not finished releasing the `flock` on `close()` yet —
            // observed on macOS under parallel load, e.g. an in-process `verify`
            // immediately followed by `commit`. (In production those are separate
            // processes, whose exit releases the lock outright, so the window is moot.)
            let mut waited = std::time::Duration::ZERO;
            let step = std::time::Duration::from_millis(2);
            loop {
                let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if rc == 0 {
                    return Ok(RunLock { _file: file });
                }
                if waited >= LOCK_ACQUIRE_GRACE {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!(
                            "run `{run_id}` is already being operated by another perdure process"
                        ),
                    ));
                }
                std::thread::sleep(step);
                waited += step;
            }
        }
        #[cfg(not(unix))]
        {
            match fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
            {
                Ok(_) => Ok(RunLock { path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "run `{run_id}` appears to be operated by another process \
                         (remove `{}` if it is stale)",
                        path.display()
                    ),
                )),
                Err(e) => Err(e),
            }
        }
    }
}

#[cfg(not(unix))]
impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
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

/// A content hash of a tool's input, recorded on the receipt so an audit can tell
/// two effects apart (or spot a duplicate) without re-canonicalizing the value.
/// Derived from the same canonical bytes as the idempotency key, so it depends only
/// on the input's content, not its serde key order.
pub fn input_hash(v: &Value) -> String {
    // SHA-256, not the FNV mixer below: `input_hash` is integrity/audit metadata on a
    // receipt, so it must resist a *crafted* collision (two distinct effects forged to
    // share a hash). The idempotency *key* stays FNV — it is addressing, not
    // tamper-evidence (re-deriving it on resume is all it must do).
    format!("ih_{}", crate::hash::sha256_hex(&[&canonical_bytes(v)]))
}

/// A short stable hex digest of a JSON value, used by fake tools to mint
/// deterministic result ids (e.g. `re_<digest>`) with no clock or randomness.
pub fn short_digest(v: &Value) -> String {
    fnv1a_hex(&[&canonical_bytes(v)])[..12].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_repo(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "perdure_store_{}_{}_{}",
            std::process::id(),
            tag,
            n
        ));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn receipt(id: &str, key: &str) -> Receipt {
        Receipt {
            receipt_id: id.to_string(),
            idempotency_key: key.to_string(),
            action_id: "a".into(),
            tool: "shell.run".into(),
            input: Value::Null,
            output: Value::Null,
            run_id: "run_x".into(),
            step: 1,
            effect: "shell.run".into(),
            input_hash: String::new(),
            approval_id: None,
            created_event_id: String::new(),
        }
    }

    #[test]
    fn corrupt_receipt_blocks_strict_reads_but_not_lossy() {
        let repo = tmp_repo("corrupt_receipt");
        let run_id = "run_x";
        save_receipt(&repo, run_id, &receipt("r1", "key1")).unwrap();
        // A receipt file that exists but won't parse — disk/version corruption.
        fs::write(receipts_dir(&repo, run_id).join("r2.json"), "{ not json").unwrap();

        // Lossy list skips the bad file; strict list and key lookup must error so a
        // damaged proof can never be mistaken for an absent one (which would re-run
        // the effect).
        assert_eq!(list_receipts(&repo, run_id).len(), 1);
        assert!(list_receipts_strict(&repo, run_id).is_err());
        assert!(find_receipt_by_key(&repo, run_id, "key1").is_err());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn durable_write_overwrites_stale_tmp_and_leaves_none_behind() {
        // A `.tmp` stranded by a hypothetical crash-before-rename must never poison
        // the next write, and a successful write leaves no staging file behind (the
        // rename consumes it).
        let repo = tmp_repo("durable");
        let run_id = "run_d";
        let state = RunState {
            run_id: run_id.into(),
            step: 1,
            ..Default::default()
        };
        save_state(&repo, &state).unwrap();
        let p = state_path(&repo, run_id);
        let tmp = tmp_sibling(&p);
        fs::write(&tmp, "garbage that must be overwritten").unwrap();

        let mut state2 = state.clone();
        state2.step = 2;
        save_state(&repo, &state2).unwrap();

        assert_eq!(
            load_state(&repo, run_id).unwrap().step,
            2,
            "target is the new value"
        );
        assert!(
            !tmp.exists(),
            "no stale .tmp survives a successful durable write"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn run_lock_is_exclusive_and_releases_on_drop() {
        let repo = tmp_repo("lock");
        let run_id = "run_l";
        let held = RunLock::acquire(&repo, run_id).unwrap();
        // A second acquire while the first is held is refused (WouldBlock).
        match RunLock::acquire(&repo, run_id) {
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::WouldBlock),
            Ok(_) => panic!("a held run lock must block a second acquire"),
        }
        // Releasing the first lets it be re-acquired.
        drop(held);
        assert!(
            RunLock::acquire(&repo, run_id).is_ok(),
            "lock must be acquirable again after release"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn find_receipt_by_key_returns_none_when_absent() {
        let repo = tmp_repo("absent_receipt");
        let run_id = "run_x";
        save_receipt(&repo, run_id, &receipt("r1", "key1")).unwrap();
        assert!(find_receipt_by_key(&repo, run_id, "key1")
            .unwrap()
            .is_some());
        assert!(find_receipt_by_key(&repo, run_id, "nope")
            .unwrap()
            .is_none());
        // An empty run dir is "no receipts yet", not corruption.
        let empty = tmp_repo("empty_receipts");
        assert!(list_receipts_strict(&empty, run_id).unwrap().is_empty());
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&empty);
    }
}
