//! `perdure serve-mcp` — a SERVER-ONLY Model Context Protocol front door.
//!
//! Exposes Perdure's *existing* safe guard/goal operations as MCP tools over stdio, so
//! an external coding agent (Claude Code, Codex, Cursor-style, local) can drive a
//! guarded repo through structured tool calls instead of shelling out. This is a
//! server only: it never acts as an MCP client, never runs arbitrary shell, and never
//! writes arbitrary files. Every tool maps to a command Perdure already vets — the scope
//! gate, the receipt ledger, and the verify path are unchanged. The agent still makes
//! the edits with its own tools; Perdure remains the guardrail and the ledger.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout (the MCP stdio
//! transport). One JSON object per line; a request without an `id` is a notification
//! and gets no reply. Dependency-free (serde_json only), matching the rest of the
//! crate. The request handler is factored into [`handle`] so it is unit-testable
//! without real pipes.

use crate::{event, guard, runtime, store};
use serde::Serialize;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::Path;

/// The MCP protocol revision we advertise by default. We echo the client's requested
/// version when it sends one, since the tool shapes are version-independent.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Serve the MCP loop on stdin/stdout until stdin closes. `repo` is the working tree
/// (the cwd) the tools operate on.
pub fn serve(repo: &Path) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if let Some(resp) = handle(repo, &line) {
            write_msg(&mut out, &resp)?;
        }
    }
    Ok(())
}

/// Process one JSON-RPC line. Returns the response to write, or `None` for a blank
/// line or a notification (which produce no reply).
pub fn handle(repo: &Path, line: &str) -> Option<Value> {
    if line.trim().is_empty() {
        return None;
    }
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Some(error_envelope(Value::Null, -32700, "parse error")),
    };
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    // No `id` → notification (e.g. notifications/initialized): act if needed, no reply.
    let id = req.get("id").cloned()?;
    let resp = match method {
        "initialize" => result_envelope(id, initialize(&params)),
        "ping" => result_envelope(id, json!({})),
        "tools/list" => result_envelope(id, json!({ "tools": tool_list() })),
        "tools/call" => match call_tool(repo, &params) {
            Ok(value) => result_envelope(id, value),
            Err((code, msg)) => error_envelope(id, code, &msg),
        },
        other => error_envelope(id, -32601, &format!("method not found: {other}")),
    };
    Some(resp)
}

// ----- handshake -----

fn initialize(params: &Value) -> Value {
    let pv = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": pv,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "perdure",
            "title": "Perdure guard",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "Operate a guarded repo from the outside: begin a session, read \
            context/next, make edits with your own tools, verify, then finalize. Perdure is the \
            guardrail and the ledger — it never runs arbitrary shell or writes files for you.",
    })
}

// ----- tools -----

/// The tool catalog. Server-only and safe: guard/goal operations Perdure already vets.
/// No raw shell execution and no arbitrary file writes are exposed.
fn tool_list() -> Vec<Value> {
    let run_id =
        json!({ "type": "string", "description": "Guard run id; defaults to the active session." });
    let tool = |name: &str, desc: &str, props: Value, required: Value| {
        json!({
            "name": name,
            "description": desc,
            "inputSchema": {
                "type": "object",
                "properties": props,
                "required": required,
                "additionalProperties": false,
            }
        })
    };
    vec![
        tool(
            "perdure_guard_begin",
            "Open a guard session over the working tree and return the generic agent context. Mutates: records a new run and a baseline snapshot.",
            json!({ "goal": { "type": "string", "description": "Goal name from the Perdurefile; defaults to the sole goal." } }),
            json!([]),
        ),
        tool(
            "perdure_guard_status",
            "The compact session status (phase, verified bit, command counts). Read-only.",
            json!({ "run_id": run_id }),
            json!([]),
        ),
        tool(
            "perdure_guard_context",
            "The operating contract for a session. With for_agent=generic, the full agent context (allowed files/commands, changes, receipts, next action). Read-only.",
            json!({ "run_id": run_id, "for_agent": { "type": "string", "enum": ["generic"], "description": "Render the full generic agent context." } }),
            json!([]),
        ),
        tool(
            "perdure_guard_next",
            "The single next required action for the agent (what to do now, and the exact next command). Read-only.",
            json!({ "run_id": run_id }),
            json!([]),
        ),
        tool(
            "perdure_guard_diff",
            "The changed files since the baseline, classified in/out of the write scope. Read-only.",
            json!({ "run_id": run_id }),
            json!([]),
        ),
        tool(
            "perdure_guard_verify",
            "Run the goal's required commands and set the verified bit; on refusal, returns machine-actionable repair hints. Mutates: executes the allowlisted commands and mints receipts.",
            json!({ "run_id": run_id, "rerun": { "type": "boolean", "description": "Force a fresh run even if a receipt for the unchanged tree exists." } }),
            json!([]),
        ),
        tool(
            "perdure_guard_finalize",
            "Finalize a verified session into Perdure's ledger. LEDGER-ONLY: never creates a git commit or touches git. Mutates: marks the run completed.",
            json!({ "run_id": run_id }),
            json!([]),
        ),
        tool(
            "perdure_guard_abort",
            "Cancel a guard session. Mutates: marks the run cancelled.",
            json!({ "run_id": run_id }),
            json!([]),
        ),
        tool(
            "perdure_goal_inspect",
            "The goal spec, run state, and full event history for a run. Read-only.",
            json!({ "run_id": run_id }),
            json!([]),
        ),
        tool(
            "perdure_goal_replay",
            "Replay a recorded run and report whether it reproduces. Read-only by default; rerun=true re-executes the required commands.",
            json!({ "run_id": run_id, "rerun": { "type": "boolean", "description": "Re-execute the commands instead of re-deriving from receipts." } }),
            json!([]),
        ),
        tool(
            "perdure_goal_receipts",
            "The durable command receipts minted by a run. Read-only.",
            json!({ "run_id": run_id }),
            json!([]),
        ),
    ]
}

/// Dispatch a `tools/call`. `Err((code, msg))` is a JSON-RPC protocol error (bad
/// method usage); an operational failure (no session, unknown run) is returned as a
/// tool result with `isError: true` so the agent can see and recover from it.
fn call_tool(repo: &Path, params: &Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or((-32602i64, "tools/call is missing `name`".to_string()))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match dispatch(repo, name, &args) {
        None => Err((-32602, format!("unknown tool: {name}"))),
        Some(Ok(value)) => Ok(text_result(value)),
        Some(Err(msg)) => Ok(text_error(&msg)),
    }
}

/// `None` → unknown tool. `Some(Ok)` → a JSON payload. `Some(Err)` → an operational
/// error message surfaced as an `isError` tool result.
fn dispatch(repo: &Path, name: &str, args: &Value) -> Option<Result<Value, String>> {
    match name {
        "perdure_guard_begin" => Some((|| {
            let goal = args.get("goal").and_then(|v| v.as_str());
            let st = guard::begin(repo, goal).map_err(|e| e.to_string())?;
            let _ = store::set_active_guard(repo, &st.run_id);
            jv(guard::agent_context(repo, &st.run_id, "generic"))
        })()),
        "perdure_guard_status" => Some((|| jv(guard::status(repo, &arg_run_id(repo, args)?)))()),
        "perdure_guard_context" => Some((|| {
            let rid = arg_run_id(repo, args)?;
            match args.get("for_agent").and_then(|v| v.as_str()) {
                Some("generic") => jv(guard::agent_context(repo, &rid, "generic")),
                Some(other) => Err(format!("unsupported for_agent `{other}` (only `generic`)")),
                None => jv(guard::context(repo, &rid)),
            }
        })()),
        "perdure_guard_next" => Some((|| jv(guard::next(repo, &arg_run_id(repo, args)?)))()),
        "perdure_guard_diff" => Some((|| jv(guard::diff(repo, &arg_run_id(repo, args)?)))()),
        "perdure_guard_verify" => Some((|| {
            let rid = arg_run_id(repo, args)?;
            let rerun = args.get("rerun").and_then(|v| v.as_bool()).unwrap_or(false);
            jv(guard::verify_report(repo, &rid, rerun))
        })()),
        "perdure_guard_finalize" => Some((|| {
            let rid = arg_run_id(repo, args)?;
            let out = guard::commit(repo, &rid).map_err(|e| e.to_string())?;
            if out.ok {
                store::clear_active_guard(repo);
            }
            jv::<guard::GuardOutcome>(Ok(out))
        })()),
        "perdure_guard_abort" => Some((|| {
            let rid = arg_run_id(repo, args)?;
            let out = guard::abort(repo, &rid).map_err(|e| e.to_string())?;
            store::clear_active_guard(repo);
            jv::<guard::GuardOutcome>(Ok(out))
        })()),
        "perdure_goal_inspect" => Some((|| {
            let rid = arg_run_id(repo, args)?;
            let state = store::load_state(repo, &rid).map_err(|_| format!("no run `{rid}`"))?;
            let events = event::read_all(&store::events_path(repo, &rid)).unwrap_or_default();
            let goal = store::load_goal(repo, &rid).ok().map(|r| r.spec);
            Ok(json!({ "goal": goal, "state": state, "events": events }))
        })()),
        "perdure_goal_replay" => Some((|| {
            let rid = arg_run_id(repo, args)?;
            let rerun = args.get("rerun").and_then(|v| v.as_bool()).unwrap_or(false);
            let r = runtime::replay_run(repo, &rid, rerun).map_err(|e| e.to_string())?;
            Ok(json!({
                "recorded_status": r.recorded_status,
                "replayed_status": r.replayed_status,
                "identical": r.identical,
                "steps": r.steps,
            }))
        })()),
        "perdure_goal_receipts" => Some((|| {
            let rid = arg_run_id(repo, args)?;
            Ok(serde_json::to_value(store::list_receipts(repo, &rid)).unwrap_or(Value::Null))
        })()),
        _ => None,
    }
}

/// The explicit `run_id` argument, else the remembered active guard session.
fn arg_run_id(repo: &Path, args: &Value) -> Result<String, String> {
    if let Some(id) = args.get("run_id").and_then(|v| v.as_str()) {
        return Ok(id.to_string());
    }
    store::active_guard(repo)
        .ok_or_else(|| "no `run_id` given and no active guard session".to_string())
}

fn jv<T: Serialize>(r: io::Result<T>) -> Result<Value, String> {
    r.map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
        .map_err(|e| e.to_string())
}

// ----- JSON-RPC envelopes -----

fn result_envelope(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_envelope(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn text_result(value: Value) -> Value {
    json!({
        "content": [ { "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() } ],
        "isError": false,
    })
}

fn text_error(msg: &str) -> Value {
    json!({ "content": [ { "type": "text", "text": msg } ], "isError": true })
}

fn write_msg<W: Write>(out: &mut W, msg: &Value) -> io::Result<()> {
    let line = serde_json::to_string(msg).unwrap_or_else(|_| "{}".into());
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempRepo(PathBuf);
    impl TempRepo {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "perdure_mcp_{}_{}_{}",
                std::process::id(),
                tag,
                n
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            // A repo whose check passes iff src/lib.rs has FIXED.
            fs::write(dir.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
            fs::write(
                dir.join("Perdurefile"),
                crate::adopt::coding_goal_source(
                    "FixFailingTests",
                    "sh check.sh",
                    &["src/**".to_string(), "tests/**".to_string()],
                ),
            )
            .unwrap();
            fs::write(
                dir.join("check.sh"),
                "grep -q FIXED src/lib.rs && exit 0 || exit 1\n",
            )
            .unwrap();
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::write(dir.join("src/lib.rs"), "// FIXED\n").unwrap();
            TempRepo(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn call(repo: &Path, id: i64, method: &str, params: Value) -> Value {
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        handle(repo, &req.to_string()).expect("a request must get a response")
    }

    /// The text payload of a successful `tools/call`, parsed back to JSON.
    fn tool_json(resp: &Value) -> Value {
        let result = resp.get("result").expect("tool result");
        assert_eq!(result["isError"], json!(false), "tool errored: {result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn initialize_and_list_tools() {
        let r = TempRepo::new("init");
        let init = call(
            r.path(),
            1,
            "initialize",
            json!({ "protocolVersion": "2025-06-18" }),
        );
        assert_eq!(init["result"]["protocolVersion"], json!("2025-06-18"));
        assert_eq!(init["result"]["serverInfo"]["name"], json!("perdure"));

        let list = call(r.path(), 2, "tools/list", json!({}));
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for must in [
            "perdure_guard_begin",
            "perdure_guard_status",
            "perdure_guard_context",
            "perdure_guard_next",
            "perdure_guard_diff",
            "perdure_guard_verify",
            "perdure_guard_finalize",
            "perdure_guard_abort",
            "perdure_goal_inspect",
            "perdure_goal_replay",
            "perdure_goal_receipts",
        ] {
            assert!(names.contains(&must), "missing tool {must}");
        }
        // No raw shell / file-write tool is ever exposed.
        assert!(!names
            .iter()
            .any(|n| n.contains("shell") || n.contains("write") || n.contains("exec")));
    }

    #[test]
    fn notifications_get_no_reply() {
        let r = TempRepo::new("notif");
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle(r.path(), &note.to_string()).is_none());
        assert!(handle(r.path(), "   ").is_none());
    }

    #[test]
    fn drive_a_full_session_through_tools() {
        let r = TempRepo::new("session");

        // begin → an agent context with a run_id.
        let begun = tool_json(&call(
            r.path(),
            10,
            "tools/call",
            json!({ "name": "perdure_guard_begin", "arguments": { "goal": "FixFailingTests" } }),
        ));
        let run_id = begun["run_id"].as_str().unwrap().to_string();
        assert_eq!(begun["agent"], json!("generic"));

        // next → finalize (the tree already satisfies the check) or run_verify.
        let next = tool_json(&call(
            r.path(),
            11,
            "tools/call",
            json!({ "name": "perdure_guard_next", "arguments": {} }),
        ));
        assert!(next["next_action"].is_string());

        // verify → verified true, with a receipt.
        let verify = tool_json(&call(
            r.path(),
            12,
            "tools/call",
            json!({ "name": "perdure_guard_verify", "arguments": { "run_id": run_id } }),
        ));
        assert_eq!(verify["verified"], json!(true), "verify result: {verify}");

        // receipts → at least one durable command receipt.
        let receipts = tool_json(&call(
            r.path(),
            13,
            "tools/call",
            json!({ "name": "perdure_goal_receipts", "arguments": { "run_id": run_id } }),
        ));
        assert!(!receipts.as_array().unwrap().is_empty());

        // finalize → ledger-only completion.
        let fin = tool_json(&call(
            r.path(),
            14,
            "tools/call",
            json!({ "name": "perdure_guard_finalize", "arguments": { "run_id": run_id } }),
        ));
        assert_eq!(fin["ok"], json!(true), "finalize: {fin}");
        // No git repository was created by finalizing.
        assert!(!r.path().join(".git").exists());
    }

    #[test]
    fn unknown_tool_and_method_are_reported() {
        let r = TempRepo::new("errs");
        // Unknown tool → protocol error.
        let bad = call(
            r.path(),
            20,
            "tools/call",
            json!({ "name": "perdure_run_shell", "arguments": {} }),
        );
        assert_eq!(bad["error"]["code"], json!(-32602));

        // Unknown method → method-not-found.
        let m = call(r.path(), 21, "frobnicate", json!({}));
        assert_eq!(m["error"]["code"], json!(-32601));

        // Operational failure (no active session) → isError tool result, not a crash.
        let op = call(
            r.path(),
            22,
            "tools/call",
            json!({ "name": "perdure_guard_status", "arguments": {} }),
        );
        assert_eq!(op["result"]["isError"], json!(true));
    }
}
