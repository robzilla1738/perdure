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
