//! Append-only event history for goal runs.
//!
//! A long-horizon agent run is not a single saved blob; it is a *log*. Every
//! meaningful thing that happens — a diagnostic emitted, a patch proposed,
//! verified, applied or rejected, a checkpoint written, the run completing —
//! becomes one immutable line of JSON appended to `events.jsonl`. That log is
//! the source of truth: `tach goal inspect` reads it, `tach goal resume` extends
//! it, and nothing rewrites history. Because events carry a logical sequence
//! number rather than a wall-clock time (`timestamp_mode: "deterministic"`), two
//! runs of the same deterministic goal produce byte-identical logs.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The schema tag stamped on every event. Bump this (and add a migration) only
/// when the event envelope itself changes shape.
pub const EVENT_SCHEMA: &str = "tach.event.v1";

/// One immutable entry in a run's history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub schema: String,
    pub event_id: String,
    pub run_id: String,
    pub seq: u64,
    pub kind: String,
    pub timestamp_mode: String,
    pub payload: Value,
}

impl Event {
    fn build(run_id: &str, seq: u64, kind: &str, payload: Value) -> Self {
        Event {
            schema: EVENT_SCHEMA.to_string(),
            event_id: format!("evt_{:06}", seq),
            run_id: run_id.to_string(),
            seq,
            kind: kind.to_string(),
            timestamp_mode: "deterministic".to_string(),
            payload,
        }
    }
}

/// The canonical event kinds a goal run emits, in roughly the order they occur.
/// Kept as constants (not an enum) so the JSONL stays open for forward-compatible
/// kinds — `tach goal query` matches on the string — while these names remain the
/// stable vocabulary callers can rely on.
pub mod kind {
    pub const RUN_STARTED: &str = "run.started";
    pub const RUN_RESUMED: &str = "run.resumed";
    pub const WORKSPACE_LOADED: &str = "workspace.loaded";
    pub const DIAGNOSTIC_EMITTED: &str = "diagnostic.emitted";
    pub const PATCH_PROPOSED: &str = "patch.proposed";
    pub const PATCH_VERIFIED: &str = "patch.verified";
    pub const PATCH_APPLIED: &str = "patch.applied";
    pub const PATCH_REJECTED: &str = "patch.rejected";
    pub const TEST_COMPLETED: &str = "test.completed";
    pub const EFFECT_DELTA_DETECTED: &str = "effect.delta_detected";
    pub const CHECKPOINT_WRITTEN: &str = "checkpoint.written";
    pub const BUDGET_EXHAUSTED: &str = "budget.exhausted";
    pub const RUN_COMPLETED: &str = "run.completed";
    pub const RUN_FAILED: &str = "run.failed";
    pub const RUN_CANCELLED: &str = "run.cancelled";

    // ----- Action Layer -----
    // A long-horizon *business* goal does not patch source; it proposes effectful
    // actions, pauses for human approval, calls (fake) tools, and proves each
    // effect with a durable receipt. These kinds record that lifecycle.
    pub const ACTION_PROPOSED: &str = "action.proposed";
    pub const APPROVAL_REQUESTED: &str = "approval.requested";
    pub const APPROVAL_GRANTED: &str = "approval.granted";
    pub const APPROVAL_DENIED: &str = "approval.denied";
    pub const TOOL_CALLED: &str = "tool.called";
    pub const TOOL_COMPLETED: &str = "tool.completed";
    pub const TOOL_FAILED: &str = "tool.failed";
    pub const RECEIPT_CREATED: &str = "receipt.created";
    /// An effectful action re-entered on resume whose receipt already exists — the
    /// tool is *not* called again. This is the no-duplicate-side-effect guarantee.
    pub const RECEIPT_REUSED: &str = "receipt.reused";
    pub const ACTION_SKIPPED: &str = "action.skipped";

    // ----- Coding / guard layer -----
    // A coding goal does not patch toy source or call fake tools; it gates an
    // external agent editing a real repo. These kinds record that session: the
    // baseline snapshot, real command execution, scope rejections, verification,
    // and the final accept-into-the-ledger commit.
    pub const GUARD_BEGUN: &str = "guard.begun";
    pub const FS_SNAPSHOTTED: &str = "fs.snapshotted";
    pub const SHELL_EXECUTED: &str = "shell.executed";
    pub const SCOPE_VIOLATION: &str = "scope.violation";
    pub const VERIFY_PASSED: &str = "verify.passed";
    pub const VERIFY_FAILED: &str = "verify.failed";
    pub const GUARD_COMMITTED: &str = "guard.committed";
    pub const GUARD_ABORTED: &str = "guard.aborted";
}

/// An append-only JSONL writer over a run's `events.jsonl`. Each `append` writes
/// exactly one line and flushes it, so a crash never loses an already-recorded
/// event — the property the whole resume story depends on.
pub struct EventLog {
    path: PathBuf,
    run_id: String,
    next_seq: u64,
}

impl EventLog {
    /// Open a fresh log for a run that is just starting. Uses `create_new`, so it
    /// **refuses to clobber** an existing history: a fresh run must land on a fresh
    /// path. Run ids are allocated to be unique (see `store::allocate_run`), so in
    /// normal operation this always succeeds; the refusal is the last line of
    /// defense against ever overwriting the durable record.
    pub fn create(path: &Path, run_id: &str) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        OpenOptions::new().write(true).create_new(true).open(path)?;
        Ok(EventLog {
            path: path.to_path_buf(),
            run_id: run_id.to_string(),
            next_seq: 1,
        })
    }

    /// Re-open an existing log to continue appending, picking up the sequence
    /// number right after the last recorded event. Used by `resume`. Reads
    /// strictly: a corrupt history is a hard error, not a silent reset to seq 1 —
    /// resuming onto a log we couldn't fully parse would mis-number and could
    /// clobber the durable record the whole resume story depends on.
    pub fn resume(path: &Path, run_id: &str) -> io::Result<Self> {
        let existing = match read_all_strict(path) {
            Ok(events) => events,
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let next_seq = existing.iter().map(|e| e.seq).max().unwrap_or(0) + 1;
        Ok(EventLog {
            path: path.to_path_buf(),
            run_id: run_id.to_string(),
            next_seq,
        })
    }

    /// The sequence number the *next* appended event will carry. Used to precompute
    /// the id of an event a receipt will reference before that event is emitted (a
    /// receipt is written before its `receipt.created` event, so the id can't be read
    /// back from `append`).
    pub fn peek_seq(&self) -> u64 {
        self.next_seq
    }

    /// Append one event durably and return it.
    pub fn append(&mut self, kind: &str, payload: Value) -> io::Result<Event> {
        let event = Event::build(&self.run_id, self.next_seq, kind, payload);
        self.next_seq += 1;
        let line = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        f.flush()?;
        Ok(event)
    }
}

/// Read an entire event log back into memory, skipping any unparseable line. Lossy
/// by design — for best-effort inspect/audit/display only, never for resume.
pub fn read_all(path: &Path) -> io::Result<Vec<Event>> {
    let text = fs::read_to_string(path)?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Event>(l).ok())
        .collect())
}

/// Strict twin of [`read_all`]: any non-empty line that fails to parse is an error
/// (`InvalidData`). Used by resume/replay, where a corrupt history must block the
/// run rather than be silently truncated.
pub fn read_all_strict(path: &Path) -> io::Result<Vec<Event>> {
    let text = fs::read_to_string(path)?;
    let mut events = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str::<Event>(line).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("corrupt event at {}:{}: {e}", path.display(), i + 1),
            )
        })?;
        events.push(event);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_log(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "tach_evt_{}_{}_{}.jsonl",
            std::process::id(),
            tag,
            n
        ));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn strict_read_rejects_a_corrupt_line_and_resume_blocks() {
        let path = tmp_log("corrupt");
        let mut log = EventLog::create(&path, "run_x").unwrap();
        log.append("test.event", serde_json::json!({ "ok": true }))
            .unwrap();
        // A garbage line appended after a valid one (disk/edit corruption).
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{{not json").unwrap();
        drop(f);

        // Lossy read silently skips the bad line; strict read refuses it; and a
        // resume must block rather than reset the sequence and clobber history.
        assert_eq!(
            read_all(&path).unwrap().len(),
            1,
            "lossy read skips garbage"
        );
        assert!(
            read_all_strict(&path).is_err(),
            "strict read errors on garbage"
        );
        assert!(
            EventLog::resume(&path, "run_x").is_err(),
            "resume must block on a corrupt log"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn resume_on_a_missing_log_starts_fresh() {
        // A never-written log is "no history yet", not corruption.
        let path = tmp_log("missing");
        let log = EventLog::resume(&path, "run_x").unwrap();
        assert_eq!(log.peek_seq(), 1);
    }
}
