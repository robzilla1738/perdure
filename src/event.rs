//! Append-only event history for goal runs.
//!
//! A long-horizon agent run is not a single saved blob; it is a *log*. Every
//! meaningful thing that happens — a diagnostic emitted, a patch proposed,
//! verified, applied or rejected, a checkpoint written, the run completing —
//! becomes one immutable line of JSON appended to `events.jsonl`. That log is
//! the source of truth: `perdure goal inspect` reads it, `perdure goal resume` extends
//! it, and nothing rewrites history. Because events carry a logical sequence
//! number rather than a wall-clock time (`timestamp_mode: "deterministic"`), two
//! runs of the same deterministic goal produce byte-identical logs.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The schema tag stamped on every event. v1 carries the tamper-evident chain
/// fields (`prev_hash`, `entry_hash`). A pre-release log without them still
/// deserializes via `#[serde(default)]`, and [`verify_chain`] reports its
/// hash-less events as legacy/unverifiable rather than as tampering.
pub const EVENT_SCHEMA: &str = "perdure.event.v1";

/// The chain anchor: the `prev_hash` of a run's first event. A fixed, content-free
/// constant — no clock, no randomness — so two identical runs chain identically and
/// the determinism the replay story depends on is preserved.
pub const GENESIS_HASH: &str = "genesis";

/// One immutable, self-authenticating entry in a run's history. The log is a hash
/// chain: each event commits to the one before it, so an agent with write access to
/// `.perdure/` can still *corrupt* its own ledger — Perdure has no sandbox to stop that —
/// but it cannot do so **undetectably**. [`verify_chain`] (surfaced as
/// `perdure guard audit`) re-derives every link and flags any edit, insertion, removal,
/// or reorder.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub schema: String,
    pub event_id: String,
    pub run_id: String,
    pub seq: u64,
    pub kind: String,
    pub timestamp_mode: String,
    pub payload: Value,
    /// The `entry_hash` of the previous event (or [`GENESIS_HASH`] for the first), so
    /// tampering with any event breaks every link after it. `#[serde(default)]` keeps
    /// v1 logs readable.
    #[serde(default)]
    pub prev_hash: String,
    /// SHA-256 over this event's canonical content — every field above, `prev_hash`
    /// included, but not `entry_hash` itself (which would be circular). Recompute it
    /// and any altered byte shows.
    #[serde(default)]
    pub entry_hash: String,
}

impl Event {
    fn build(run_id: &str, seq: u64, kind: &str, payload: Value, prev_hash: &str) -> Self {
        let mut ev = Event {
            schema: EVENT_SCHEMA.to_string(),
            event_id: format!("evt_{:06}", seq),
            run_id: run_id.to_string(),
            seq,
            kind: kind.to_string(),
            timestamp_mode: "deterministic".to_string(),
            payload,
            prev_hash: prev_hash.to_string(),
            entry_hash: String::new(),
        };
        ev.entry_hash = ev.compute_hash();
        ev
    }

    /// SHA-256 over the canonical serialization of every field *except* `entry_hash`.
    /// Uses the same canonical-bytes encoder the store hashes its ids with, so a
    /// different serde key order can never change the result.
    pub fn compute_hash(&self) -> String {
        let content = serde_json::json!({
            "schema": self.schema,
            "event_id": self.event_id,
            "run_id": self.run_id,
            "seq": self.seq,
            "kind": self.kind,
            "timestamp_mode": self.timestamp_mode,
            "payload": self.payload,
            "prev_hash": self.prev_hash,
        });
        crate::hash::sha256_hex(&[&crate::store::canonical_bytes(&content)])
    }
}

/// The canonical event kinds a goal run emits, in roughly the order they occur.
/// Kept as constants (not an enum) so the JSONL stays open for forward-compatible
/// kinds — `perdure goal query` matches on the string — while these names remain the
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
/// exactly one line and `fsync`s it, so a crash never loses an already-recorded
/// event — the property the whole resume story depends on. It also threads the hash
/// chain: `last_hash` is the `entry_hash` of the most recent event, fed as the next
/// event's `prev_hash`.
pub struct EventLog {
    path: PathBuf,
    run_id: String,
    next_seq: u64,
    last_hash: String,
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
            last_hash: GENESIS_HASH.to_string(),
        })
    }

    /// Re-open an existing log to continue appending, picking up the sequence
    /// number right after the last recorded event. Used by `resume`. Reads
    /// strictly: a corrupt history is a hard error, not a silent reset to seq 1 —
    /// resuming onto a log we couldn't fully parse would mis-number and could
    /// clobber the durable record the whole resume story depends on.
    ///
    /// The one corruption it *does* recover from is a power-loss **torn tail**: an
    /// append interrupted by a crash leaves a final line with no terminating newline
    /// (the content is written before the `\n`). That single partial line is
    /// truncated away and resume continues, so a crash mid-append never permanently
    /// bricks a run. Any *interior* corrupt line — the signature of tampering or
    /// disk rot, not an interrupted append — still blocks.
    pub fn resume(path: &Path, run_id: &str) -> io::Result<Self> {
        let existing = match read_all_strict(path) {
            Ok(events) => events,
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            // Strict read failed: attempt torn-tail recovery, and only if *that* also
            // fails do we surface the original (interior-corruption) error.
            Err(e) => recover_torn_tail(path).map_err(|_| e)?,
        };
        let next_seq = existing.iter().map(|e| e.seq).max().unwrap_or(0) + 1;
        // Continue the chain from the last event's hash. A legacy (v1) tail event has
        // no `entry_hash`, so we fall back to genesis — the next v2 event then anchors
        // the chain, which `verify_chain` accepts at the v1→v2 boundary.
        let last_hash = existing
            .last()
            .map(|e| e.entry_hash.clone())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| GENESIS_HASH.to_string());
        Ok(EventLog {
            path: path.to_path_buf(),
            run_id: run_id.to_string(),
            next_seq,
            last_hash,
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
        let event = Event::build(&self.run_id, self.next_seq, kind, payload, &self.last_hash);
        self.next_seq += 1;
        self.last_hash = event.entry_hash.clone();
        let line = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        // `sync_all`, not `flush`: `Write::flush` on a `std::fs::File` is a no-op
        // (the file has no user-space buffer), so it bought us nothing — the bytes
        // sat in the page cache and a power loss could drop an already-"recorded"
        // event. `sync_all` forces them to disk, so the durability the resume story
        // depends on is real, not merely clean-crash deep. The content is written
        // before the terminating newline so an interrupted append leaves a final
        // line with no newline — the signal `EventLog::resume` uses to recover.
        f.sync_all()?;
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

/// Recover a log whose only damage is a power-loss-truncated **final** line, and
/// truncate the file back to the last intact event so future appends land cleanly.
///
/// A torn write has a precise signature: `append` writes a line's bytes, then `\n`,
/// then `fsync`, so an interrupted append leaves the *last* physical line with **no
/// terminating newline** and an unexpected-EOF JSON parse error. Recovery fires only
/// for that exact shape:
///   * a complete-but-invalid line (it has a newline, or parses to a non-EOF error)
///     is genuine corruption / tampering, not a torn append → error;
///   * any non-empty content *after* a torn line means the damage is interior, not a
///     tail → error.
///
/// On success the partial bytes are removed and `fsync`'d, so the repair itself is
/// durable; on any other damage the file is left untouched and the caller blocks.
fn recover_torn_tail(path: &Path) -> io::Result<Vec<Event>> {
    let bytes = fs::read(path)?;
    let corrupt = |msg: String| io::Error::new(io::ErrorKind::InvalidData, msg);
    let mut events: Vec<Event> = Vec::new();
    let mut valid_end: usize = 0; // byte offset just past the last intact line
    let mut cursor = 0usize;
    let mut torn = false;
    while cursor < bytes.len() {
        let nl = bytes[cursor..].iter().position(|&b| b == b'\n');
        let (content_end, next) = match nl {
            Some(rel) => (cursor + rel, cursor + rel + 1),
            None => (bytes.len(), bytes.len()), // final line, no terminating newline
        };
        let content = std::str::from_utf8(&bytes[cursor..content_end]).unwrap_or("\u{fffd}");
        if content.trim().is_empty() {
            valid_end = next;
            cursor = next;
            continue;
        }
        if torn {
            return Err(corrupt(format!(
                "{}: content follows a truncated line — interior corruption, not a torn tail",
                path.display()
            )));
        }
        match serde_json::from_str::<Event>(content) {
            Ok(ev) => {
                events.push(ev);
                valid_end = next;
            }
            // The only recoverable case: an unterminated *and* EOF-truncated final line.
            Err(e) if nl.is_none() && e.is_eof() => torn = true,
            Err(e) => return Err(corrupt(format!("{}: {e}", path.display()))),
        }
        cursor = next;
    }
    if !torn {
        return Err(corrupt(format!(
            "{}: no recoverable torn tail",
            path.display()
        )));
    }
    let f = OpenOptions::new().write(true).open(path)?;
    f.set_len(valid_end as u64)?;
    f.sync_all()?;
    Ok(events)
}

/// A broken link found by [`verify_chain`]: which event, and why.
#[derive(Debug, Clone)]
pub struct ChainBreak {
    pub index: usize,
    pub seq: u64,
    pub reason: String,
}

/// Verify a run's hash chain end to end. Each hashed event must (a) self-authenticate
/// — its `entry_hash` recomputes from its content — and (b) link to its predecessor —
/// its `prev_hash` equals the previous event's `entry_hash` (or [`GENESIS_HASH`] for
/// the first). The first failure is returned; `Ok(())` means the history is intact.
///
/// Legacy v1 events (no `entry_hash`) are *skipped* as unverifiable rather than
/// flagged, and reset the link anchor to genesis — so auditing a pre-v2 or mixed log
/// degrades gracefully instead of crying tamper at the migration boundary. (Detection
/// is the guarantee, not prevention: an agent with `.perdure/` write access can rewrite
/// the file, but it cannot forge a *valid* chain without inverting SHA-256.)
pub fn verify_chain(events: &[Event]) -> Result<(), ChainBreak> {
    let mut expected_prev = GENESIS_HASH.to_string();
    for (i, e) in events.iter().enumerate() {
        if e.entry_hash.is_empty() {
            expected_prev = GENESIS_HASH.to_string();
            continue;
        }
        if e.compute_hash() != e.entry_hash {
            return Err(ChainBreak {
                index: i,
                seq: e.seq,
                reason: "event content was altered (entry_hash mismatch)".to_string(),
            });
        }
        if e.prev_hash != expected_prev {
            return Err(ChainBreak {
                index: i,
                seq: e.seq,
                reason: "broken chain link — an event was inserted, removed, or reordered"
                    .to_string(),
            });
        }
        expected_prev = e.entry_hash.clone();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_log(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "perdure_evt_{}_{}_{}.jsonl",
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

    #[test]
    fn torn_tail_is_recovered_on_resume() {
        // Simulate a power-loss-interrupted append: two intact events, then a partial
        // JSON line with NO terminating newline (append writes content, then '\n',
        // then fsyncs — an interrupted append leaves exactly this shape).
        let path = tmp_log("torn");
        let mut log = EventLog::create(&path, "run_x").unwrap();
        log.append("a.event", serde_json::json!({ "i": 1 }))
            .unwrap();
        log.append("b.event", serde_json::json!({ "i": 2 }))
            .unwrap();
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"schema\":\"perdure.event.v0\",\"seq\":99")
            .unwrap(); // truncated, no '\n'
        drop(f);

        // A pure strict read still errors (it never mutates the file)...
        assert!(
            read_all_strict(&path).is_err(),
            "strict read is non-destructive"
        );
        // ...but resume recovers: it truncates the torn tail and continues from seq 3.
        let log2 = EventLog::resume(&path, "run_x").unwrap();
        assert_eq!(log2.peek_seq(), 3, "next seq follows the 2 intact events");
        // The torn bytes are gone, so the log is clean again and appends land right.
        let events = read_all_strict(&path).unwrap();
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2],
            "exactly the intact events survive"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn intact_chain_verifies_and_tampering_is_detected() {
        let path = tmp_log("chain");
        let mut log = EventLog::create(&path, "run_x").unwrap();
        log.append("a.event", serde_json::json!({ "i": 1 }))
            .unwrap();
        log.append("b.event", serde_json::json!({ "i": 2 }))
            .unwrap();
        log.append("c.event", serde_json::json!({ "i": 3 }))
            .unwrap();
        let events = read_all_strict(&path).unwrap();
        assert!(verify_chain(&events).is_ok(), "an untouched chain verifies");

        // Edit a payload in the middle — its self-hash no longer matches.
        let mut edited = events.clone();
        edited[1].payload = serde_json::json!({ "i": 999 });
        let br = verify_chain(&edited).unwrap_err();
        assert_eq!(br.seq, 2);
        assert!(br.reason.contains("altered"), "reason: {}", br.reason);

        // Remove the middle event — the link from c back to a is broken.
        let mut removed = events.clone();
        removed.remove(1);
        assert!(
            verify_chain(&removed).is_err(),
            "a removed event breaks the chain"
        );

        // Reorder two events — same broken link.
        let mut reordered = events.clone();
        reordered.swap(1, 2);
        assert!(
            verify_chain(&reordered).is_err(),
            "a reordered event breaks the chain"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn legacy_v1_events_are_unverifiable_not_flagged() {
        // A v1 event carries no entry_hash; verify_chain skips it (degrades
        // gracefully on a pre-chain or mixed log) rather than crying tamper.
        let legacy = Event {
            schema: "perdure.event.v0".into(),
            event_id: "evt_000001".into(),
            run_id: "run_x".into(),
            seq: 1,
            kind: "x".into(),
            timestamp_mode: "deterministic".into(),
            payload: serde_json::json!({}),
            prev_hash: String::new(),
            entry_hash: String::new(),
        };
        assert!(verify_chain(&[legacy]).is_ok());
    }

    #[test]
    fn corruption_before_more_content_still_blocks_resume() {
        // A torn-looking partial line that is NOT the final line (content follows) is
        // interior damage — tampering or disk rot — and must block, not "recover".
        let path = tmp_log("interior");
        let mut log = EventLog::create(&path, "run_x").unwrap();
        log.append("a.event", serde_json::json!({ "i": 1 }))
            .unwrap();
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"schema\":\"perdure.event.v0\"\n{\"more\":true}\n")
            .unwrap();
        drop(f);
        assert!(
            EventLog::resume(&path, "run_x").is_err(),
            "interior corruption must block resume, never silently truncate"
        );
        let _ = fs::remove_file(&path);
    }
}
