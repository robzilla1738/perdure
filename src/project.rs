//! Project I/O: discovering `.tach` files into a `Workspace`, writing verified
//! results back, and scaffolding new projects with `tach new`.

use crate::patch::Workspace;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Load every `.tach` file under `root` into a workspace, keyed by path relative
/// to `root` (using forward slashes for stability).
pub fn load_workspace(root: &Path) -> io::Result<Workspace> {
    let mut ws = Workspace::new();
    collect(root, root, &mut ws)?;
    Ok(ws)
}

fn collect(base: &Path, dir: &Path, ws: &mut Workspace) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.path());
    for e in entries {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            if matches!(name.as_str(), "target" | ".tach" | ".git") || name.starts_with('.') {
                continue;
            }
            collect(base, &p, ws)?;
        } else if p.extension().map_or(false, |x| x == "tach") {
            let rel = p
                .strip_prefix(base)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            let text = fs::read_to_string(&p)?;
            ws.insert(rel, text);
        }
    }
    Ok(())
}

/// Load a single file as a one-file workspace (keyed by its file name).
pub fn load_single(path: &Path) -> io::Result<Workspace> {
    let mut ws = Workspace::new();
    let key = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    ws.insert(key, fs::read_to_string(path)?);
    Ok(ws)
}

/// Write back every file whose contents changed. Returns the list of changed
/// relative paths.
pub fn write_back(
    root: &Path,
    base: &BTreeMap<String, String>,
    final_files: &BTreeMap<String, String>,
) -> io::Result<Vec<String>> {
    let mut changed = Vec::new();
    for (path, text) in final_files {
        if base.get(path).map(|s| s.as_str()) != Some(text.as_str()) {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&full, text)?;
            changed.push(path.clone());
        }
    }
    changed.sort();
    Ok(changed)
}

/// Scaffold a new project at `root/name`. By default this is the deliberately
/// broken auth demo (so `tach fix` has something to do); `--clean` produces a
/// minimal green project instead.
pub fn scaffold(parent: &Path, name: &str, clean: bool) -> io::Result<PathBuf> {
    let root = parent.join(name);
    fs::create_dir_all(root.join("src"))?;
    fs::create_dir_all(root.join("tests"))?;
    fs::write(root.join("Tach.toml"), manifest(name))?;
    if clean {
        fs::write(root.join("src/main.tach"), CLEAN_MAIN)?;
        fs::write(root.join("tests/main_test.tach"), CLEAN_TEST)?;
    } else {
        fs::write(root.join("src/auth.tach"), DEMO_AUTH)?;
        fs::write(root.join("tests/auth_test.tach"), DEMO_TEST)?;
    }
    Ok(root)
}

fn manifest(name: &str) -> String {
    format!("[project]\nname = \"{}\"\nversion = \"0.1.0\"\n", name)
}

/// The broken demo. Three planted, structurally-repairable bugs:
///   1. `log` is used but never imported          -> E0322 unknown_module
///   2. `load_session` performs undeclared effects -> E0421 effect_undeclared
///   3. `session_summary` returns Int as String    -> E0309 type_mismatch
pub const DEMO_AUTH: &str = r#"// auth.tach — session loading.
//
// This file ships with three planted bugs. Run `tach check` to see them, then
// `tach fix` to watch the compiler-as-agent-harness drive it to green.

import db
import time

type Session = {
  token: String
  user_id: Int
  expires_at: Int
}

fn load_session(token: String) -> Result<Session, AuthError> {
  let row = db.query("select * from sessions where token = ?", token)?
  ensure row.expires_at > time.now()
  log.info("session loaded")
  return Ok(Session { token: row.token, user_id: row.user_id, expires_at: row.expires_at })
}

fn session_summary(s: Session) -> String {
  return s.user_id
}
"#;

pub const DEMO_TEST: &str = r#"// auth_test.tach — these pass once the code compiles cleanly.

import db

test "valid session loads" {
  db.seed("abc", { token: "abc", user_id: 7, expires_at: 9999 })
  ensure load_session("abc").is_ok()
}

test "expired session rejected" {
  db.seed("old", { token: "old", user_id: 7, expires_at: 1 })
  ensure load_session("old").is_err()
}

test "missing session rejected" {
  ensure load_session("nope").is_err()
}
"#;

pub const CLEAN_MAIN: &str = r#"import log

fn greet(name: String) -> String {
  return name
}

fn main() -> String effects [log.write] {
  log.info("tach is online")
  return greet("world")
}
"#;

pub const CLEAN_TEST: &str = r#"test "greet echoes its argument" {
  ensure greet("tach") == "tach"
}
"#;
