//! Tool dispatch: the one place that knows every tool a plan can `call`, which
//! of them are *real* (they touch the world outside the run), and how each is
//! authorized and invoked.
//!
//! Fake tools stay pure deterministic functions in `action.rs`. Real tools
//! route through the same hardened seams the guard harness already uses —
//! `shell::run` is the only process spawner in the codebase — and obey the
//! determinism invariant: their nondeterministic output bytes land on receipts
//! and in `artifacts/`, never in the control-flow event log.

use crate::action;
use crate::goal::GoalSpec;
use crate::shell::{self, ShellRequest};
use crate::store;
use serde_json::{json, Value};
use std::io;
use std::path::Path;

/// Wall-clock default for one real call when the plan does not say
/// `timeout_ms`. Matches the guard's verify timeout: long enough for a test
/// suite, short enough that a hung process cannot wedge a run forever.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Hard ceiling on a per-call `timeout_ms`. A plan cannot opt out of the
/// timeout entirely; the budget system, not the tool call, owns long horizons.
pub const MAX_TIMEOUT_MS: u64 = 600_000;
/// Bytes of stdout inlined into a shell receipt's `output.stdout` (lossy
/// UTF-8). The complete stream is always in the artifact file; the inline
/// prefix exists so plans can branch on output without filesystem reads.
pub const STDOUT_INLINE_CAP: u64 = 16 * 1024;

/// Every tool a plan may name: the deterministic fakes plus the real ones.
pub fn known_tools() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = action::known_tools().to_vec();
    v.push("shell.run");
    v
}

pub fn is_known(tool: &str) -> bool {
    known_tools().contains(&tool)
}

/// A real tool performs an effect outside the run's own store. Its output is
/// nondeterministic, so resume memoizes it from receipts and replay must never
/// re-invoke it.
pub fn is_real(tool: &str) -> bool {
    tool == "shell.run" || tool.starts_with("http.")
}

/// Is this tool granted *at all* by the goal? Fake tools are granted by name
/// in `allow { <tool> }`; `shell.run` is granted by a non-empty
/// `allow { shell.run [...] }` command list (the per-command check is
/// [`authorize`]'s job). Default-deny: an empty grant list grants nothing.
pub fn tool_granted(spec: &GoalSpec, tool: &str) -> bool {
    if tool == "shell.run" {
        !spec.allowed_commands().is_empty()
    } else {
        spec.allowed_tools().contains(tool)
    }
}

/// Validate a real call's input against the goal's authority and the input
/// contract — BEFORE anything fires. `Err` is the refusal reason; the caller
/// records it as an `authority.denied` event and fails the run without any
/// side effect (and without a receipt: a refused call never happened).
pub fn authorize(spec: &GoalSpec, tool: &str, input: &Value) -> Result<(), String> {
    match tool {
        "shell.run" => {
            let cmd = required_str(input, "cmd")?;
            if !spec.command_allowed(cmd) {
                return Err(format!(
                    "command `{cmd}` is not in the goal's `allow {{ shell.run [...] }}` list"
                ));
            }
            if let Some(v) = input.get("cwd") {
                let cwd = v
                    .as_str()
                    .ok_or_else(|| "`cwd` must be a string".to_string())?;
                check_cwd(cwd)?;
            }
            parse_timeout(input)?;
            Ok(())
        }
        other => Err(format!("`{other}` is not a real tool")),
    }
}

/// Invoke a real tool. The caller has already authorized the call and verified
/// no receipt exists for its idempotency key.
///
/// The outer `io::Result` is infrastructure failure. The inner `Result` is the
/// tool-level outcome: `Err` means the effect never fired (e.g. the binary
/// could not be spawned), so the caller may safely re-attempt on a later walk;
/// `Ok(output)` means the effect ran (even if the process exited nonzero —
/// that is an output the plan branches on, not a tool error).
pub fn invoke_real(
    repo: &Path,
    run_id: &str,
    key: &str,
    tool: &str,
    input: &Value,
) -> io::Result<Result<Value, String>> {
    match tool {
        "shell.run" => invoke_shell(repo, run_id, key, input),
        other => Ok(Err(format!("`{other}` is not a real tool"))),
    }
}

fn invoke_shell(
    repo: &Path,
    run_id: &str,
    key: &str,
    input: &Value,
) -> io::Result<Result<Value, String>> {
    // Authorize re-validated these; failures here are defense in depth.
    let cmd = match required_str(input, "cmd") {
        Ok(c) => c,
        Err(e) => return Ok(Err(e)),
    };
    let timeout_ms = match parse_timeout(input) {
        Ok(t) => t,
        Err(e) => return Ok(Err(e)),
    };
    let cwd = match input.get("cwd").and_then(|v| v.as_str()) {
        Some(c) => match check_cwd(c) {
            Ok(()) => repo.join(c),
            Err(e) => return Ok(Err(e)),
        },
        None => repo.to_path_buf(),
    };

    let res = match shell::run(&ShellRequest {
        command: cmd,
        cwd: &cwd,
        timeout_ms,
        artifact_dir: &store::artifacts_dir(repo, run_id),
        key,
        home: &store::sandbox_home(repo),
    }) {
        Ok(r) => r,
        // The spawn itself failed (program not found, artifact dir unwritable):
        // the effect never fired, so this is a tool error, not an output.
        Err(e) => return Ok(Err(format!("`{cmd}` could not be spawned: {e}"))),
    };

    let ok = res.exit_code == Some(0) && !res.timed_out;
    let stdout = inline_prefix(&res.stdout_path, STDOUT_INLINE_CAP);
    Ok(Ok(json!({
        "ok": ok,
        "exit_code": res.exit_code,
        "timed_out": res.timed_out,
        "duration_ms": res.duration_ms,
        "argv": res.argv,
        "program_path": res.program_path,
        "stdout": stdout,
        "stdout_bytes": res.stdout_bytes,
        "stderr_bytes": res.stderr_bytes,
        "stdout_artifact": rel(repo, &res.stdout_path),
        "stderr_artifact": rel(repo, &res.stderr_path),
        "env_redacted": shell::allowed_env_names(),
    })))
}

/// The wall-clock milliseconds a real receipt's effect took, for billing the
/// run's `budget { time: … }`. Durations live on receipts (nondeterministic
/// evidence), so summing them is stable across resume.
pub fn receipt_duration_ms(output: &Value) -> u64 {
    output
        .get("duration_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

fn required_str<'a>(input: &'a Value, field: &str) -> Result<&'a str, String> {
    input
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("`{field}` is required and must be a string"))
}

/// A plan-supplied cwd stays inside the repo: relative, no parent escapes.
fn check_cwd(cwd: &str) -> Result<(), String> {
    let p = Path::new(cwd);
    if p.is_absolute() {
        return Err(format!("`cwd` must be repo-relative, got absolute `{cwd}`"));
    }
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("`cwd` must not contain `..`, got `{cwd}`"));
    }
    Ok(())
}

fn parse_timeout(input: &Value) -> Result<u64, String> {
    match input.get("timeout_ms") {
        None => Ok(DEFAULT_TIMEOUT_MS),
        Some(v) => {
            let t = v
                .as_u64()
                .ok_or_else(|| "`timeout_ms` must be a non-negative integer".to_string())?;
            if t == 0 || t > MAX_TIMEOUT_MS {
                return Err(format!(
                    "`timeout_ms` must be between 1 and {MAX_TIMEOUT_MS}"
                ));
            }
            Ok(t)
        }
    }
}

/// The first `cap` bytes of an artifact, lossy UTF-8. Missing file reads as
/// empty (the artifact is the authority; the inline copy is a convenience).
fn inline_prefix(path: &Path, cap: u64) -> String {
    use std::io::Read;
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let mut buf = Vec::new();
    let _ = f.take(cap).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

fn rel(repo: &Path, p: &Path) -> String {
    p.strip_prefix(repo)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::{AllowSpec, BudgetSpec};

    fn spec_with_shell(cmds: &[&str]) -> GoalSpec {
        GoalSpec {
            name: "G".into(),
            success: None,
            budget: BudgetSpec::default(),
            allow: AllowSpec {
                shell: cmds.iter().map(|s| s.to_string()).collect(),
                ..AllowSpec::default()
            },
            require: vec![],
        }
    }

    #[test]
    fn shell_grant_requires_a_nonempty_command_list() {
        assert!(!tool_granted(&spec_with_shell(&[]), "shell.run"));
        assert!(tool_granted(&spec_with_shell(&["true"]), "shell.run"));
    }

    #[test]
    fn authorize_enforces_the_exact_command_allowlist() {
        let spec = spec_with_shell(&["cargo test"]);
        assert!(authorize(&spec, "shell.run", &json!({ "cmd": "cargo test" })).is_ok());
        // Exact match only — a prefix or variant is a different command.
        assert!(authorize(&spec, "shell.run", &json!({ "cmd": "cargo test --all" })).is_err());
        assert!(authorize(&spec, "shell.run", &json!({})).is_err());
    }

    #[test]
    fn cwd_cannot_escape_the_repo() {
        let spec = spec_with_shell(&["true"]);
        let bad_abs = json!({ "cmd": "true", "cwd": "/etc" });
        let bad_up = json!({ "cmd": "true", "cwd": "a/../../b" });
        let good = json!({ "cmd": "true", "cwd": "sub/dir" });
        assert!(authorize(&spec, "shell.run", &bad_abs).is_err());
        assert!(authorize(&spec, "shell.run", &bad_up).is_err());
        assert!(authorize(&spec, "shell.run", &good).is_ok());
    }

    #[test]
    fn timeout_is_bounded() {
        let spec = spec_with_shell(&["true"]);
        let zero = json!({ "cmd": "true", "timeout_ms": 0 });
        let huge = json!({ "cmd": "true", "timeout_ms": MAX_TIMEOUT_MS + 1 });
        let fine = json!({ "cmd": "true", "timeout_ms": 1000 });
        assert!(authorize(&spec, "shell.run", &zero).is_err());
        assert!(authorize(&spec, "shell.run", &huge).is_err());
        assert!(authorize(&spec, "shell.run", &fine).is_ok());
    }

    #[test]
    fn real_classification_covers_shell_and_http_only() {
        assert!(is_real("shell.run"));
        assert!(is_real("http.get"));
        assert!(!is_real("fake.stripe.refund"));
        assert!(known_tools().contains(&"shell.run"));
    }
}
