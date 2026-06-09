//! Adopting an existing repo: `tach init --existing` and `tach goal init`.
//!
//! `tach new` scaffolds a fresh toy-language project. This is the opposite move:
//! take a real Rust/JS/Go/Python repo and lay a Tach control plane *over* it
//! without touching a line of its source. We write three small files —
//!
//!   * `Tachfile`      — the coding goal (authority + the command that proves it)
//!   * `TACH_AGENT.md` — the operating contract an external agent reads
//!   * `AGENTS.md`     — the generic, vendor-neutral contract (only if absent)
//!   * `.tachignore`   — what the snapshot/diff gate skips
//!
//! and detect the project's test command so the goal is useful out of the box.

use crate::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// What we learned by sniffing a repo's build manifests.
#[derive(Clone, Debug)]
pub struct Detected {
    /// The test command, e.g. `cargo test`.
    pub command: String,
    /// A typecheck/compile command if one is obvious, e.g. `cargo check`.
    pub typecheck: Option<String>,
    /// A short ecosystem label for messages: `rust`, `javascript`, `go`, `python`.
    pub ecosystem: &'static str,
    /// A sensible default `fs.write` scope for this ecosystem.
    pub write_globs: Vec<String>,
}

/// Best-effort detection of how this repo is built and tested. Ordered: the first
/// manifest that matches wins. `None` means we couldn't tell — the caller still
/// writes the files, but with a placeholder command and a warning.
pub fn detect(repo: &Path) -> Option<Detected> {
    let has = |f: &str| repo.join(f).exists();
    if has("Cargo.toml") {
        return Some(Detected {
            command: "cargo test".into(),
            typecheck: Some("cargo check".into()),
            ecosystem: "rust",
            write_globs: vec!["src/**".into(), "tests/**".into()],
        });
    }
    if has("package.json") {
        let pm = detect_js_pm(repo);
        return Some(Detected {
            command: js_test_command(pm),
            typecheck: Some(format!("{pm} run typecheck")),
            ecosystem: "javascript",
            write_globs: vec!["src/**".into(), "test/**".into(), "tests/**".into()],
        });
    }
    if has("go.mod") {
        return Some(Detected {
            command: "go test ./...".into(),
            typecheck: Some("go build ./...".into()),
            ecosystem: "go",
            write_globs: vec!["**/*.go".into()],
        });
    }
    if has("pyproject.toml") || has("pytest.ini") || has("setup.cfg") || has("tox.ini") {
        return Some(Detected {
            command: "pytest".into(),
            typecheck: None,
            ecosystem: "python",
            write_globs: vec!["src/**".into(), "tests/**".into()],
        });
    }
    None
}

/// The JS package manager, inferred from the lockfile present.
fn detect_js_pm(repo: &Path) -> &'static str {
    if repo.join("bun.lockb").exists() || repo.join("bun.lock").exists() {
        "bun"
    } else if repo.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if repo.join("yarn.lock").exists() {
        "yarn"
    } else {
        "npm"
    }
}

fn js_test_command(pm: &str) -> String {
    match pm {
        // bun has a native test runner; the others delegate to the `test` script.
        "bun" => "bun test".into(),
        other => format!("{other} test"),
    }
}

/// The default `Detected` used when nothing matched, so adoption still produces a
/// well-formed Tachfile (with a clearly-marked placeholder command to fill in).
fn placeholder() -> Detected {
    Detected {
        command: "echo set-your-test-command".into(),
        typecheck: None,
        ecosystem: "unknown",
        write_globs: vec!["src/**".into(), "tests/**".into()],
    }
}

/// Map a goal template id (the CLI argument) to a goal name and the command that
/// must pass for it. `coding.fix-tests` is the headline; `coding.typecheck` is a
/// thin variant. The goal *name* is always a plain identifier so the existing
/// `goal <Ident>` grammar is untouched.
fn template(goal_id: &str, d: &Detected) -> io::Result<(&'static str, String)> {
    match goal_id {
        "coding.fix-tests" | "fix-tests" => Ok(("FixFailingTests", d.command.clone())),
        "coding.typecheck" | "typecheck" => {
            let cmd = d.typecheck.clone().unwrap_or_else(|| d.command.clone());
            Ok(("Typecheck", cmd))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown coding goal `{other}` (try `coding.fix-tests` or `coding.typecheck`)"),
        )),
    }
}

/// Render the `Tachfile` for a goal, in canonical (fmt) form so `tach fmt` is a
/// no-op on it. Built from a draft and normalized by the formatter so glob lists
/// and spacing always match the one canonical spelling.
pub fn coding_goal_source(name: &str, command: &str, write_globs: &[String]) -> String {
    let writes = write_globs
        .iter()
        .map(|g| format!("\"{g}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let draft = format!(
        "goal {name} -> Success {{\n  \
         budget {{\n    steps: 40\n  }}\n  \
         allow {{\n    fs.write [{writes}]\n    shell.run [\"{command}\"]\n  }}\n  \
         require {{\n    command(\"{command}\").passes\n    no_out_of_scope_writes\n  }}\n}}\n"
    );
    // Normalize through the formatter; on the off chance it can't parse, the draft
    // is already close to canonical.
    fmt::format_file("Tachfile", &draft).unwrap_or(draft)
}

/// The operating contract written to `TACH_AGENT.md`. Deliberately blunt about the
/// one rule that matters: never claim done unless Tach says `verified: true`.
pub fn tach_agent_md(goal_name: &str, command: &str) -> String {
    format!(
        r#"# Tach guard — operating contract for AI coding agents

This repository is operated through **Tach**: a runtime that scopes, verifies, and
records your work. You bring the reasoning and the edits; Tach is the guardrail and
the durable ledger. Follow this contract.

## Open a session
    tach guard begin {goal_name}
    tach guard context --json                      # the contract for this run
    tach guard context --for-agent generic --json  # the full agent packet (changes, receipts, next move)
    tach guard next --json                          # just the single next required action

`context` reports:
  - allowed_files     — globs you may edit; edits elsewhere are rejected at the gate
  - allowed_commands  — the only commands Tach will run for you
  - current_failure   — what is failing right now (or null)
  - next_required_action

## While you work
  - Edit only files matching `allowed_files`. An edit outside that scope is an
    out-of-scope write: Tach records it and it will block the commit. Tach detects
    and rejects such edits at the gate — it does not silently prevent the write, so
    staying in scope is on you.
  - Run project commands through `tach guard verify`, not directly, so each run is
    captured as a receipt.

## Before you claim done
  - Run `tach guard verify` and read the JSON.
  - **Do not tell the user the task is done unless Tach reports `verified: true`.**
    `verified` is true only when every required command passed AND no out-of-scope
    file changed. The `done_condition` field of `context --json` states this check.
  - Finish with `tach guard finalize` (alias: `tach guard commit`). This finalizes the
    run into **Tach's own ledger only — it does not create a git commit or touch git in
    any way.** Staging, committing, or pushing to git is yours to do (or not), separately.
  - If `finalize` refuses, run `tach guard diff --json`, fix the violations, and verify
    again.

Goal: {goal_name}
Verified by: `{command}`
"#
    )
}

/// The marker on the first line of a Tach-generated `AGENTS.md`. Its presence is
/// how we tell "this AGENTS.md is ours and `--force` may regenerate it" from a
/// user-authored one, which we must never touch.
pub const AGENTS_SENTINEL: &str = "<!-- tach:agents-contract v1 -->";

/// The generic, vendor-neutral operating contract written to `AGENTS.md` — the file
/// many coding agents read by convention. Unlike `TACH_AGENT.md` (Tach-owned), this
/// is the shared community file, so it is only ever written when absent, and only
/// overwritten by `--force` when it still carries our sentinel.
pub fn tach_agents_md(goal_name: &str, command: &str) -> String {
    format!(
        r#"{AGENTS_SENTINEL}
# Agent operating contract

This repository is governed by **Tach**: a runtime that scopes, verifies, and records
the work of AI coding agents. Tach does not write code — you do. Tach is the guardrail
and the durable ledger. Any agent operating here follows this contract.

## Before you edit
Read the machine-readable contract for the active session:

    tach guard context --for-agent generic --json

It reports `allowed_files` (what you may edit), `allowed_commands` (what Tach will run),
`required_commands`, the current failure, the `done_condition`, and the single
`next_action`. If no session is open yet, open one:

    tach guard begin {goal_name}

To ask only "what do I do next?":

    tach guard next --json

## While you work
  - Edit only files matching `allowed_files`. An edit outside that scope is an
    out-of-scope write: Tach detects and rejects it at the gate and it blocks
    finalization. Tach does not silently prevent the write, so staying in scope is on you.
  - Run project commands through `tach guard verify`, never directly, so each run is
    captured as a receipt with its stdout/stderr.

## Before you claim done
  - Run `tach guard verify` and read the JSON (or `tach guard status --json`).
  - **Do not tell the user the task is done unless Tach reports `verified: true`.** That
    bit is true only when every required command passed AND no out-of-scope file changed.
  - If verify is refused, its JSON carries `rejection` (what to fix — `scope_violations`
    lists the exact paths) and `recommended_command` (what to run next).

## Finalize
    tach guard finalize        # alias: tach guard commit

**Finalization is ledger-only — it does not create a git commit or touch git in any
way.** Staging, committing, or pushing to git is yours to do (or not), separately.

---
Generated by `tach init --existing`. Goal: {goal_name}; verified by `{command}`.
Tach-specific operating details live in `TACH_AGENT.md`.
"#
    )
}

/// What `.tachignore` ships with: heavy build/dependency roots the snapshot gate
/// should never walk. (Dotdirs and `.tach/` are always skipped regardless.)
pub const DEFAULT_TACHIGNORE: &str = "# Paths the Tach scope/diff gate ignores.\n\
    target\n\
    node_modules\n\
    dist\n\
    build\n\
    .venv\n\
    __pycache__\n\
    *.log\n";

/// Write the Tachfile for `goal_id`, detecting the test command. Overwrites an
/// existing Tachfile (the goal definition is meant to be regenerated/edited).
pub fn goal_init(repo: &Path, goal_id: &str) -> io::Result<(PathBuf, String, String)> {
    let d = detect(repo).unwrap_or_else(placeholder);
    let (name, command) = template(goal_id, &d)?;
    let src = coding_goal_source(name, &command, &d.write_globs);
    let path = repo.join("Tachfile");
    fs::write(&path, &src)?;
    Ok((path, name.to_string(), command))
}

/// The result of adopting a repo: which files were written and which were left
/// alone (already present, and `--force` not given).
pub struct InitReport {
    pub written: Vec<PathBuf>,
    pub skipped: Vec<PathBuf>,
    pub detected: Option<Detected>,
    pub goal_name: String,
    pub command: String,
}

/// Decide whether `init --existing` should (over)write `AGENTS.md`. Returns `true`
/// to write it: when it is absent, or when `--force` is given *and* the existing file
/// still carries our sentinel (i.e. it is a previously-generated Tach file). A
/// user-authored `AGENTS.md` (no sentinel) is never touched, even with `--force`.
fn write_agents_md(path: &Path, force: bool) -> io::Result<bool> {
    if !path.exists() {
        return Ok(true);
    }
    if !force {
        return Ok(false);
    }
    let existing = fs::read_to_string(path).unwrap_or_default();
    Ok(existing.contains(AGENTS_SENTINEL))
}

/// Adopt an existing repo: write `Tachfile`, `TACH_AGENT.md`, `AGENTS.md`, and
/// `.tachignore`. Never scaffolds source. Tach-owned files are overwritten by
/// `--force`; a user-authored `AGENTS.md` is never touched.
pub fn init_existing(repo: &Path, force: bool) -> io::Result<InitReport> {
    let detected = detect(repo);
    let d = detected.clone().unwrap_or_else(placeholder);
    let (goal_name, command) = template("coding.fix-tests", &d)?;

    let mut written = Vec::new();
    let mut skipped = Vec::new();

    let tachfile = repo.join("Tachfile");
    if tachfile.exists() && !force {
        skipped.push(tachfile);
    } else {
        fs::write(
            &tachfile,
            coding_goal_source(goal_name, &command, &d.write_globs),
        )?;
        written.push(tachfile);
    }

    // `TACH_AGENT.md` is Tach-owned: written fresh, overwritten by `--force`.
    let agent = repo.join("TACH_AGENT.md");
    if agent.exists() && !force {
        skipped.push(agent);
    } else {
        fs::write(&agent, tach_agent_md(goal_name, &command))?;
        written.push(agent);
    }

    // `AGENTS.md` is the shared community contract many agents read by convention.
    // We generate it when absent, but never clobber a user-authored one: `--force`
    // regenerates it only if it still carries our sentinel.
    let agents = repo.join("AGENTS.md");
    if write_agents_md(&agents, force)? {
        fs::write(&agents, tach_agents_md(goal_name, &command))?;
        written.push(agents);
    } else {
        skipped.push(agents);
    }

    let ignore = repo.join(".tachignore");
    if ignore.exists() && !force {
        skipped.push(ignore);
    } else {
        fs::write(&ignore, DEFAULT_TACHIGNORE)?;
        written.push(ignore);
    }

    Ok(InitReport {
        written,
        skipped,
        detected,
        goal_name: goal_name.to_string(),
        command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempRepo(PathBuf);
    impl TempRepo {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "tach_adopt_{}_{}_{}",
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
        fn touch(&self, rel: &str, text: &str) {
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

    #[test]
    fn detects_rust_and_js_and_go() {
        let r = TempRepo::new("rust");
        r.touch("Cargo.toml", "[package]\nname=\"x\"\n");
        assert_eq!(detect(r.path()).unwrap().command, "cargo test");

        let j = TempRepo::new("js");
        j.touch("package.json", "{}");
        j.touch("bun.lockb", "");
        assert_eq!(detect(j.path()).unwrap().command, "bun test");

        let j2 = TempRepo::new("js2");
        j2.touch("package.json", "{}"); // no lockfile → npm
        assert_eq!(detect(j2.path()).unwrap().command, "npm test");

        let g = TempRepo::new("go");
        g.touch("go.mod", "module x\n");
        assert_eq!(detect(g.path()).unwrap().command, "go test ./...");
    }

    #[test]
    fn generated_tachfile_is_canonical_and_parses() {
        let d = Detected {
            command: "cargo test".into(),
            typecheck: None,
            ecosystem: "rust",
            write_globs: vec!["src/**".into(), "tests/**".into()],
        };
        let src = coding_goal_source("FixFailingTests", &d.command, &d.write_globs);
        // A fmt fixed point.
        assert_eq!(
            fmt::format_file("Tachfile", &src).unwrap(),
            src,
            "generated Tachfile is not canonical"
        );
        // Parses + checks cleanly (no E0431 on the command require form).
        let (prog, pdiags) =
            crate::program::Program::parse_sources(vec![crate::source::SourceFile::new(
                "Tachfile", src,
            )]);
        assert!(pdiags.iter().all(|x| !x.is_error()), "parse: {pdiags:?}");
        let cdiags = crate::check::check_program(&prog);
        assert!(
            cdiags.iter().all(|x| !x.is_error()),
            "check errors: {cdiags:?}"
        );
        let g = crate::goal::find_goal(&prog, "FixFailingTests").expect("goal");
        let spec = crate::goal::GoalSpec::from_decl(g);
        assert_eq!(spec.allowed_commands(), &["cargo test".to_string()]);
        assert_eq!(spec.required_commands(), vec!["cargo test"]);
    }

    #[test]
    fn init_existing_writes_four_files_and_respects_force() {
        let r = TempRepo::new("init");
        r.touch("Cargo.toml", "[package]\nname=\"x\"\n");
        let rep = init_existing(r.path(), false).unwrap();
        assert_eq!(rep.written.len(), 4);
        assert!(r.path().join("Tachfile").exists());
        assert!(r.path().join("TACH_AGENT.md").exists());
        assert!(r.path().join("AGENTS.md").exists());
        assert!(r.path().join(".tachignore").exists());
        // No toy source scaffolded.
        assert!(!r.path().join("src/main.tach").exists());

        // Re-running without --force skips all four.
        let rep2 = init_existing(r.path(), false).unwrap();
        assert_eq!(rep2.written.len(), 0);
        assert_eq!(rep2.skipped.len(), 4);
    }

    #[test]
    fn agents_md_is_generated_but_never_clobbers_a_user_file() {
        // Absent → generated, with the sentinel and the generic contract.
        let r = TempRepo::new("agents");
        r.touch("Cargo.toml", "[package]\nname=\"x\"\n");
        init_existing(r.path(), false).unwrap();
        let generated = fs::read_to_string(r.path().join("AGENTS.md")).unwrap();
        assert!(generated.contains(AGENTS_SENTINEL));
        assert!(generated.contains("tach guard context --for-agent generic --json"));
        assert!(generated.contains("ledger-only") || generated.contains("ledger"));

        // --force regenerates our own (sentinel-bearing) file.
        let rep = init_existing(r.path(), true).unwrap();
        assert!(rep.written.iter().any(|p| p.ends_with("AGENTS.md")));

        // A user-authored AGENTS.md (no sentinel) is never touched, even with --force.
        let u = TempRepo::new("agents_user");
        u.touch("Cargo.toml", "[package]\nname=\"x\"\n");
        u.touch("AGENTS.md", "user owned — do not touch");
        let rep = init_existing(u.path(), true).unwrap();
        assert_eq!(
            fs::read_to_string(u.path().join("AGENTS.md")).unwrap(),
            "user owned — do not touch"
        );
        assert!(rep.skipped.iter().any(|p| p.ends_with("AGENTS.md")));
    }
}
