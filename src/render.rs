//! Human-facing rendering. Everything an agent consumes is JSON elsewhere; this
//! module is purely the pretty, colored view for people.

use crate::agent::{FixOutcome, Lap, RaceOutcome};
use crate::ast::Item;
use crate::check;
use crate::diagnostics::{Diagnostic, Severity};
use crate::patch::Workspace;
use crate::program::Program;
use crate::runner::TestReport;
use crate::source::SourceFile;
use crate::{builtins, term};

/// Format a microsecond duration as `µs` or `ms` depending on magnitude.
pub fn fmt_duration(us: u64) -> String {
    if us >= 1000 {
        format!("{:.1}ms", us as f64 / 1000.0)
    } else {
        format!("{}µs", us)
    }
}

fn dot(status: &str) -> String {
    match status {
        "green" => term::bold_green("●"),
        "stuck" => term::bold_red("●"),
        _ => term::bold_yellow("●"),
    }
}

/// A colored status dot for a goal run's lifecycle status.
pub fn status_dot(status: &str) -> String {
    match status {
        "completed" => term::bold_green("●"),
        "failed" | "budget_exhausted" | "denied" => term::bold_red("●"),
        "cancelled" => term::dim("●"),
        // running / awaiting_approval and anything else: in-flight.
        _ => term::bold_yellow("●"),
    }
}

/// The summary block for a goal run: its identity, status, and agent-loop metrics.
pub fn run_state(s: &crate::store::RunState) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} {}  {}\n",
        status_dot(&s.status),
        term::bold(&s.goal),
        term::dim(&s.run_id)
    ));
    let row = |k: &str, v: String| format!("  {:<20} {}\n", k, v);
    out.push_str(&row("status", s.status.clone()));
    if s.kind == "plan" {
        // A plan run's progress is its tool calls and receipts, recomputed by
        // re-execution; there is no linear step cursor to report.
        out.push_str(&row("tool-calls", s.cursor.to_string()));
        out.push_str(&row("receipts", s.receipts_created.to_string()));
        if let Some(a) = &s.pending_approval {
            out.push_str(&row("awaiting-approval", a.clone()));
        }
        return out;
    }
    out.push_str(&row("steps", s.step.to_string()));
    if s.kind == "action" {
        out.push_str(&row("actions-executed", s.actions_executed.to_string()));
        out.push_str(&row("cursor", s.cursor.to_string()));
        out.push_str(&row("receipts", s.receipts_created.to_string()));
        if let Some(a) = &s.pending_approval {
            out.push_str(&row("awaiting-approval", a.clone()));
        }
    } else {
        out.push_str(&row("patches-applied", s.patches_applied.to_string()));
        out.push_str(&row("patches-rejected", s.patches_rejected.to_string()));
        out.push_str(&row("tests-run", s.tests_run.to_string()));
        out.push_str(&row("regressions", s.regressions.to_string()));
        out.push_str(&row("diff-size", format!("{} chars", s.diff_chars)));
        out.push_str(&row(
            "tests",
            format!("{} passed, {} failed", s.tests_passed, s.tests_failed),
        ));
    }
    out
}

/// A compact timeline of a run's event history.
pub fn events(evs: &[crate::event::Event]) -> String {
    if evs.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(&format!("\n{}\n", term::bold("event history")));
    for e in evs {
        let detail = event_detail(e);
        out.push_str(&format!(
            "  {:>4}  {:<22} {}\n",
            term::dim(&e.seq.to_string()),
            event_kind_colored(&e.kind),
            term::dim(&detail)
        ));
    }
    out
}

fn event_kind_colored(kind: &str) -> String {
    match kind {
        "patch.applied" | "run.completed" | "patch.verified" | "approval.granted"
        | "tool.completed" | "receipt.created" => term::green(kind),
        "patch.rejected" | "run.failed" | "budget.exhausted" | "approval.denied"
        | "tool.failed" => term::red(kind),
        "effect.delta_detected"
        | "run.resumed"
        | "approval.requested"
        | "action.proposed"
        | "receipt.reused"
        | "action.skipped" => term::yellow(kind),
        _ => kind.to_string(),
    }
}

/// A one-line human gloss of an event's payload — best-effort, never panics.
fn event_detail(e: &crate::event::Event) -> String {
    let p = &e.payload;
    let s = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    match e.kind.as_str() {
        "run.started" => {
            let k = s("kind");
            if k.is_empty() {
                format!("goal {} · strategy {}", s("goal"), s("strategy"))
            } else {
                format!("goal {} · {}", s("goal"), k)
            }
        }
        "action.proposed" => {
            let summary = s("summary");
            if summary.is_empty() {
                s("tool")
            } else {
                format!("{} · {}", s("tool"), summary)
            }
        }
        "approval.requested" => format!("{} for {}", s("approval_id"), s("action")),
        "approval.granted" | "approval.denied" => {
            format!("{} ({})", s("approval_id"), s("action"))
        }
        "tool.called" => format!("{} · {}", s("tool"), s("action")),
        "tool.completed" => s("tool"),
        "tool.failed" => format!("{}: {}", s("tool"), s("error")),
        "receipt.created" => format!("{} · {}", s("receipt_id"), s("tool")),
        "receipt.reused" => format!("{} (idempotent — no re-call)", s("receipt_id")),
        "action.skipped" => format!("{} — {}", s("action"), s("reason")),
        "diagnostic.emitted" => format!("{} {}", s("code"), s("kind")),
        "patch.proposed" => s("name"),
        "patch.applied" => s("patch"),
        "patch.rejected" => {
            let n = p
                .get("rejections")
                .and_then(|r| r.as_array())
                .map_or(0, |a| a.len());
            format!("{} ({} reason(s))", s("patch"), n)
        }
        "checkpoint.written" => format!(
            "step {}",
            p.get("step").and_then(|v| v.as_u64()).unwrap_or(0)
        ),
        "test.completed" => format!(
            "{} passed, {} failed",
            p.get("passed").and_then(|v| v.as_u64()).unwrap_or(0),
            p.get("failed").and_then(|v| v.as_u64()).unwrap_or(0)
        ),
        "effect.delta_detected" => {
            let effs = p
                .get("new_effects")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            effs
        }
        "run.failed" => s("reason"),
        "run.resumed" => format!(
            "from step {}",
            p.get("from_step").and_then(|v| v.as_u64()).unwrap_or(0)
        ),
        _ => String::new(),
    }
}

/// Render a list of diagnostics with source context and the agent-repair hint.
pub fn diagnostics(diags: &[Diagnostic], ws: &Workspace) -> String {
    let mut out = String::new();
    for d in diags {
        out.push_str(&one(d, ws));
        out.push('\n');
    }
    out
}

fn one(d: &Diagnostic, ws: &Workspace) -> String {
    let mut out = String::new();
    let tag = match d.severity {
        Severity::Error => term::bold_red(&format!("error[{}]", d.code)),
        Severity::Warning => term::bold_yellow(&format!("warning[{}]", d.code)),
        Severity::Note => term::bold_cyan(&format!("note[{}]", d.code)),
    };
    out.push_str(&format!("{}: {}\n", tag, term::bold(&d.message)));

    if let Some(src_text) = ws.files.get(&d.file) {
        let src = SourceFile::new(d.file.clone(), src_text.clone());
        let (line, col) = src.line_col(d.span.start);
        out.push_str(&format!(
            "  {} {}:{}:{}\n",
            term::dim("-->"),
            d.file,
            line,
            col
        ));
        let line_text = src.line_text(line);
        out.push_str(&format!("{:>4} {} {}\n", line, term::dim("|"), line_text));
        let line_chars = line_text.chars().count();
        let avail = line_chars.saturating_sub(col.saturating_sub(1)).max(1);
        let ul = d.span.len().clamp(1, avail);
        let caret = format!(
            "{}{}",
            " ".repeat(col.saturating_sub(1)),
            term::red(&"^".repeat(ul))
        );
        out.push_str(&format!("{:>4} {} {}\n", "", term::dim("|"), caret));
    } else {
        out.push_str(&format!("  {} {}\n", term::dim("-->"), d.file));
    }

    for n in &d.notes {
        out.push_str(&format!(
            "     {} {}\n",
            term::dim("="),
            term::dim(&format!("note: {}", n))
        ));
    }
    let strat = d.repair_strategies.join(", ");
    out.push_str(&format!(
        "     {} {}\n",
        term::dim("="),
        term::cyan(&format!("agent  kind={}  strategies=[{}]", d.kind, strat))
    ));
    if let Some(pp) = &d.preferred_patch {
        let verb = if pp.span.is_empty() {
            "insert"
        } else {
            "replace"
        };
        let shown = pp.replacement.replace('\n', "⏎");
        out.push_str(&format!(
            "       {} {}\n",
            term::cyan("patch"),
            term::dim(&format!("{} `{}` — {}", verb, shown, pp.rationale))
        ));
    }
    out
}

/// Render a fix run as a trace of laps plus the agent-era metrics footer.
pub fn fix(o: &FixOutcome) -> String {
    let mut s = format!(
        "{} {}\n\n",
        term::bold("Tach fix"),
        term::dim(&format!("· strategy {}", o.strategy))
    );
    for lap in &o.laps {
        s.push_str(&lap_block(lap));
    }
    s.push('\n');
    s.push_str(&metrics_footer(o));
    s
}

fn lap_block(l: &Lap) -> String {
    let mut s = String::new();
    let mark = if l.action.starts_with("applied") {
        term::green("✓")
    } else if l.action.starts_with("rejected") {
        term::red("✗")
    } else {
        term::bold_green("●")
    };
    let loc = l
        .targeted
        .as_ref()
        .map(|t| term::dim(&format!("  {}:{}", t.file, t.line)))
        .unwrap_or_default();
    s.push_str(&format!(
        "  {} {}  {}{}\n",
        mark,
        term::bold(&format!("lap {}", l.index)),
        l.action,
        loc
    ));
    if let Some(t) = &l.targeted {
        s.push_str(&format!(
            "          {}\n",
            term::dim(&format!("{} {}: {}", t.code, t.kind, t.message))
        ));
    }
    if let Some(p) = &l.patch {
        let shown = p.replacement.replace('\n', "⏎");
        s.push_str(&format!(
            "          {} {}   {}\n",
            term::dim("↳"),
            term::cyan(&shown),
            term::dim(&format!("scope {}", p.touches.join(", ")))
        ));
    }
    if let Some(v) = &l.verdict {
        if v.accepted {
            s.push_str(&format!(
                "          {} {} impacted tests pass · {} errors left\n",
                term::green("proof"),
                v.tests_run,
                l.errors_remaining
            ));
        } else {
            s.push_str(&format!(
                "          {} {}\n",
                term::red("rejected"),
                v.rejections.join("; ")
            ));
        }
    }
    s
}

fn metrics_footer(o: &FixOutcome) -> String {
    let m = &o.metrics;
    let mut s = format!(
        "  {} {} in {} laps · {} patches · {} tests run · {} regressions · {}\n",
        dot(&o.status),
        term::bold(&o.status),
        m.laps,
        m.patches_applied,
        m.tests_run,
        m.regressions,
        fmt_duration(m.us)
    );
    s.push_str(&format!(
        "    {}\n",
        term::dim(&format!(
            "{} tests passing · {} errors remaining",
            o.final_tests_passed, o.final_errors
        ))
    ));
    s
}

/// Render a speculative race across strategies.
pub fn race(o: &RaceOutcome) -> String {
    let mut s = format!(
        "{} {}\n\n",
        term::bold("Tach race"),
        term::dim(&format!("· {} branches, isolated", o.branches.len()))
    );
    for (i, b) in o.branches.iter().enumerate() {
        let win = if Some(i) == o.winner {
            term::bold_green("   ← winner")
        } else {
            String::new()
        };
        s.push_str(&format!(
            "  {} {:<9} {:<9} · {} laps · diff {:>3} · {}{}\n",
            dot(&b.status),
            term::bold(&b.strategy),
            b.status,
            b.metrics.laps,
            b.metrics.diff_chars,
            fmt_duration(b.metrics.us),
            win
        ));
    }
    s.push('\n');
    match o.winner {
        Some(i) => s.push_str(&format!(
            "  {} winner: {} {}\n",
            term::bold_green("●"),
            term::bold(&o.branches[i].strategy),
            term::dim("— green with the smallest verified diff")
        )),
        None => s.push_str(&format!(
            "  {} no branch reached green\n",
            term::bold_red("●")
        )),
    }
    s
}

/// Render a test report.
pub fn tests(r: &TestReport) -> String {
    let mut s = String::new();
    for o in &r.outcomes {
        if o.passed {
            s.push_str(&format!("  {} {}\n", term::green("✓"), o.name));
        } else {
            s.push_str(&format!(
                "  {} {}\n      {}\n",
                term::red("✗"),
                term::bold(&o.name),
                term::dim(o.reason.as_deref().unwrap_or(""))
            ));
        }
    }
    if !r.outcomes.is_empty() {
        s.push('\n');
    }
    let lead = if r.all_green() {
        term::bold_green("●")
    } else {
        term::bold_red("●")
    };
    s.push_str(&format!(
        "  {} {} passed, {} failed\n",
        lead, r.passed, r.failed
    ));
    s
}

/// Render the effect surface of every function (`tach audit`).
pub fn audit(program: &Program) -> String {
    let mut s = format!("{}\n\n", term::bold("Tach audit · effect surface"));
    let mut any = false;
    for u in &program.units {
        for it in &u.module.items {
            if let Item::Fn(f) = it {
                any = true;
                let effects: Vec<String> = f
                    .effects
                    .as_ref()
                    .map(|c| c.effects.iter().map(|e| e.name.clone()).collect())
                    .unwrap_or_default();
                s.push_str(&format!(
                    "  {} {}\n",
                    term::bold(&f.name),
                    term::dim(&check::signature_string(f))
                ));
                if effects.is_empty() {
                    s.push_str(&format!(
                        "      {}\n",
                        term::dim("pure — no declared effects")
                    ));
                } else {
                    for e in &effects {
                        let desc = builtins::effect_description(e);
                        let label = if builtins::is_sensitive(e) {
                            term::yellow(&format!("⚠ {}", e))
                        } else {
                            term::green(e)
                        };
                        s.push_str(&format!("      {} {}\n", label, term::dim(desc)));
                    }
                }
            }
        }
    }
    if !any {
        s.push_str(&format!("  {}\n", term::dim("no functions found")));
    }
    s
}

/// A run's approval gates: status, id, the tool, and the human-readable intent.
pub fn approvals(items: &[crate::store::Approval]) -> String {
    let mut out = format!("{}\n\n", term::bold("approvals"));
    for a in items {
        let dot = match a.status.as_str() {
            "granted" => term::bold_green("●"),
            "denied" => term::bold_red("●"),
            _ => term::bold_yellow("●"),
        };
        out.push_str(&format!(
            "  {} {:<9} {}  {}\n",
            dot,
            a.status,
            term::bold(&a.id),
            term::dim(&a.tool)
        ));
        if !a.summary.is_empty() {
            out.push_str(&format!("       {}\n", term::dim(&a.summary)));
        }
        if let Some(n) = &a.note {
            out.push_str(&format!("       {}\n", term::dim(&format!("note: {}", n))));
        }
    }
    out
}

/// A run's effect receipts — the durable proof that each effectful action ran once.
pub fn receipts(items: &[crate::store::Receipt]) -> String {
    let mut out = format!("{}\n\n", term::bold("receipts"));
    for r in items {
        out.push_str(&format!(
            "  {} {}  {}  {}\n",
            term::bold_green("●"),
            term::bold(&r.receipt_id),
            term::dim(&r.tool),
            term::dim(&r.action_id)
        ));
        out.push_str(&format!(
            "       {}\n",
            term::dim(&format!("key {}", r.idempotency_key))
        ));
    }
    out
}

/// A single receipt in full: its identity, the tool, and the input/output it proves.
pub fn receipt(r: &crate::store::Receipt) -> String {
    let mut out = format!("{} {}\n", term::bold_green("●"), term::bold(&r.receipt_id));
    let row = |k: &str, v: String| format!("  {:<14} {}\n", k, v);
    out.push_str(&row("tool", r.tool.clone()));
    out.push_str(&row("action", r.action_id.clone()));
    out.push_str(&row("idempotency", r.idempotency_key.clone()));
    out.push_str(&row(
        "input",
        serde_json::to_string(&r.input).unwrap_or_default(),
    ));
    out.push_str(&row(
        "output",
        serde_json::to_string(&r.output).unwrap_or_default(),
    ));
    out
}
