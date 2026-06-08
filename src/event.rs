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
    /// number right after the last recorded event. Used by `resume`.
    pub fn resume(path: &Path, run_id: &str) -> io::Result<Self> {
        let existing = read_all(path).unwrap_or_default();
        let next_seq = existing.iter().map(|e| e.seq).max().unwrap_or(0) + 1;
        Ok(EventLog {
            path: path.to_path_buf(),
            run_id: run_id.to_string(),
            next_seq,
        })
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

/// Read an entire event log back into memory, skipping any unparseable line.
pub fn read_all(path: &Path) -> io::Result<Vec<Event>> {
    let text = fs::read_to_string(path)?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Event>(l).ok())
        .collect())
}
