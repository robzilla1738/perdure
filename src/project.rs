//! Project I/O: discovering `.pdr` files into a `Workspace`, writing verified
//! results back, and scaffolding new projects with `perdure new`.

use crate::diagnostics::Diagnostic;
use crate::patch::Workspace;
use crate::program::Program;
use crate::source::SourceFile;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The conventional name of a coding goal file at a real repo's root. Unlike toy
/// `.pdr` source, a `Perdurefile` is the one place a coding goal lives, so a real
/// Rust/JS repo (which has no `.pdr` files) is still adoptable. Written by
/// `perdure init --existing` and parsed with the same goal grammar.
pub const TACHFILE: &str = "Perdurefile";

/// Load and parse the repo's `Perdurefile`, if present, into a `Program` plus its
/// parse diagnostics. Used by the guard runtime to resolve a coding goal — it does
/// not go through `load_workspace`, which only sees `*.pdr`.
pub fn load_goal_file(repo: &Path) -> io::Result<(Program, Vec<Diagnostic>)> {
    let path = repo.join(TACHFILE);
    let text = fs::read_to_string(&path)?;
    Ok(Program::parse_sources(vec![SourceFile::new(
        TACHFILE.to_string(),
        text,
    )]))
}

/// Load every `.pdr` file under `root` into a workspace, keyed by path relative
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
            if matches!(name.as_str(), "target" | ".perdure" | ".git") || name.starts_with('.') {
                continue;
            }
            collect(base, &p, ws)?;
        } else if p.extension().is_some_and(|x| x == "pdr") {
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

/// Load every immediate subdirectory of `dir` as its own project workspace,
/// returned sorted by case name. Subdirectories with no `.pdr` files are
/// skipped. Used by `perdure bench --suite` to run the repair corpus.
pub fn load_suite(dir: &Path) -> io::Result<Vec<(String, Workspace)>> {
    let mut cases = Vec::new();
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.path());
    for e in entries {
        let p = e.path();
        if p.is_dir() {
            let ws = load_workspace(&p)?;
            if !ws.files.is_empty() {
                let name = e.file_name().to_string_lossy().to_string();
                cases.push((name, ws));
            }
        }
    }
    Ok(cases)
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
/// broken auth demo (so `perdure fix` has something to do); `--clean` produces a
/// minimal green project, and `plan_template` (e.g. `chargebacks`) produces a
/// workspace-authored plan goal ready to `perdure goal run`.
pub fn scaffold(
    parent: &Path,
    name: &str,
    clean: bool,
    plan_template: Option<&str>,
) -> io::Result<PathBuf> {
    let root = parent.join(name);
    if let Some(tmpl) = plan_template {
        let src = match tmpl {
            "chargebacks" => PLAN_DEMO_CHARGEBACKS,
            "shell" => PLAN_DEMO_SHELL,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown goal template `{other}` (try `chargebacks` or `shell`)"),
                ))
            }
        };
        fs::create_dir_all(&root)?;
        fs::write(root.join("Perdure.toml"), manifest(name))?;
        fs::write(root.join("goal.pdr"), src)?;
        return Ok(root);
    }
    fs::create_dir_all(root.join("src"))?;
    fs::create_dir_all(root.join("tests"))?;
    fs::write(root.join("Perdure.toml"), manifest(name))?;
    if clean {
        fs::write(root.join("src/main.pdr"), CLEAN_MAIN)?;
        fs::write(root.join("tests/main_test.pdr"), CLEAN_TEST)?;
    } else {
        fs::write(root.join("src/auth.pdr"), DEMO_AUTH)?;
        fs::write(root.join("tests/auth_test.pdr"), DEMO_TEST)?;
        fs::write(root.join("goal.pdr"), DEMO_GOAL)?;
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
pub const DEMO_AUTH: &str = r#"// auth.pdr — session loading.
//
// This file ships with three planted bugs. Run `perdure check` to see them, then
// `perdure fix` to watch the compiler-as-agent-harness drive it to green.

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

pub const DEMO_TEST: &str = r#"// auth_test.pdr — these pass once the code compiles cleanly.

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

/// The demo's goal: a durable, budgeted, authority-scoped contract for driving
/// the planted-bug project from red to green. `perdure goal run FixFailingTests`
/// executes the same repair loop as `perdure fix`, but checkpointed and resumable,
/// and refuses any patch that would write outside `src/**`/`tests/**` or perform
/// an effect it was never granted. Written in canonical form so `perdure fmt` is a
/// no-op on a fresh project.
pub const DEMO_GOAL: &str = r#"goal FixFailingTests -> Success {
  budget {
    steps: 30
    retries: 3
  }
  allow {
    effect db.read
    effect db.write
    effect time.read
    effect log.write
    fs.write ["src/**", "tests/**"]
  }
  require {
    tests.pass
    no_new_effects
  }
}
"#;

/// A workspace-authored plan goal (`perdure new <name> --goal chargebacks`). It is a
/// durable workflow, not a repair run: it lists a customer's disputed charges,
/// loops over them, and refunds the duplicates behind a per-charge approval gate —
/// exactly the shape of the built-in `ReconcileChargebacks`, but living in the
/// user's own file under a distinct name, so it runs entirely off the binary's
/// catalog and resumes/replays from the source snapshot. Written canonically so
/// `perdure fmt` is a no-op.
pub const PLAN_DEMO_CHARGEBACKS: &str = r#"goal ReconcileLocalDemo -> Success {
  budget {
    steps: 60
  }
  allow {
    fake.stripe.list_disputes
    fake.stripe.refund
    fake.email.send
    fake.zendesk.comment
  }
  require {
    refunds.receipted
  }
  plan {
    let disputes = call fake.stripe.list_disputes {
      customer: "cus_42"
    }
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
}
"#;

/// A workspace-authored plan goal that runs REAL commands
/// (`perdure new <name> --goal shell`). The smallest demonstration of the
/// centerpiece guarantees: the `allow` list is the only authority (an
/// unlisted command is refused before it spawns), every execution commits a
/// receipt (crash + resume never reruns a finished command), the follow-up
/// is gated on human approval, and `goal replay` re-walks the plan from
/// receipts without spawning anything. Written canonically so `perdure fmt`
/// is a no-op.
pub const PLAN_DEMO_SHELL: &str = r#"goal SmokeCheck -> Success {
  budget {
    steps: 10
    time: 15m
  }
  allow {
    shell.run ["echo perdure says hello", "uname"]
  }
  require {
    commands.receipted
  }
  plan {
    let hello = call shell.run {
      cmd: "echo perdure says hello"
    }
    if hello.ok {
      approve "also record this machine's kernel name" {
        call shell.run {
          cmd: "uname"
        }
      }
    }
  }
}
"#;

pub const CLEAN_MAIN: &str = r#"import log

fn greet(name: String) -> String {
  return name
}

fn main() -> String effects [log.write] {
  log.info("perdure is online")
  return greet("world")
}
"#;

pub const CLEAN_TEST: &str = r#"test "greet echoes its argument" {
  ensure greet("perdure") == "perdure"
}
"#;
