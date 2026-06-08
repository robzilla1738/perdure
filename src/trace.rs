//! Persisting agent runs so `tach trace` and `tach replay` can re-open them.
//!
//! The trace stores the *initial* file contents, every lap, and the final
//! result. Because runs are deterministic, the base files are all `replay`
//! needs to reproduce the exact same laps.

use crate::agent::{FixOutcome, RaceOutcome};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TraceFile {
    Fix(FixOutcome),
    Race(RaceOutcome),
}

fn trace_path(root: &Path) -> std::path::PathBuf {
    root.join(".tach").join("trace.json")
}

pub fn save(root: &Path, trace: &TraceFile) -> std::io::Result<()> {
    let dir = root.join(".tach");
    fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(trace).unwrap_or_else(|_| "{}".into());
    fs::write(trace_path(root), json)
}

pub fn load(root: &Path) -> Option<TraceFile> {
    let s = fs::read_to_string(trace_path(root)).ok()?;
    serde_json::from_str(&s).ok()
}
